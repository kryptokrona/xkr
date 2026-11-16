//! Offline/at-end-of-sync maintenance: table repacking, file compaction and re-compression
//! with a new zstd dictionary (`docs/storage.md` "Maintenance").
//!
//! **Why repack.** redb splits a full leaf half/half. Tables whose inserts land in the
//! middle of the key space — `outputs` (one append point per amount) and the hash-keyed
//! tables (`key_images`, `tx_hash_to_loc`, `block_hash_to_height`, `payment_ids`) — end up
//! with pages 50–70 % full. Copying such a table in key order into a fresh table appends at
//! the right edge of the tree, where redb packs leaves full. The copy moves `REPACK_STEP` rows
//! per durable write transaction and deletes them from the source as it goes (no doubled disk
//! space); `Store::open` finishes an interrupted repack (`<name>.repack` exists).
//!
//! **Why compact.** redb never shrinks the file by itself; `Database::compact` moves pages
//! down and truncates the freed tail.

use std::ops::Bound;
use std::time::Duration;

use redb::{
    CompactionError, Database, ReadableDatabase, ReadableTable, TableDefinition, TableHandle,
};

use crate::{
    Result,
    compress::{dictionary_id, framed_dictionary_id, is_raw_frame},
    ops::{
        Reader,
        chunk::{compressor_from_blobs, delete_parts, read_parts, write_parts},
    },
    schema::{
        ALT_BLOCK_HEIGHTS, ALT_BLOCKS, BLOCK_HASH_TO_HEIGHT, BODY_PARTS, CHUNK_BLOCKS, KEY_IMAGES,
        META, META_BLOBS, OUTPUTS, OutputRow, PAYMENT_IDS, TIMESTAMPS, TX_HASH_TO_LOC, TxLocRow,
        UNLOCK_OVERFLOW, meta,
    },
    stats::{collect, file_stats},
};

/// Rows moved per write transaction while repacking.
pub const REPACK_STEP: usize = 1_000_000;

/// When maintenance repacks and compacts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaintainOptions {
    /// Repack a table whose pages are less full than this (stored / page bytes).
    pub repack_below_fill: f64,
    /// ... and that occupies at least this many bytes.
    pub repack_min_bytes: u64,
    /// Compact when free and fragmented space is at least this fraction of the file.
    pub compact_min_free_fraction: f64,
    /// Repack every repackable table and compact regardless of the thresholds.
    pub force: bool,
    /// Run the compaction step at all.
    pub compact: bool,
}

impl Default for MaintainOptions {
    fn default() -> Self {
        Self {
            repack_below_fill: 0.85,
            repack_min_bytes: 32 << 20,
            compact_min_free_fraction: 0.10,
            force: false,
            compact: true,
        }
    }
}

/// What maintenance did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaintainReport {
    /// `(table, rows moved, fill before)` of every repacked table.
    pub repacked: Vec<(&'static str, u64, f64)>,
    /// Whether the file was compacted.
    pub compacted: bool,
    pub file_bytes_before: u64,
    pub file_bytes_after: u64,
}

/// Defines `fn $fname(db, step) -> rows moved` that repacks `$def` through `$tmp`.
macro_rules! repack_fn {
    ($fname:ident, $def:expr, $tmp:literal, $k:ty, $v:ty) => {
        fn $fname(db: &Database, step: usize) -> Result<u64> {
            const TMP: TableDefinition<$k, $v> = TableDefinition::new($tmp);
            let mut moved = 0_u64;
            loop {
                let txn = db.begin_write()?;
                let done = {
                    let mut src = txn.open_table($def)?;
                    let mut dst = txn.open_table(TMP)?;
                    let mut batch: Vec<($k, $v)> = Vec::with_capacity(step.min(1 << 20));
                    for row in src.iter()? {
                        let (k, v) = row?;
                        batch.push((k.value(), v.value()));
                        if batch.len() >= step {
                            break;
                        }
                    }
                    if let Some(&(last, _)) = batch.last() {
                        let mut cursor = dst.upper_bound_mut(Bound::<$k>::Unbounded)?;
                        for (k, v) in &batch {
                            cursor.insert_before(*k, *v)?;
                        }
                        cursor.close()?;
                        src.retain_in(..=last, |_, _| false)?;
                    }
                    moved += batch.len() as u64;
                    batch.len() < step
                };
                if done {
                    txn.delete_table($def)?;
                    txn.rename_table(TMP, $def)?;
                    txn.commit()?;
                    return Ok(moved);
                }
                txn.commit()?;
            }
        }
    };
}

repack_fn!(
    repack_outputs,
    OUTPUTS,
    "outputs.repack",
    (u64, u32),
    OutputRow
);
repack_fn!(
    repack_key_images,
    KEY_IMAGES,
    "key_images.repack",
    [u8; 32],
    ()
);
repack_fn!(
    repack_tx_locs,
    TX_HASH_TO_LOC,
    "tx_hash_to_loc.repack",
    [u8; 32],
    TxLocRow
);
repack_fn!(
    repack_block_hashes,
    BLOCK_HASH_TO_HEIGHT,
    "block_hash_to_height.repack",
    [u8; 32],
    u32
);
repack_fn!(
    repack_payment_ids,
    PAYMENT_IDS,
    "payment_ids.repack",
    ([u8; 32], u32, u32),
    ()
);
repack_fn!(
    repack_timestamps,
    TIMESTAMPS,
    "timestamps.repack",
    (u64, u32),
    ()
);
repack_fn!(
    repack_unlock_overflow,
    UNLOCK_OVERFLOW,
    "unlock_overflow.repack",
    (u64, u32),
    u64
);

type RepackFn = fn(&Database, usize) -> Result<u64>;

/// `(table, temporary table, repack function)`.
const REPACKABLE: &[(&str, &str, RepackFn)] = &[
    ("outputs", "outputs.repack", repack_outputs),
    ("key_images", "key_images.repack", repack_key_images),
    ("tx_hash_to_loc", "tx_hash_to_loc.repack", repack_tx_locs),
    (
        "block_hash_to_height",
        "block_hash_to_height.repack",
        repack_block_hashes,
    ),
    ("payment_ids", "payment_ids.repack", repack_payment_ids),
    ("timestamps", "timestamps.repack", repack_timestamps),
    (
        "unlock_overflow",
        "unlock_overflow.repack",
        repack_unlock_overflow,
    ),
];

fn table_names(db: &Database) -> Result<Vec<String>> {
    let txn = db.begin_read()?;
    let names = txn.list_tables()?.map(|h| h.name().to_owned()).collect();
    Ok(names)
}

/// Finishes repacks that were interrupted (called by `Store::open`); `on_resume` gets the
/// table name before each one.
pub(crate) fn resume_repacks(db: &Database, on_resume: &dyn Fn(&str)) -> Result<()> {
    let names = table_names(db)?;
    for (name, tmp, run) in REPACKABLE {
        if names.iter().any(|n| n == tmp) {
            on_resume(name);
            log(&format!("finishing the interrupted repack of {name}"));
            run(db, REPACK_STEP)?;
        }
    }
    Ok(())
}

/// The storage crate has no logger; maintenance messages go to stderr.
fn log(msg: &str) {
    eprintln!("xkr-storage: {msg}");
}

/// Repacks tables below the fill threshold, then compacts the file if enough of it is free.
/// Needs exclusive access (`Store::maintain` holds the database lock).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn maintain(
    db: &mut Database,
    path: &std::path::Path,
    opts: &MaintainOptions,
    progress: &mut dyn FnMut(&str),
) -> Result<MaintainReport> {
    let mut report = MaintainReport {
        file_bytes_before: std::fs::metadata(path)?.len(),
        ..MaintainReport::default()
    };
    let (tables, _) = collect(&*db)?;
    for (name, _, run) in REPACKABLE {
        let Some(t) = tables.iter().find(|t| t.name == *name) else {
            continue;
        };
        let fill = t.fill();
        let wanted = opts.force
            || (t.total_bytes() >= opts.repack_min_bytes && fill < opts.repack_below_fill);
        if !wanted || t.entries == 0 {
            continue;
        }
        progress(&format!(
            "repacking {name}: {} rows, {:.1} MB, {:.0} % full",
            t.entries,
            t.total_bytes() as f64 / 1e6,
            fill * 100.0
        ));
        let moved = run(db, REPACK_STEP)?;
        report.repacked.push((*name, moved, fill));
    }

    if opts.compact {
        let fs = file_stats(db)?;
        let file = std::fs::metadata(path)?.len().max(1);
        let free = fs.fragmented_bytes as f64 / file as f64;
        if opts.force || free >= opts.compact_min_free_fraction || !report.repacked.is_empty() {
            progress(&format!(
                "compacting: {:.1} MB file, {:.0} % free or fragmented",
                file as f64 / 1e6,
                free * 100.0
            ));
            report.compacted = compact(db)?;
        }
    }
    report.file_bytes_after = std::fs::metadata(path)?.len();
    Ok(report)
}

/// `Database::compact`, waiting (up to 60 s) for read transactions that are still open.
pub(crate) fn compact(db: &mut Database) -> Result<bool> {
    let mut waited = 0;
    loop {
        match db.compact() {
            Ok(done) => return Ok(done),
            Err(CompactionError::TransactionInProgress) if waited < 600 => {
                std::thread::sleep(Duration::from_millis(100));
                waited += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Installs `dictionary` for new chunks and re-compresses every sealed chunk with it.
/// Resumable: while it runs the previous dictionary stays readable as
/// `META_BLOBS["zstd_dictionary_prev"]`; an unfinished run is completed by running it again with
/// the same dictionary (or the previous one), any other dictionary is refused until then.
/// Alternative blocks (compressed with the old dictionary, and dropped on every start anyway)
/// are deleted. Returns the chunks rewritten.
pub(crate) fn recompress(
    db: &Database,
    dictionary: &[u8],
    progress: &mut dyn FnMut(u64, u64),
) -> Result<u64> {
    let new_id = dictionary_id(dictionary);
    let txn = db.begin_write()?;
    {
        let mut blobs = txn.open_table(META_BLOBS)?;
        let current = blobs
            .get(meta::ZSTD_DICTIONARY)?
            .map(|g| g.value().to_vec());
        let prev_id = blobs
            .get(meta::ZSTD_DICTIONARY_PREV)?
            .map(|g| dictionary_id(g.value()));
        if let Some(cur) = current
            && dictionary_id(&cur) != new_id
        {
            // Chunks are readable with the current and the previous dictionary only. While an
            // earlier run has not finished, a third dictionary would leave some chunks without
            // theirs; switching back to the previous one keeps both readable.
            if prev_id.is_some_and(|p| p != new_id) {
                return Err(crate::Error::Unsupported(
                    "an earlier re-compression has not finished; run it again with the same \
                     dictionary first",
                ));
            }
            blobs.insert(meta::ZSTD_DICTIONARY_PREV, cur.as_slice())?;
        }
        blobs.insert(meta::ZSTD_DICTIONARY, dictionary)?;
    }
    txn.commit()?;

    let (sealed, mut comp) = {
        let txn = db.begin_read()?;
        let sealed = txn
            .open_table(META)?
            .get(meta::SEALED_BELOW)?
            .map_or(0, |g| g.value());
        (sealed, compressor_from_blobs(&txn.open_table(META_BLOBS)?)?)
    };
    let chunks = sealed / CHUNK_BLOCKS;
    let mut rewritten = 0_u64;
    let mut chunk = 0;
    while chunk < chunks {
        let end = (chunk + 64).min(chunks);
        let txn = db.begin_write()?;
        {
            let mut body = txn.open_table(BODY_PARTS)?;
            for c in chunk..end {
                let framed =
                    read_parts(&body, c)?.ok_or(crate::Error::Corrupt("sealed chunk missing"))?;
                if framed_dictionary_id(&framed) == Some(new_id) || is_raw_frame(&framed) {
                    continue;
                }
                let raw = comp.decompress(&framed)?;
                let packed = comp.compress_chunk(&raw)?;
                delete_parts(&mut body, c)?;
                write_parts(&mut body, c, &packed)?;
                rewritten += 1;
            }
        }
        txn.commit()?;
        chunk = end;
        progress(chunk, chunks);
    }

    let txn = db.begin_write()?;
    {
        txn.open_table(META_BLOBS)?
            .remove(meta::ZSTD_DICTIONARY_PREV)?;
        txn.open_table(ALT_BLOCKS)?.retain(|_, _| false)?;
        txn.open_table(ALT_BLOCK_HEIGHTS)?.retain(|_, ()| false)?;
    }
    txn.commit()?;
    Ok(rewritten)
}

/// Up to `n` encoded block records spread evenly over the chain, as dictionary training
/// samples (they are what chunks are made of). Each is cut to `max_len` bytes.
pub(crate) fn training_samples(db: &Database, n: usize, max_len: usize) -> Result<Vec<Vec<u8>>> {
    let txn = db.begin_read()?;
    let mut comp = compressor_from_blobs(&txn.open_table(META_BLOBS)?)?;
    let reader = Reader::new(&txn)?;
    let Some(top) = reader.top_height()? else {
        return Ok(Vec::new());
    };
    let blocks = top + 1;
    let n = (n as u64).min(blocks);
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let height = blocks * i / n.max(1);
        if let Some(rec) = reader.block_record(height, &mut comp)? {
            let mut bytes = rec.encode();
            bytes.truncate(max_len);
            out.push(bytes);
        }
    }
    Ok(out)
}
