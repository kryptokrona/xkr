//! Opening the database, dictionaries, statistics and maintenance.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use parking_lot::{MappedRwLockReadGuard, RwLock, RwLockReadGuard};
use redb::{Database, ReadableDatabase, ReadableTable, WriteTransaction};

use crate::{
    Error, Result,
    compress::Compressor,
    maintain::{self, MaintainOptions, MaintainReport},
    ops::{self, Reader, chunk::compressor_from_blobs},
    pruning::{PruneMode, PruneReport, PruningSeed, STRIPE_SPAN},
    schema::{
        ALT_BLOCK_HEIGHTS, ALT_BLOCKS, BLOCK_HASH_TO_HEIGHT, BLOCK_INFOS, BODY_PARTS, HOT_BLOCKS,
        HOT_SIGS, KEY_IMAGES, META, META_BLOBS, OUTPUT_COUNTS, OUTPUTS, PAYMENT_IDS,
        SCHEMA_VERSION, SIG_PARTS, TIMESTAMPS, TX_HASH_TO_LOC, UNLOCK_OVERFLOW, meta,
    },
    stats::{self, DbStats},
};

/// Blocks [`Store::prune`] advances the prune height by per write transaction (8 stripe spans:
/// with 4 stripes about 384 chunks, ~110 MB of mainnet signature parts).
pub const PRUNE_STEP_BLOCKS: u64 = 8 * STRIPE_SPAN;

/// What [`Store::open_observed`] reports while it opens the database.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenEvent {
    /// redb checks and repairs the file after an unclean stop; `progress` estimates how far,
    /// 0.0 to 1.0. Reported at least once when a check runs.
    Repair { progress: f64 },
    /// An interrupted repack of `table` is being finished.
    ResumingRepack { table: String },
}

/// The chain database. Holds no other state: transactions are opened by the caller.
///
/// The redb handle sits behind a lock that ordinary use only takes for reading (to begin a
/// transaction; transactions do not borrow it). [`Store::maintain`] takes it exclusively:
/// meanwhile [`Store::reader`] fails fast with [`Error::Busy`] instead of blocking.
///
/// [`Store::close`] closes the file while other handles to the store may still exist (tasks
/// that hold the chain at exit): from then on every call returns [`Error::Closed`].
pub struct Store {
    /// `None` once [`Store::close`] ran.
    db: RwLock<Option<Database>>,
    path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (or creates) the database at `path`, creates missing tables, checks the schema
    /// version and finishes an interrupted repack.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        Self::init(Database::create(&path)?, path, &|_| {})
    }

    /// Like [`Store::open`] with an explicit redb page-cache size in bytes (redb's default is
    /// 1 GiB; a desktop sidecar may want less).
    pub fn open_with_cache(path: impl AsRef<Path>, cache_bytes: usize) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        Self::init(
            Database::builder()
                .set_cache_size(cache_bytes)
                .create(&path)?,
            path,
            &|_| {},
        )
    }

    /// Like [`Store::open_with_cache`], reporting the check redb runs after an unclean stop and
    /// a resumed repack to `observer` ([`OpenEvent`]). `observer` returning `false` during the
    /// check stops it: the open fails with redb's `RepairAborted` and the file stays as it was
    /// (the check runs again at the next open).
    pub fn open_observed(
        path: impl AsRef<Path>,
        cache_bytes: usize,
        observer: impl Fn(&OpenEvent) -> bool + 'static,
    ) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let observer: Rc<dyn Fn(&OpenEvent) -> bool> = Rc::new(observer);
        let on_repair = observer.clone();
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .set_repair_callback(move |session: &mut redb::RepairSession| {
                let event = OpenEvent::Repair {
                    progress: session.progress(),
                };
                if !on_repair(&event) {
                    session.abort();
                }
            })
            .create(&path)?;
        Self::init(db, path, &|table| {
            let _ = observer(&OpenEvent::ResumingRepack {
                table: table.to_owned(),
            });
        })
    }

    fn init(db: Database, path: PathBuf, on_resume: &dyn Fn(&str)) -> Result<Self> {
        let txn = db.begin_write()?;
        // The version check comes first: a v1 database has tables of the same names with
        // other types, which `create_tables` would refuse with a less helpful error.
        {
            let mut m = txn.open_table(META)?;
            let found = m.get(meta::SCHEMA_VERSION)?.map(|g| g.value());
            match found {
                None => {
                    m.insert(meta::SCHEMA_VERSION, SCHEMA_VERSION)?;
                }
                Some(v) if v == SCHEMA_VERSION => {}
                Some(v) => {
                    return Err(Error::SchemaVersion {
                        found: v,
                        expected: SCHEMA_VERSION,
                    });
                }
            }
        }
        create_tables(&txn)?;
        txn.commit()?;
        maintain::resume_repacks(&db, on_resume)?;
        Ok(Self {
            db: RwLock::new(Some(db)),
            path,
        })
    }

    /// The open database for a short read lock; [`Error::Closed`] after [`Store::close`].
    fn db(&self) -> Result<MappedRwLockReadGuard<'_, Database>> {
        RwLockReadGuard::try_map(self.db.read(), Option::as_ref).map_err(|_| Error::Closed)
    }

    /// Like [`Store::db`], but [`Error::Busy`] instead of waiting while maintenance runs.
    fn try_db(&self) -> Result<MappedRwLockReadGuard<'_, Database>> {
        let guard = self.db.try_read().ok_or(Error::Busy)?;
        RwLockReadGuard::try_map(guard, Option::as_ref).map_err(|_| Error::Closed)
    }

    /// Closes the database file: redb records its allocator state, makes every commit durable
    /// and writes its clean-shutdown header, so the next open does not check the whole file.
    /// Works while other handles to this store still exist; afterwards every call on it
    /// returns [`Error::Closed`] (read snapshots that are still open fail their reads with
    /// it). Closing again does nothing. Waits for maintenance to finish. If another thread
    /// still has a write transaction open, redb closes the file when that transaction ends
    /// and this returns [`Error::CloseDeferred`].
    pub fn close(&self) -> Result<()> {
        let Some(db) = self.db.write().take() else {
            return Ok(());
        };
        match db.close() {
            Ok(()) => Ok(()),
            Err(redb::DatabaseError::TransactionInProgress) => Err(Error::CloseDeferred),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether [`Store::close`] ran.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.db.read().is_none()
    }

    /// Path of the database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A write transaction (redb allows one at a time; it blocks until available).
    pub fn begin_write(&self) -> Result<WriteTransaction> {
        Ok(self.db()?.begin_write()?)
    }

    /// A consistent read snapshot of all tables. [`Error::Busy`] during maintenance.
    pub fn reader(&self) -> Result<Reader> {
        let db = self.try_db()?;
        let txn = db.begin_read()?;
        drop(db);
        Reader::new(&txn)
    }

    /// A [`Compressor`] using the dictionaries stored in the database, if any.
    pub fn compressor(&self) -> Result<Compressor> {
        let db = self.try_db()?;
        let txn = db.begin_read()?;
        drop(db);
        compressor_from_blobs(&txn.open_table(META_BLOBS)?)
    }

    /// The zstd dictionary new chunks are written with.
    pub fn dictionary(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db()?.begin_read()?;
        let blobs = txn.open_table(META_BLOBS)?;
        Ok(blobs
            .get(meta::ZSTD_DICTIONARY)?
            .map(|g| g.value().to_vec()))
    }

    /// Installs a zstd dictionary for new chunks. Only allowed before the first chunk is
    /// sealed; use [`Store::recompress`] on a database that has chunks.
    pub fn set_dictionary(&self, dictionary: &[u8]) -> Result<()> {
        let txn = self.begin_write()?;
        {
            let m = txn.open_table(META)?;
            if m.get(meta::SEALED_BELOW)?.is_some_and(|g| g.value() > 0) {
                return Err(Error::Unsupported(
                    "the database already has sealed chunks: re-compress instead (xkrd db train-dict)",
                ));
            }
            let mut blobs = txn.open_table(META_BLOBS)?;
            blobs.insert(meta::ZSTD_DICTIONARY, dictionary)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Installs `dictionary` if the database has none and no sealed chunk yet (fresh sync
    /// with the dictionary shipped in the binary). Returns whether it was installed.
    pub fn install_dictionary_if_fresh(&self, dictionary: &[u8]) -> Result<bool> {
        if dictionary.is_empty() {
            return Ok(false);
        }
        match self.dictionary()? {
            Some(_) => Ok(false),
            None => match self.set_dictionary(dictionary) {
                Ok(()) => Ok(true),
                Err(Error::Unsupported(_)) => Ok(false),
                Err(e) => Err(e),
            },
        }
    }

    /// Sets how many blocks behind the tip a chunk is sealed (default
    /// [`crate::schema::DEFAULT_SEAL_LAG`]; tests use small values).
    pub fn set_seal_lag(&self, lag: u64) -> Result<()> {
        let txn = self.begin_write()?;
        txn.open_table(META)?.insert(meta::SEAL_LAG, lag)?;
        txn.commit()?;
        Ok(())
    }

    /// Stores the stripe seed of a pruning node without pruning anything yet
    /// ([`ops::prune_stripe`] with height 0; the same seed checks apply).
    pub fn set_pruning_seed(&self, seed: PruningSeed) -> Result<()> {
        let txn = self.begin_write()?;
        ops::prune_stripe(&txn, seed, 0)?;
        txn.commit()?;
        Ok(())
    }

    /// Prunes up to `below_height` ([`ops::prune_stripe`] or [`ops::prune_all_below`]) in steps
    /// of [`PRUNE_STEP_BLOCKS`], one durable write transaction each, so pruning a full mainnet
    /// database never builds one huge transaction. `progress` gets the running total after
    /// every step. Switching a stripe-pruned database to [`PruneMode::All`] drops the kept
    /// stripe below the old prune height in the first step. Call it with no write transaction
    /// of the calling thread open.
    pub fn prune(
        &self,
        mode: PruneMode,
        below_height: u64,
        progress: &mut dyn FnMut(&PruneReport),
    ) -> Result<PruneReport> {
        let mut total = PruneReport {
            state: self.reader()?.prune_state(),
            ..PruneReport::default()
        };
        loop {
            let before = total.state;
            let step = below_height.min(before.pruned_below.saturating_add(PRUNE_STEP_BLOCKS));
            let txn = self.begin_write()?;
            let report = match mode {
                PruneMode::Stripe(seed) => ops::prune_stripe(&txn, seed, step)?,
                PruneMode::All => ops::prune_all_below(&txn, step)?,
            };
            txn.commit()?;
            total.absorb(&report);
            progress(&total);
            if report.state == before || step >= below_height {
                return Ok(total);
            }
        }
    }

    /// Per-table and file statistics (opens and aborts a write transaction).
    pub fn stats(&self) -> Result<DbStats> {
        let db = self.db()?;
        let (tables, chain) = stats::collect(&*db)?;
        let file = Some(stats::file_stats(&db)?);
        Ok(DbStats {
            file_bytes: std::fs::metadata(&self.path)?.len(),
            read_only: false,
            tables,
            file,
            chain,
        })
    }

    /// Repacks under-filled tables and compacts the file (`MaintainOptions` thresholds).
    /// Exclusive: readers get [`Error::Busy`] meanwhile and writers wait. Call it with no
    /// write transaction of the calling thread open.
    pub fn maintain(
        &self,
        opts: &MaintainOptions,
        progress: &mut dyn FnMut(&str),
    ) -> Result<MaintainReport> {
        let mut guard = self.db.write();
        let db = guard.as_mut().ok_or(Error::Closed)?;
        maintain::maintain(db, &self.path, opts, progress)
    }

    /// Re-compresses every sealed chunk with `dictionary` and makes it the dictionary for new
    /// chunks. `progress(done, total)` is called per batch of chunks.
    pub fn recompress(&self, dictionary: &[u8], progress: &mut dyn FnMut(u64, u64)) -> Result<u64> {
        let guard = self.db.write();
        let db = guard.as_ref().ok_or(Error::Closed)?;
        maintain::recompress(db, dictionary, progress)
    }

    /// Up to `n` block records spread over the chain, as zstd dictionary training samples.
    pub fn training_samples(&self, n: usize, max_len: usize) -> Result<Vec<Vec<u8>>> {
        let db = self.db()?;
        maintain::training_samples(&db, n, max_len)
    }
}

fn create_tables(txn: &WriteTransaction) -> Result<()> {
    // `open_table` creates a table that does not exist yet.
    txn.open_table(BLOCK_INFOS)?;
    txn.open_table(BLOCK_HASH_TO_HEIGHT)?;
    txn.open_table(HOT_BLOCKS)?;
    txn.open_table(HOT_SIGS)?;
    txn.open_table(BODY_PARTS)?;
    txn.open_table(SIG_PARTS)?;
    txn.open_table(TX_HASH_TO_LOC)?;
    txn.open_table(KEY_IMAGES)?;
    txn.open_table(OUTPUTS)?;
    txn.open_table(UNLOCK_OVERFLOW)?;
    txn.open_table(OUTPUT_COUNTS)?;
    txn.open_table(PAYMENT_IDS)?;
    txn.open_table(TIMESTAMPS)?;
    txn.open_table(ALT_BLOCKS)?;
    txn.open_table(ALT_BLOCK_HEIGHTS)?;
    txn.open_table(META)?;
    txn.open_table(META_BLOBS)?;
    Ok(())
}
