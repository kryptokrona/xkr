//! The chain: storage, the in-memory tip window, alternative blocks and reorgs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock, RwLockReadGuard};
use rayon::prelude::*;
use redb::Durability;
use tokio::sync::watch;
use xkr_consensus::reward::{full_reward_zone, max_block_cumulative_size, median};
use xkr_consensus::{
    BlockError, ConsensusError, Network, NetworkParams, block_major_version_for_height,
    check_proof_of_work_with_hash, difficulty_for_next_block, genesis_block,
};
use xkr_crypto::cn_fast_hash;
use xkr_p2p_legacy::sync::BlockOutcome;
use xkr_serialization::{
    block_hash, block_long_hash, decode_block, decode_transaction, encode_block,
    encode_transaction, transaction_hash_from_blob,
};
use xkr_storage::ops::{
    pop_blocks, prune_alt_blocks_below, push_blocks, put_alt_block, remove_alt_block,
};
use xkr_storage::schema::AltBlockHeader;
use xkr_storage::{
    BlockInfo, Compressor, PRUNE_STEP_BLOCKS, PruneMode, PruneReport, Reader, Store, VerifiedBlock,
    VerifiedTx,
};
use xkr_txpool::{PoolError, TxPool, TxSource};
use xkr_types::{Hash, KeyImage, RawBlock, TxInput, payment_id};

use crate::checkpoints::Checkpoints;
use crate::error::{ChainError, RejectReason};
use crate::headers::{
    DEFAULT_HEADER_CACHE, HeaderCache, context_data, context_from_storage, window_start,
};
use crate::prepare::{
    DecodedBlock, PowMode, PrepareError, PreparedBlock, check_raw_sizes, decode_raw, prepare_block,
};
use crate::verify::{
    HeaderVerdict, Overlay, SigJob, check_header, check_header_cheap, check_inputs,
    signature_error, verify_signatures,
};
use crate::view::ChainSnapshot;

/// Heights above the tip covered by the pre-decode size check (see `Chain::raw_size_limit`).
const SIZE_CHECK_HEIGHT_SLACK: u64 = 1_000_000;

/// Unix time in seconds.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Chain settings.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    /// Which network.
    pub network: Network,
    /// Node policy (`docs/node-architecture.md`, "Alternative blocks and reorgs"): reorgs deeper
    /// than this are refused and logged.
    pub max_reorg_depth: u64,
    /// Reorgs deeper than this are logged as warnings.
    pub warn_reorg_depth: u64,
    /// `--fast-sync`: skip proof of work up to the last checkpoint for blocks of requested sync
    /// batches ([`Chain::add_blocks`] with expected ids); [`Chain::add_block`] always checks it.
    pub fast_sync: bool,
    /// Blocks kept in the in-memory header window.
    pub header_cache: usize,
    /// During sync, batches are committed without fsync (`Durability::None`); at least one
    /// durable commit happens per interval, and always for live blocks and reorgs.
    pub durable_interval: Duration,
    /// Checkpoints (defaults to the compiled-in list of the network).
    pub checkpoints: Checkpoints,
    /// Height of fork 13 (block major version 6, `docs/forks.md`); defaults to the network's
    /// table value (`None` = not scheduled). `xkrd --fork13-height` sets it on testnet.
    pub fork13_height: Option<u32>,
    /// Height of fork 14 (block major version 7, transaction version 2, `docs/tx-v2.md`);
    /// defaults to the network's table value (`None` = not scheduled). Testnet only, like
    /// `fork13_height`.
    pub fork14_height: Option<u32>,
    /// Fork-14 parameters (grace period for version 1, ring sizes); defaults to the network's.
    pub fork14: xkr_consensus::Fork14Params,
}

impl ChainConfig {
    /// Defaults for `network`.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            max_reorg_depth: 1_000,
            warn_reorg_depth: 60,
            fast_sync: false,
            header_cache: DEFAULT_HEADER_CACHE,
            durable_interval: Duration::from_secs(5),
            checkpoints: Checkpoints::for_network(network),
            fork13_height: network.params().fork13_height,
            fork14_height: network.params().fork14_height,
            fork14: network.params().fork14,
        }
    }
}

/// The top of the main chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TipInfo {
    /// Index of the top block (`getTopBlockIndex`); the C++ "height" is this + 1.
    pub height: u64,
    /// Top block id.
    pub hash: Hash,
    /// Top block timestamp.
    pub timestamp: u64,
    /// Top block difficulty.
    pub difficulty: u64,
    /// Cumulative difficulty.
    pub cumulative_difficulty: u128,
    /// Coins generated up to and including the top block.
    pub already_generated_coins: u64,
    /// Transactions (including miner transactions) up to and including the top block.
    pub cumulative_tx_count: u64,
}

impl TipInfo {
    fn from_info(height: u64, info: &BlockInfo) -> Self {
        Self {
            height,
            hash: Hash(info.hash),
            timestamp: info.timestamp,
            difficulty: info.difficulty,
            cumulative_difficulty: info.cumulative_difficulty(),
            already_generated_coins: info.already_generated_coins,
            cumulative_tx_count: info.cumulative_tx_count,
        }
    }
}

/// What happened to a single block (C++ `AddBlockErrorCode` / `AddBlockErrorCondition`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddBlockOutcome {
    /// Appended to the main chain.
    AddedToMain,
    /// Stored as an alternative block.
    AddedToAlternative,
    /// Stored as an alternative block that made its branch the main chain.
    AddedAndSwitched,
    /// Already known (main or alternative).
    AlreadyExists,
    /// Parent unknown (`REJECTED_AS_ORPHANED`).
    Orphan,
    /// Refused.
    Invalid(RejectReason),
}

impl AddBlockOutcome {
    /// The `xkr-p2p-legacy` sync outcome for this result.
    #[must_use]
    pub fn sync_outcome(&self) -> BlockOutcome {
        match self {
            Self::AddedToMain => BlockOutcome::AddedToMain,
            Self::AddedToAlternative => BlockOutcome::AddedToAlternative,
            Self::AddedAndSwitched => BlockOutcome::AddedAndSwitched,
            Self::AlreadyExists => BlockOutcome::AlreadyExists,
            Self::Orphan => BlockOutcome::Rejected,
            Self::Invalid(_) => BlockOutcome::Invalid,
        }
    }

    /// Whether the main chain changed.
    #[must_use]
    pub fn main_chain_changed(&self) -> bool {
        matches!(self, Self::AddedToMain | Self::AddedAndSwitched)
    }
}

/// Result of [`Chain::add_block`].
#[derive(Debug, Clone)]
pub struct AddBlockResult {
    /// Block id (zero if the block did not decode).
    pub hash: Hash,
    /// Height of the block, if it could be placed.
    pub height: Option<u64>,
    /// Outcome.
    pub outcome: AddBlockOutcome,
}

/// Kind of a batch failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchErrorKind {
    /// A block is invalid: the peer that sent it misbehaved.
    Invalid,
    /// A block's parent is unknown.
    Orphan,
}

/// The first failing block of a batch.
#[derive(Debug, Clone)]
pub struct BatchError {
    /// Index in the submitted batch.
    pub index: usize,
    /// Block id, if it decoded.
    pub hash: Option<Hash>,
    /// Kind.
    pub kind: BatchErrorKind,
    /// Why (for [`BatchErrorKind::Orphan`]: [`RejectReason::NotConnected`]).
    pub reason: RejectReason,
}

/// Result of [`Chain::add_blocks`]. Blocks before [`BatchReport::error`] are stored.
#[derive(Debug, Clone, Default)]
pub struct BatchReport {
    /// Blocks submitted.
    pub submitted: usize,
    /// Leading blocks that were already in the main chain.
    pub skipped: usize,
    /// Blocks appended to the main chain.
    pub added_main: usize,
    /// Blocks stored as alternative blocks.
    pub added_alt: usize,
    /// Whether an alternative branch became the main chain.
    pub switched: bool,
    /// First failure.
    pub error: Option<BatchError>,
    /// Main-chain tip afterwards.
    pub tip: TipInfo,
    /// Time spent in each stage.
    pub times: StageTimes,
}

/// Wall time of the stages of [`Chain::add_blocks`]. The stages after `prepare` are timed
/// for a batch that extends the main chain; they stay zero for alternative blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct StageTimes {
    /// Decoding, size and semantic checks and the proof of work (parallel).
    pub prepare: Duration,
    /// Header context checks and the key-image and ring-member lookups (serial).
    pub context: Duration,
    /// Ring signatures (parallel).
    pub signatures: Duration,
    /// Writing the blocks into the write transaction.
    pub write: Duration,
    /// Commit, tip and pool update.
    pub commit: Duration,
}

/// An alternative block (in memory; the raw block is in the `alt_blocks` table).
#[derive(Debug, Clone, Copy)]
struct AltEntry {
    height: u64,
    prev: Hash,
    info: BlockInfo,
}

/// State only the writer touches.
#[derive(Debug)]
struct Writer {
    comp: Compressor,
    cache: HeaderCache,
    last_durable: Instant,
}

/// Outcome of appending prepared blocks to the main chain.
struct MainResult {
    /// The blocks stored, in height order.
    pushed: Vec<VerifiedBlock>,
    /// First failure: index into the input and reason.
    fail: Option<(usize, Hash, RejectReason)>,
    /// Time of the context, signature, write and commit stages.
    times: StageTimes,
}

/// Chain state shared by the node: one writer at a time (sync batches, live blocks, reorgs),
/// any number of concurrent readers on MVCC snapshots.
pub struct Chain {
    store: Store,
    cfg: ChainConfig,
    params: &'static NetworkParams,
    genesis: Hash,
    pool: Arc<TxPool>,
    tip: RwLock<TipInfo>,
    /// The last tip published to [`Chain::subscribe_tip`] receivers.
    pub(crate) tip_watch: watch::Sender<TipInfo>,
    alt: RwLock<HashMap<Hash, AltEntry>>,
    writer: Mutex<Writer>,
    read_comps: Mutex<Vec<Compressor>>,
    /// Held for writing while a block commit and the matching pool update happen, and for
    /// reading while a pool transaction is validated and inserted (C++ `m_accessLock`).
    pool_gate: RwLock<()>,
    /// Fork-signal count of the readiness window, keyed by the tip it was computed for.
    pub(crate) signal_cache: Mutex<Option<crate::forks::SignalCache>>,
}

impl std::fmt::Debug for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chain")
            .field("network", &self.cfg.network)
            .field("tip", &*self.tip.read())
            .finish_non_exhaustive()
    }
}

fn commit(mut txn: redb::WriteTransaction, durable: bool) -> Result<(), ChainError> {
    txn.set_durability(if durable {
        Durability::Immediate
    } else {
        Durability::None
    })?;
    txn.commit()?;
    Ok(())
}

impl Chain {
    /// Opens the chain: inserts the genesis block into an empty store, checks the genesis of
    /// an existing one, drops stale alternative blocks (C++ keeps them only in memory) and
    /// loads the header window.
    pub fn open(store: Store, cfg: ChainConfig, pool: Arc<TxPool>) -> Result<Self, ChainError> {
        let base = cfg.network.params();
        let wanted = base
            .with_fork13_height(cfg.fork13_height)
            .with_fork14_height(cfg.fork14_height)
            .with_fork14_params(cfg.fork14);
        // A run-time fork schedule (testnet `--fork13-height`) needs parameters that live as
        // long as the chain; leaked once per opened chain.
        let params: &'static NetworkParams = if wanted == *base {
            base
        } else {
            Box::leak(Box::new(wanted))
        };
        if let Some(h) = params.fork13_height {
            tracing::info!(
                fork13_height = h,
                first_v6_block = u64::from(h) + 1,
                network = ?cfg.network,
                "fork 13 scheduled"
            );
        }
        if let Some(h) = params.fork14_height {
            tracing::info!(
                fork14_height = h,
                first_v7_block = u64::from(h) + 1,
                v1_grace_blocks = params.fork14.grace_blocks,
                network = ?cfg.network,
                "fork 14 (transaction version 2) scheduled"
            );
            if params.fork13_height.is_some_and(|f13| f13 >= h) {
                tracing::warn!("fork 14 is scheduled at or before fork 13");
            }
        }
        let gblock = genesis_block(params)?;
        let genesis = block_hash(&gblock)?;
        let mut comp = store.compressor()?;

        let top = store.reader()?.top_height()?;
        if top.is_none() {
            let coinbase = encode_transaction(&gblock.miner_tx)?;
            let reward = gblock.miner_tx.output_amount_checked().unwrap_or(0);
            let vb = VerifiedBlock {
                height: 0,
                hash: genesis,
                block_blob: encode_block(&gblock)?,
                block: gblock,
                miner_tx_hash: cn_fast_hash(&coinbase),
                difficulty: 1,
                already_generated_coins: reward,
                block_size: coinbase.len() as u64,
                txs: Vec::new(),
            };
            let txn = store.begin_write()?;
            push_blocks(&txn, &[vb], &mut comp)?;
            commit(txn, true)?;
            tracing::info!(%genesis, network = ?cfg.network, "initialised an empty chain with the genesis block");
        } else {
            let found = store
                .reader()?
                .block_hash(0)?
                .ok_or_else(|| ChainError::Corrupt("no block 0".into()))?;
            if found != genesis {
                return Err(ChainError::WrongGenesis {
                    found,
                    expected: genesis,
                });
            }
        }

        let txn = store.begin_write()?;
        prune_alt_blocks_below(&txn, u64::MAX)?;
        commit(txn, true)?;

        let reader = store.reader()?;
        let top = reader
            .top_height()?
            .ok_or_else(|| ChainError::Corrupt("empty chain after genesis".into()))?;
        let cache = HeaderCache::load(&reader, top, cfg.header_cache.max(DEFAULT_HEADER_CACHE))?;
        let tip = TipInfo::from_info(top, cache.top());
        for (height, expected) in cfg.checkpoints.iter().take_while(|(h, _)| *h <= top) {
            if let Some(found) = reader.block_hash(height)?
                && found != expected
            {
                tracing::error!(
                    height,
                    %found,
                    %expected,
                    "the stored block at a checkpoint height differs from the checkpoint; \
                     this database has to be synced again"
                );
            }
        }
        drop(reader);
        tracing::info!(height = top, hash = %tip.hash, "chain opened");

        Ok(Self {
            store,
            params,
            genesis,
            pool,
            tip: RwLock::new(tip),
            tip_watch: watch::Sender::new(tip),
            alt: RwLock::new(HashMap::new()),
            writer: Mutex::new(Writer {
                comp,
                cache,
                last_durable: Instant::now(),
            }),
            read_comps: Mutex::new(Vec::new()),
            pool_gate: RwLock::new(()),
            signal_cache: Mutex::new(None),
            cfg,
        })
    }

    // ------------------------------------------------------------------ read side

    /// Network parameters.
    #[must_use]
    pub fn params(&self) -> &'static NetworkParams {
        self.params
    }

    /// The network.
    #[must_use]
    pub fn network(&self) -> Network {
        self.cfg.network
    }

    /// Configuration.
    #[must_use]
    pub fn config(&self) -> &ChainConfig {
        &self.cfg
    }

    /// Genesis block id.
    #[must_use]
    pub fn genesis_hash(&self) -> Hash {
        self.genesis
    }

    /// The transaction pool.
    #[must_use]
    pub fn pool(&self) -> &Arc<TxPool> {
        &self.pool
    }

    /// The store (read transactions are cheap and never block the writer).
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Main-chain tip.
    #[must_use]
    pub fn tip(&self) -> TipInfo {
        *self.tip.read()
    }

    /// A storage snapshot.
    pub fn reader(&self) -> Result<Reader, ChainError> {
        Ok(self.store.reader()?)
    }

    /// Runs `f` with a (pooled) decompression context for reading blobs.
    pub fn with_compressor<R>(
        &self,
        f: impl FnOnce(&mut Compressor) -> R,
    ) -> Result<R, ChainError> {
        let cached = self.read_comps.lock().pop();
        let mut comp = match cached {
            Some(c) => c,
            None => self.store.compressor()?,
        };
        let r = f(&mut comp);
        let mut comps = self.read_comps.lock();
        if comps.len() < 16 {
            comps.push(comp);
        }
        Ok(r)
    }

    /// Number of alternative blocks held.
    #[must_use]
    pub fn alt_block_count(&self) -> usize {
        self.alt.read().len()
    }

    /// Whether `hash` is an alternative block.
    #[must_use]
    pub fn is_alt_block(&self, hash: &Hash) -> bool {
        self.alt.read().contains_key(hash)
    }

    /// `ICore::hasBlock`: main or alternative chain.
    #[must_use]
    pub fn has_block(&self, hash: &Hash) -> bool {
        self.is_alt_block(hash)
            || self
                .store
                .reader()
                .and_then(|r| r.height_of(hash))
                .is_ok_and(|h| h.is_some())
    }

    /// Whether proof of work is skipped for a block at `height` (`--fast-sync`).
    #[must_use]
    pub fn skips_pow(&self, height: u64) -> bool {
        self.cfg.fast_sync && self.cfg.checkpoints.in_zone(height)
    }

    /// The cumulative-size limit of the highest height a block received now could have: the
    /// tip plus [`SIZE_CHECK_HEIGHT_SLACK`] (the limit grows by ~0.3 bytes per block, so the
    /// slack costs ~290 KB and keeps far-ahead orphans out of the pre-decode size check).
    fn raw_size_limit(&self, blocks: usize) -> u64 {
        max_block_cumulative_size(
            self.tip()
                .height
                .saturating_add(SIZE_CHECK_HEIGHT_SLACK)
                .saturating_add(blocks as u64),
        )
    }

    fn pow_mode(&self) -> PowMode {
        match (self.cfg.fast_sync, self.cfg.checkpoints.last_height()) {
            (true, Some(last)) => PowMode::SkipUpTo(last),
            _ => PowMode::Full,
        }
    }

    /// Difficulty of the next block (`getDifficultyForNextBlock`).
    pub fn next_difficulty(&self) -> Result<u64, ChainError> {
        let reader = self.reader()?;
        let top = reader.top_height()?.unwrap_or(0);
        let ctx = context_from_storage(self.params, &reader, top + 1)?;
        let h = u32::try_from(top + 1).unwrap_or(u32::MAX);
        Ok(difficulty_for_next_block(
            self.params,
            h,
            &ctx.difficulty_timestamps,
            &ctx.difficulty_cumulative,
        ))
    }

    /// C++ `Core::blockMedianSize` for the next block: `max(median of the last 100 sizes, full
    /// reward zone)`.
    pub fn block_median_size(&self, reader: &Reader) -> Result<u64, ChainError> {
        let top = reader.top_height()?.unwrap_or(0);
        let sizes = reader.median_block_size_window(
            top,
            xkr_consensus::params::CRYPTONOTE_REWARD_BLOCKS_WINDOW,
        )?;
        let next = u32::try_from(top + 1).unwrap_or(u32::MAX);
        let zone = full_reward_zone(block_major_version_for_height(self.params, next)) as u64;
        Ok(median(&sizes).max(zone))
    }

    /// A snapshot implementing the pool's view of the chain.
    pub fn snapshot(&self) -> Result<ChainSnapshot, ChainError> {
        let reader = self.reader()?;
        let top = reader.top_height()?.unwrap_or(0);
        let median_size = self.block_median_size(&reader)?;
        Ok(ChainSnapshot::new(
            reader,
            u32::try_from(top).unwrap_or(u32::MAX),
            median_size,
            unix_now(),
        )
        .with_forks(self.params.forks()))
    }

    /// Validates a transaction against the main chain and adds it to the pool
    /// (`Core::addTransactionToPool`). Blocks cannot be committed meanwhile.
    pub fn submit_transaction(
        &self,
        blob: Vec<u8>,
        source: TxSource,
    ) -> Result<Result<Hash, PoolError>, ChainError> {
        let _gate = self.pool_gate.read();
        let snap = self.snapshot()?;
        Ok(self.pool.add(blob, &snap, source, unix_now()))
    }

    /// Holds off block commits while the guard lives (for callers that use the pool directly).
    pub fn pool_gate(&self) -> RwLockReadGuard<'_, ()> {
        self.pool_gate.read()
    }

    /// Makes every non-durable commit durable (call on shutdown).
    pub fn flush(&self) -> Result<(), ChainError> {
        let mut w = self.writer.lock();
        let txn = self.store.begin_write()?;
        commit(txn, true)?;
        w.last_durable = Instant::now();
        Ok(())
    }

    /// Stops storing for good: makes the last batch durable and closes the database
    /// (`xkr_storage::Store::close`), so the next start finds a clean file and skips redb's
    /// check of the whole file. Holds the writer, so no block is being stored meanwhile. Other
    /// handles to this chain may still exist (tasks that outlive the stop); their storage calls
    /// return `xkr_storage::Error::Closed` from then on ([`ChainError::is_closed`]). Closing
    /// again does nothing. The close runs even when the final commit fails; the first error
    /// is returned.
    pub fn close(&self) -> Result<(), ChainError> {
        let mut w = self.writer.lock();
        let flushed = if self.store.is_closed() {
            Ok(())
        } else {
            self.store
                .begin_write()
                .map_err(ChainError::from)
                .and_then(|txn| commit(txn, true))
        };
        if flushed.is_ok() {
            w.last_durable = Instant::now();
        }
        let closed = self.store.close();
        drop(w);
        flushed?;
        closed?;
        Ok(())
    }

    /// Whether [`Chain::close`] ran.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.store.is_closed()
    }

    /// Database maintenance (`xkr_storage::Store::maintain`): repacks under-filled index
    /// tables and compacts the file. Holds the writer for the whole run, so no block is
    /// applied meanwhile; storage readers (RPC, P2P serving) get `Busy` errors until it ends.
    /// Meant for the end of the initial sync (minutes on a full mainnet database).
    pub fn maintain(
        &self,
        opts: &xkr_storage::MaintainOptions,
    ) -> Result<xkr_storage::MaintainReport, ChainError> {
        let mut w = self.writer.lock();
        let txn = self.store.begin_write()?;
        commit(txn, true)?;
        w.last_durable = Instant::now();
        let report = self.store.maintain(opts, &mut |msg| {
            tracing::info!("database maintenance: {msg}");
        })?;
        w.comp.clear_cache();
        self.read_comps.lock().clear();
        Ok(report)
    }

    /// Stripe-pruning bound for the current tip ([`Checkpoints::prune_bound`]).
    #[must_use]
    pub fn prune_bound(&self) -> u64 {
        self.cfg.checkpoints.prune_bound(self.tip().height)
    }

    /// Stripe pruning step (`docs/storage.md` "Stripe pruning"): if the database has a stripe
    /// seed, drops the other stripes' signatures below [`Self::prune_bound`]. `Ok(None)` when
    /// nothing was due: a full (or completely pruned) database, or a bound that has not moved
    /// past the stored prune height (a cheap snapshot check, so callers can run it after every
    /// batch). A large first prune of a full database runs in steps of
    /// [`xkr_storage::PRUNE_STEP_BLOCKS`], one durable transaction each; the writer is taken per
    /// step (and the state and bound read again under it), so other writers can run between
    /// the steps (the chain thread, which calls this, still runs the steps back to back).
    pub fn prune(&self) -> Result<Option<PruneReport>, ChainError> {
        if !self.reader()?.prune_state().seed.is_stripe() {
            return Ok(None);
        }
        let mut total: Option<PruneReport> = None;
        loop {
            let (report, before) = {
                let _writer = self.writer.lock();
                let state = self.reader()?.prune_state();
                let bound = self.prune_bound();
                if !state.seed.is_stripe() || bound <= state.pruned_below {
                    break;
                }
                let below = bound.min(state.pruned_below.saturating_add(PRUNE_STEP_BLOCKS));
                let report = self
                    .store
                    .prune(PruneMode::Stripe(state.seed), below, &mut |_| {})?;
                tracing::debug!(
                    pruned_below = report.state.pruned_below,
                    chunks = report.chunks,
                    "pruning signatures"
                );
                (report, state.pruned_below)
            };
            let progressed = report.state.pruned_below > before;
            total.get_or_insert_default().absorb(&report);
            if !progressed {
                break;
            }
        }
        Ok(total)
    }

    // ------------------------------------------------------------------ write side

    /// Adds a batch of blocks in height order (sync). Stage 1 (decode, hashes, PoW, semantic
    /// checks) runs in parallel over the batch, stage 2 (context) sequentially, stage 3 (ring
    /// signatures) in parallel, then one write transaction stores the accepted prefix.
    ///
    /// `expected` are the ids that were requested; a different block is rejected. Leading
    /// blocks already in the main chain are skipped. A batch that does not extend the tip is
    /// handled block by block as alternative blocks (which may switch the main chain).
    pub fn add_blocks(
        &self,
        raws: Vec<RawBlock>,
        expected: Option<&[Hash]>,
    ) -> Result<BatchReport, ChainError> {
        let report = self.add_blocks_inner(raws, expected);
        self.publish_tip();
        report
    }

    fn add_blocks_inner(
        &self,
        raws: Vec<RawBlock>,
        expected: Option<&[Hash]>,
    ) -> Result<BatchReport, ChainError> {
        let submitted = raws.len();
        // `--fast-sync` applies to requested batches only.
        let requested = expected.is_some();
        let pow = if requested {
            self.pow_mode()
        } else {
            PowMode::Full
        };
        let size_limit = self.raw_size_limit(submitted);
        let prepare_started = Instant::now();
        // Stage 1 is a pure function of the bytes: it runs before the writer lock is taken, and
        // the sizes are checked before anything is decoded. The templates are decoded first, so
        // the ids are compared with the requested ones before any transaction is decoded or
        // slow hash computed: only the blocks before the first one that fails to decode or is
        // not the requested block are prepared.
        let decoded: Vec<Result<DecodedBlock, PrepareError>> = raws
            .into_par_iter()
            .map(|r| {
                check_raw_sizes(&r, size_limit)
                    .map_err(|reason| PrepareError { hash: None, reason })?;
                decode_raw(r)
            })
            .collect();
        let mut late_error = None;
        let mut wanted = Vec::with_capacity(submitted);
        for (i, d) in decoded.into_iter().enumerate() {
            match d {
                Ok(d) => {
                    if let Some(exp) = expected.and_then(|e| e.get(i))
                        && *exp != d.hash
                    {
                        late_error = Some(BatchError {
                            index: i,
                            hash: Some(d.hash),
                            kind: BatchErrorKind::Invalid,
                            reason: RejectReason::UnexpectedBlock {
                                expected: *exp,
                                got: d.hash,
                            },
                        });
                        break;
                    }
                    wanted.push(d);
                }
                Err(e) => {
                    late_error = Some(BatchError {
                        index: i,
                        hash: e.hash,
                        kind: BatchErrorKind::Invalid,
                        reason: e.reason,
                    });
                    break;
                }
            }
        }
        let prepared: Vec<Result<PreparedBlock, PrepareError>> =
            wanted.into_par_iter().map(|d| d.prepare(pow)).collect();
        let prepare_time = prepare_started.elapsed();
        let mut w = self.writer.lock();

        let mut report = BatchReport {
            submitted,
            times: StageTimes {
                prepare: prepare_time,
                ..StageTimes::default()
            },
            ..BatchReport::default()
        };
        let mut ok = Vec::with_capacity(prepared.len());
        for (i, p) in prepared.into_iter().enumerate() {
            match p {
                Ok(p) => ok.push(p),
                Err(e) => {
                    late_error = Some(BatchError {
                        index: i,
                        hash: e.hash,
                        kind: BatchErrorKind::Invalid,
                        reason: e.reason,
                    });
                    break;
                }
            }
        }

        let reader = self.reader()?;
        let mut skip = 0;
        while skip < ok.len() && reader.height_of(&ok[skip].hash)?.is_some() {
            skip += 1;
        }
        report.skipped = skip;
        let rest: Vec<PreparedBlock> = ok.into_iter().skip(skip).collect();
        let rest_len = rest.len();

        if rest
            .first()
            .is_some_and(|p| p.block.header.prev_hash == Hash(w.cache.top().hash))
        {
            let res = self.push_main(&mut w, reader, rest, false, requested)?;
            report.added_main = res.pushed.len();
            report.times = StageTimes {
                prepare: prepare_time,
                ..res.times
            };
            if let Some((i, hash, reason)) = res.fail {
                let kind = if reason == RejectReason::NotConnected {
                    BatchErrorKind::Orphan
                } else {
                    BatchErrorKind::Invalid
                };
                report.error = Some(BatchError {
                    index: skip + i,
                    hash: Some(hash),
                    kind,
                    reason,
                });
            }
        } else if !rest.is_empty() {
            drop(reader);
            for (k, p) in rest.into_iter().enumerate() {
                let hash = p.hash;
                let r = self.add_prepared(&mut w, p, requested)?;
                match r.outcome {
                    AddBlockOutcome::AddedToMain => report.added_main += 1,
                    AddBlockOutcome::AddedToAlternative => report.added_alt += 1,
                    AddBlockOutcome::AddedAndSwitched => {
                        report.added_alt += 1;
                        report.switched = true;
                    }
                    AddBlockOutcome::AlreadyExists => {}
                    AddBlockOutcome::Orphan => {
                        report.error = Some(BatchError {
                            index: skip + k,
                            hash: Some(hash),
                            kind: BatchErrorKind::Orphan,
                            reason: RejectReason::NotConnected,
                        });
                        break;
                    }
                    AddBlockOutcome::Invalid(reason) => {
                        report.error = Some(BatchError {
                            index: skip + k,
                            hash: Some(hash),
                            kind: BatchErrorKind::Invalid,
                            reason,
                        });
                        break;
                    }
                }
            }
        }
        if report.error.is_none()
            && let Some(e) = late_error
        {
            debug_assert_eq!(e.index, skip + rest_len);
            report.error = Some(e);
        }
        report.tip = TipInfo::from_info(w.cache.top_height(), w.cache.top());
        Ok(report)
    }

    /// Adds one block (relayed `NOTIFY_NEW_BLOCK` / completed lite block, `submitblock`).
    ///
    /// Cheap checks first: the raw sizes before anything is decoded, then only the template is
    /// decoded and, under the writer lock, an already known block, an orphan, an alternative
    /// block below a checkpoint the main chain has reached, a header that fails against its
    /// parent's context (major version, parent block, timestamps, coinbase) and a checkpoint
    /// mismatch are answered at once. Then, outside the writer lock, the slow hash of the
    /// template is computed and the proof of work checked against the difficulty of the
    /// block's parent; only a block that meets it has its transactions decoded and checked,
    /// before the full [`Self::add_prepared`] path runs. The proof of work is always checked
    /// here, also with `--fast-sync`.
    pub fn add_block(&self, raw: RawBlock) -> Result<AddBlockResult, ChainError> {
        let result = self.add_block_inner(raw);
        self.publish_tip();
        result
    }

    fn add_block_inner(&self, raw: RawBlock) -> Result<AddBlockResult, ChainError> {
        let rejected = |e: PrepareError| {
            Ok(AddBlockResult {
                hash: e.hash.unwrap_or(Hash::ZERO),
                height: None,
                outcome: AddBlockOutcome::Invalid(e.reason),
            })
        };
        if let Err(reason) = check_raw_sizes(&raw, self.raw_size_limit(1)) {
            return rejected(PrepareError { hash: None, reason });
        }
        let decoded = match decode_raw(raw) {
            Ok(d) => d,
            Err(e) => return rejected(e),
        };
        let (height, difficulty) = {
            let w = self.writer.lock();
            match self.precheck(&w, &decoded)? {
                Precheck::Answered(early) => return Ok(early),
                Precheck::Continue { height, difficulty } => (height, difficulty),
            }
        };
        let pow_failed = |e: BlockError| {
            Ok(AddBlockResult {
                hash: decoded.hash,
                height: Some(height),
                outcome: AddBlockOutcome::Invalid(RejectReason::Consensus(ConsensusError::Block(
                    e,
                ))),
            })
        };
        // C++ maps any failure of getBlockLongHash to "proof of work too weak".
        let Ok(long_hash) = block_long_hash(&decoded.block) else {
            return pow_failed(BlockError::ProofOfWorkTooWeak);
        };
        if let Err(e) =
            check_proof_of_work_with_hash(&decoded.block, &long_hash, difficulty, &self.genesis)
        {
            return pow_failed(e);
        }
        let p = match decoded.prepare_with_long_hash(long_hash) {
            Ok(p) => p,
            Err(e) => return rejected(e),
        };
        let mut w = self.writer.lock();
        self.add_prepared(&mut w, p, false)
    }

    /// The cheap part of [`Self::add_prepared`] on a decoded template, under the writer lock:
    /// [`Precheck::Answered`] when the block is already known, an orphan, an alternative block
    /// below a checkpoint the main chain has reached ([`Checkpoints::reached_at_or_above`]), its
    /// header fails [`check_header_cheap`] against its parent's context, or its hash differs
    /// from a checkpoint at its height; otherwise its height and the difficulty it must meet.
    /// Each answer is the one `add_prepared` would give at this moment (only the order of
    /// checks differs).
    fn precheck(&self, w: &Writer, d: &DecodedBlock) -> Result<Precheck, ChainError> {
        let hash = d.hash;
        let known = |height| {
            Ok(Precheck::Answered(AddBlockResult {
                hash,
                height: Some(height),
                outcome: AddBlockOutcome::AlreadyExists,
            }))
        };
        let reader = self.reader()?;
        if let Some(h) = reader.height_of(&hash)? {
            return known(h);
        }
        let alt_height = self.alt.read().get(&hash).map(|e| e.height);
        if let Some(h) = alt_height {
            return known(h);
        }
        let prev = d.block.header.prev_hash;
        let extends_tip = prev == Hash(w.cache.top().hash);
        let parent_height = if extends_tip {
            Some(w.cache.top_height())
        } else if let Some(ph) = reader.height_of(&prev)? {
            Some(ph)
        } else {
            self.alt.read().get(&prev).map(|e| e.height)
        };
        let Some(parent_height) = parent_height else {
            return Ok(Precheck::Answered(AddBlockResult {
                hash,
                height: None,
                outcome: AddBlockOutcome::Orphan,
            }));
        };
        let height = parent_height + 1;
        let invalid = |reason| {
            Ok(Precheck::Answered(AddBlockResult {
                hash,
                height: Some(height),
                outcome: AddBlockOutcome::Invalid(reason),
            }))
        };
        if !extends_tip
            && let Some(checkpoint) = self
                .cfg
                .checkpoints
                .reached_at_or_above(w.cache.top_height(), height)
        {
            return invalid(RejectReason::BelowCheckpoint { height, checkpoint });
        }
        let ctx = if extends_tip {
            w.cache.next_context(self.params)?
        } else {
            let (base, headers) = self.alt_headers(&reader, prev, height)?;
            context_data(self.params, height, base, &headers)?
        };
        let difficulty =
            match check_header_cheap(self.params, &d.block, &ctx, &self.genesis, unix_now()) {
                Ok(difficulty) => difficulty,
                Err(reason) => return invalid(reason),
            };
        if !self.cfg.checkpoints.check(height, &hash) {
            return invalid(RejectReason::Checkpoint(height));
        }
        Ok(Precheck::Continue { height, difficulty })
    }

    /// `Core::addBlock` for one prepared block: main chain, alternative chain or orphan.
    /// `requested`: the block came in a requested sync batch, so `--fast-sync` may skip its
    /// proof of work when it extends the main chain below the last checkpoint. Alternative
    /// blocks always have their proof of work checked.
    fn add_prepared(
        &self,
        w: &mut Writer,
        p: PreparedBlock,
        requested: bool,
    ) -> Result<AddBlockResult, ChainError> {
        let hash = p.hash;
        let reader = self.reader()?;
        if let Some(h) = reader.height_of(&hash)? {
            return Ok(AddBlockResult {
                hash,
                height: Some(h),
                outcome: AddBlockOutcome::AlreadyExists,
            });
        }
        if let Some(e) = self.alt.read().get(&hash) {
            return Ok(AddBlockResult {
                hash,
                height: Some(e.height),
                outcome: AddBlockOutcome::AlreadyExists,
            });
        }
        let prev = p.block.header.prev_hash;
        if prev == Hash(w.cache.top().hash) {
            let height = w.cache.top_height() + 1;
            let res = self.push_main(w, reader, vec![p], true, requested)?;
            let outcome = match res.fail {
                None => AddBlockOutcome::AddedToMain,
                Some((_, _, reason)) => AddBlockOutcome::Invalid(reason),
            };
            return Ok(AddBlockResult {
                hash,
                height: Some(height),
                outcome,
            });
        }

        // Alternative block: the parent is a main-chain block below the tip or an alt block.
        let parent_info = if let Some(ph) = reader.height_of(&prev)? {
            let info = reader
                .block_info(ph)?
                .ok_or_else(|| ChainError::Corrupt(format!("no info for {ph}")))?;
            Some((ph, info))
        } else {
            self.alt.read().get(&prev).map(|e| (e.height, e.info))
        };
        let Some((parent_height, parent)) = parent_info else {
            return Ok(AddBlockResult {
                hash,
                height: None,
                outcome: AddBlockOutcome::Orphan,
            });
        };
        let height = parent_height + 1;
        let invalid = |reason: RejectReason| -> Result<AddBlockResult, ChainError> {
            Ok(AddBlockResult {
                hash,
                height: Some(height),
                outcome: AddBlockOutcome::Invalid(reason),
            })
        };

        if let Some(checkpoint) = self
            .cfg
            .checkpoints
            .reached_at_or_above(w.cache.top_height(), height)
        {
            return invalid(RejectReason::BelowCheckpoint { height, checkpoint });
        }

        // Context along the alternative branch.
        let (base, headers) = self.alt_headers(&reader, prev, height)?;
        let ctx = context_data(self.params, height, base, &headers)?;
        let verdict = match check_header(
            self.params,
            &self.cfg.checkpoints,
            &p,
            &ctx,
            &self.genesis,
            unix_now(),
            true,
        ) {
            Ok(v) => v,
            Err(reason) => return invalid(reason),
        };
        let info = next_info(&parent, &p, &verdict);
        drop(reader);

        let txn = self.store.begin_write()?;
        let header = AltBlockHeader {
            height,
            cumulative_difficulty_lo: info.cumulative_difficulty_lo,
            cumulative_difficulty_hi: info.cumulative_difficulty_hi,
            block_size: info.block_size,
        };
        put_alt_block(&txn, &hash, header, &p.to_raw(), &mut w.comp)?;
        commit(txn, false)?;
        self.alt
            .write()
            .insert(hash, AltEntry { height, prev, info });
        tracing::info!(%hash, height, "alternative block added");

        let tip_cd = w.cache.top().cumulative_difficulty();
        if info.cumulative_difficulty() > tip_cd {
            return match self.switch_to(w, hash)? {
                Switch::Done => Ok(AddBlockResult {
                    hash,
                    height: Some(height),
                    outcome: AddBlockOutcome::AddedAndSwitched,
                }),
                Switch::Refused => Ok(AddBlockResult {
                    hash,
                    height: Some(height),
                    outcome: AddBlockOutcome::AddedToAlternative,
                }),
                Switch::Failed(reason) => invalid(reason),
            };
        }
        self.prune_alt(w.cache.top_height());
        Ok(AddBlockResult {
            hash,
            height: Some(height),
            outcome: AddBlockOutcome::AddedToAlternative,
        })
    }

    /// Headers `base..height` along the branch ending at `parent` (main-chain blocks up to the
    /// split point, alternative blocks above it).
    fn alt_headers(
        &self,
        reader: &Reader,
        parent: Hash,
        height: u64,
    ) -> Result<(u64, Vec<BlockInfo>), ChainError> {
        let mut branch = Vec::new();
        let mut cursor = parent;
        let alt = self.alt.read();
        while let Some(e) = alt.get(&cursor) {
            branch.push(e.info);
            cursor = e.prev;
        }
        drop(alt);
        branch.reverse();
        let split = height - 1 - branch.len() as u64;
        let base = window_start(self.params, height).min(split + 1);
        let mut headers = reader.block_infos(base, split + 1)?;
        if headers.len() as u64 != split + 1 - base {
            return Err(ChainError::Corrupt(format!(
                "missing main-chain infos below {split}"
            )));
        }
        headers.extend(branch);
        Ok((base, headers))
    }

    fn prune_alt(&self, top: u64) {
        let keep_from = top.saturating_sub(self.cfg.max_reorg_depth);
        let mut alt = self.alt.write();
        if alt.len() < 64 {
            return;
        }
        let old: Vec<Hash> = alt
            .iter()
            .filter(|(_, e)| e.height < keep_from)
            .map(|(h, _)| *h)
            .collect();
        for h in &old {
            alt.remove(h);
        }
        drop(alt);
        if !old.is_empty()
            && let Ok(txn) = self.store.begin_write()
        {
            let _ = prune_alt_blocks_below(&txn, keep_from);
            let _ = commit(txn, false);
        }
    }

    /// Stages 2-4 for blocks that extend the tip, in order. Stores the accepted prefix.
    /// `fast_sync_pow`: whether `--fast-sync` may skip the proof of work of these blocks
    /// ([`Self::skips_pow`]); false checks it for every block.
    fn push_main(
        &self,
        w: &mut Writer,
        reader: Reader,
        blocks: Vec<PreparedBlock>,
        durable: bool,
        fast_sync_pow: bool,
    ) -> Result<MainResult, ChainError> {
        let now = unix_now();
        let start_top = w.cache.top_height();
        let mut overlay = Overlay::default();
        let mut jobs: Vec<SigJob> = Vec::new();
        let mut verdicts: Vec<HeaderVerdict> = Vec::with_capacity(blocks.len());
        let mut fail: Option<(usize, RejectReason)> = None;
        let mut times = StageTimes::default();
        let context_started = Instant::now();

        // Stage 2: sequential context checks. The header window is extended block by block; on
        // a node-side error it is reloaded from storage before returning.
        let mut node_error: Option<ChainError> = None;
        for (i, p) in blocks.iter().enumerate() {
            let height = start_top + 1 + i as u64;
            if p.block.header.prev_hash != Hash(w.cache.top().hash) {
                fail = Some((i, RejectReason::NotConnected));
                break;
            }
            let ctx = match w.cache.next_context(self.params) {
                Ok(c) => c,
                Err(e) => {
                    node_error = Some(e);
                    break;
                }
            };
            let verdict = match check_header(
                self.params,
                &self.cfg.checkpoints,
                p,
                &ctx,
                &self.genesis,
                now,
                !(fast_sync_pow && self.skips_pow(height)),
            ) {
                Ok(v) => v,
                Err(r) => {
                    fail = Some((i, r));
                    break;
                }
            };
            let prev_index = u32::try_from(height - 1).unwrap_or(u32::MAX);
            match check_inputs(p, i, prev_index, now, &reader, &mut overlay, &mut jobs) {
                Ok(Ok(())) => {}
                Ok(Err(r)) => {
                    fail = Some((i, r));
                    break;
                }
                Err(e) => {
                    node_error = Some(e);
                    break;
                }
            }
            if let Err(e) = overlay.add_block(&reader, p) {
                node_error = Some(e);
                break;
            }
            let info = next_info(w.cache.top(), p, &verdict);
            w.cache.push(info);
            verdicts.push(verdict);
        }
        drop(reader);
        times.context = context_started.elapsed();
        if let Some(e) = node_error {
            w.cache = HeaderCache::load(
                &self.reader()?,
                start_top,
                self.cfg.header_cache.max(DEFAULT_HEADER_CACHE),
            )?;
            return Err(e);
        }

        // Stage 3: ring signatures of every block that passed stage 2, in parallel.
        let checked = fail.as_ref().map_or(blocks.len(), |f| f.0);
        jobs.retain(|j| j.block < checked);
        let signatures_started = Instant::now();
        if let Some((b, t)) = verify_signatures(&blocks[..checked], &jobs) {
            fail = Some((b, signature_error(t)));
        }
        times.signatures = signatures_started.elapsed();
        let accepted = fail.as_ref().map_or(blocks.len(), |f| f.0);
        if !w.cache.truncate_to(start_top + accepted as u64) {
            w.cache = HeaderCache::load(
                &self.reader()?,
                start_top,
                self.cfg.header_cache.max(DEFAULT_HEADER_CACHE),
            )?;
        }
        let fail = fail.map(|(i, r)| (i, blocks[i].hash, r));

        if accepted == 0 {
            return Ok(MainResult {
                pushed: Vec::new(),
                fail,
                times,
            });
        }

        // Stage 4: one write transaction for the accepted prefix.
        let pushed: Vec<VerifiedBlock> = blocks
            .into_iter()
            .take(accepted)
            .zip(verdicts)
            .enumerate()
            .map(|(i, (p, v))| {
                let height = start_top + 1 + i as u64;
                let info = w.cache.get(height).copied();
                into_verified(p, height, &v, info.map_or(0, |i| i.already_generated_coins))
            })
            .collect();
        match self.store_blocks(w, &pushed, durable) {
            Ok((write, commit)) => {
                times.write = write;
                times.commit = commit;
            }
            Err(e) => {
                w.cache = HeaderCache::load(
                    &self.reader()?,
                    start_top,
                    self.cfg.header_cache.max(DEFAULT_HEADER_CACHE),
                )?;
                return Err(e);
            }
        }
        Ok(MainResult {
            pushed,
            fail,
            times,
        })
    }

    /// Writes verified blocks, commits, updates the tip and the pool. Returns the time of the
    /// write and of the commit with the tip and pool update.
    fn store_blocks(
        &self,
        w: &mut Writer,
        blocks: &[VerifiedBlock],
        force_durable: bool,
    ) -> Result<(Duration, Duration), ChainError> {
        let write_started = Instant::now();
        let durable = force_durable || w.last_durable.elapsed() >= self.cfg.durable_interval;
        let txn = self.store.begin_write()?;
        push_blocks(&txn, blocks, &mut w.comp)?;
        let write = write_started.elapsed();
        let commit_started = Instant::now();
        let gate = self.pool_gate.write();
        commit(txn, durable)?;
        if durable {
            w.last_durable = Instant::now();
        }
        *self.tip.write() = TipInfo::from_info(w.cache.top_height(), w.cache.top());
        let (hashes, images) = included(blocks);
        let dropped = self.pool.remove_for_block(&hashes, &images);
        // Fork 14: once version 1 is no longer allowed in the next block, it leaves the pool.
        let forks = self.params.forks();
        if forks.fork14_height().is_some() {
            let next = w.cache.top_height() + 1;
            let evicted = self.pool.remove_disallowed(unix_now(), |tx| {
                xkr_consensus::check_transaction_version(&forks, next, tx).is_ok()
            });
            if !evicted.is_empty() {
                tracing::debug!(
                    count = evicted.len(),
                    "pool transactions evicted: version not allowed after fork 14"
                );
            }
        }
        drop(gate);
        if !dropped.is_empty() {
            tracing::debug!(
                count = dropped.len(),
                "pool transactions dropped by key-image conflicts"
            );
        }
        Ok((write, commit_started.elapsed()))
    }

    /// Makes the alternative branch ending at `alt_tip` the main chain.
    fn switch_to(&self, w: &mut Writer, alt_tip: Hash) -> Result<Switch, ChainError> {
        // The branch, oldest first.
        let mut branch: Vec<(Hash, AltEntry)> = Vec::new();
        {
            let alt = self.alt.read();
            let mut cursor = alt_tip;
            while let Some(e) = alt.get(&cursor) {
                branch.push((cursor, *e));
                cursor = e.prev;
            }
        }
        branch.reverse();
        let Some((_, first)) = branch.first() else {
            return Ok(Switch::Refused);
        };
        let split = first.height - 1;
        let top = w.cache.top_height();
        let depth = top - split;
        if depth > self.cfg.max_reorg_depth {
            tracing::warn!(
                depth,
                max = self.cfg.max_reorg_depth,
                "refusing a reorg deeper than the configured maximum"
            );
            return Ok(Switch::Refused);
        }
        if let Some(checkpoint) = self.cfg.checkpoints.reached_at_or_above(top, split + 1) {
            tracing::warn!(
                split,
                checkpoint,
                "refusing a reorg below a reached checkpoint"
            );
            return Ok(Switch::Refused);
        }
        if depth > self.cfg.warn_reorg_depth {
            tracing::warn!(depth, split, "deep chain reorganisation");
        } else {
            tracing::info!(depth, split, new_tip = %alt_tip, "chain reorganisation");
        }

        // Load the alternative blocks and the main blocks to pop. A pruned block cannot be
        // rebuilt for `pop_blocks` (prune_bound keeps 2 stripe spans whole, so this needs a
        // `max_reorg_depth` above 8192).
        let reader = self.reader()?;
        let popped_infos = reader.block_infos(split + 1, top + 1)?;
        if let Some((i, _)) = popped_infos
            .iter()
            .enumerate()
            .find(|(_, info)| info.is_pruned() && info.tx_count > 0)
        {
            tracing::warn!(
                split,
                pruned = split + 1 + i as u64,
                "refusing a reorg that would pop blocks whose signatures this node pruned"
            );
            return Ok(Switch::Refused);
        }
        let mut raws = Vec::with_capacity(branch.len());
        for (h, _) in &branch {
            let (_, raw) = reader
                .alt_block(h, &mut w.comp)?
                .ok_or_else(|| ChainError::Corrupt(format!("alternative block {h} missing")))?;
            raws.push(raw);
        }
        let mut popped = Vec::with_capacity((top - split) as usize);
        for h in (split + 1..=top).rev() {
            popped.push(load_verified(&reader, h, &mut w.comp)?);
        }
        drop(reader);

        // Pop.
        let txn = self.store.begin_write()?;
        pop_blocks(&txn, &popped)?;
        commit(txn, true)?;
        w.cache = HeaderCache::load(
            &self.reader()?,
            split,
            self.cfg.header_cache.max(DEFAULT_HEADER_CACHE),
        )?;
        *self.tip.write() = TipInfo::from_info(split, w.cache.top());

        // The pool as it was: if the branch fails, the old main chain comes back and so do the
        // pool transactions that pushing part of the branch removed.
        let pool_before = self.pool.blobs();

        // Push the branch through the full main-chain pipeline. Every alternative block had its
        // proof of work checked when it was added, so `--fast-sync` may skip it here.
        let pow = self.pow_mode();
        let prepared: Vec<Result<PreparedBlock, PrepareError>> = raws
            .into_par_iter()
            .map(|r| prepare_block(r, pow))
            .collect();
        let mut ok = Vec::with_capacity(prepared.len());
        let mut prep_fail = None;
        for (i, p) in prepared.into_iter().enumerate() {
            match p {
                Ok(p) => ok.push(p),
                Err(e) => {
                    prep_fail = Some((i, e.reason));
                    break;
                }
            }
        }
        let res = self.push_main(w, self.reader()?, ok, true, true)?;
        let failure = match (res.fail, prep_fail) {
            (Some((i, _, r)), _) | (None, Some((i, r))) => Some((i, r)),
            (None, None) => None,
        };

        if let Some((bad, reason)) = failure {
            // Roll back to the old main chain.
            tracing::warn!(%reason, "alternative branch invalid, restoring the previous main chain");
            let txn = self.store.begin_write()?;
            let mut back = res.pushed;
            back.reverse();
            pop_blocks(&txn, &back)?;
            popped.reverse();
            push_blocks(&txn, &popped, &mut w.comp)?;
            for (h, _) in &branch[bad..] {
                remove_alt_block(&txn, h)?;
            }
            commit(txn, true)?;
            {
                let mut alt = self.alt.write();
                for (h, _) in &branch[bad..] {
                    alt.remove(h);
                }
            }
            w.cache = HeaderCache::load(
                &self.reader()?,
                top,
                self.cfg.header_cache.max(DEFAULT_HEADER_CACHE),
            )?;
            *self.tip.write() = TipInfo::from_info(top, w.cache.top());
            {
                let _gate = self.pool_gate.write();
                let blobs: Vec<Vec<u8>> = pool_before
                    .into_iter()
                    .filter(|(h, _)| !self.pool.contains(h))
                    .map(|(_, b)| b.to_vec())
                    .collect();
                if !blobs.is_empty() {
                    let snap = self.snapshot()?;
                    let n = self.pool.readd(blobs, &snap, unix_now());
                    tracing::debug!(
                        readded = n,
                        "pool transactions returned after the alternative branch failed"
                    );
                }
            }
            return Ok(Switch::Failed(reason));
        }

        // Success: the branch is main now, the old main blocks become alternative blocks.
        let txn = self.store.begin_write()?;
        for (h, _) in &branch {
            remove_alt_block(&txn, h)?;
        }
        let mut old_alt = Vec::with_capacity(popped.len());
        for (vb, info) in popped.iter().rev().zip(&popped_infos) {
            let raw = RawBlock {
                block: vb.block_blob.clone(),
                transactions: vb.txs.iter().map(|t| t.blob.clone()).collect(),
            };
            let header = AltBlockHeader {
                height: vb.height,
                cumulative_difficulty_lo: info.cumulative_difficulty_lo,
                cumulative_difficulty_hi: info.cumulative_difficulty_hi,
                block_size: info.block_size,
            };
            put_alt_block(&txn, &vb.hash, header, &raw, &mut w.comp)?;
            old_alt.push((
                vb.hash,
                AltEntry {
                    height: vb.height,
                    prev: vb.block.header.prev_hash,
                    info: *info,
                },
            ));
        }
        commit(txn, true)?;
        {
            let mut alt = self.alt.write();
            for (h, _) in &branch {
                alt.remove(h);
            }
            alt.extend(old_alt);
        }

        // C++ `actualizePoolTransactions` then `copyTransactionsToPool`: the pool is validated
        // again against the new main chain, then the transactions of the old main chain go
        // back to the pool if still valid.
        let blobs: Vec<Vec<u8>> = popped
            .iter()
            .flat_map(|b| b.txs.iter().map(|t| t.blob.clone()))
            .collect();
        {
            let _gate = self.pool_gate.write();
            let snap = self.snapshot()?;
            let removed = self.pool.revalidate(&snap);
            if !removed.is_empty() {
                tracing::info!(
                    removed = removed.len(),
                    "pool transactions no longer valid on the new main chain"
                );
            }
            if !blobs.is_empty() {
                let n = self.pool.readd(blobs, &snap, unix_now());
                tracing::info!(
                    readded = n,
                    "transactions of the old main chain returned to the pool"
                );
            }
        }
        Ok(Switch::Done)
    }
}

/// Result of [`Chain::precheck`].
enum Precheck {
    /// The block is answered without preparing it.
    Answered(AddBlockResult),
    /// The block must be prepared: its height and the difficulty its proof of work must meet.
    Continue { height: u64, difficulty: u64 },
}

/// Result of [`Chain::switch_to`].
enum Switch {
    Done,
    Refused,
    Failed(RejectReason),
}

/// `BlockInfo` of the block `p` on top of `parent` (mirrors `xkr_storage::ops::push_blocks`).
fn next_info(parent: &BlockInfo, p: &PreparedBlock, v: &HeaderVerdict) -> BlockInfo {
    let tx_count = p.txs.len() as u32;
    let mut info = BlockInfo {
        hash: p.hash.0,
        timestamp: p.block.header.timestamp,
        difficulty: v.difficulty,
        cumulative_difficulty_lo: 0,
        cumulative_difficulty_hi: 0,
        already_generated_coins: parent
            .already_generated_coins
            .wrapping_add(v.emission_change as u64),
        cumulative_tx_count: parent.cumulative_tx_count + 1 + u64::from(tx_count),
        block_size: v.cumulative_size,
        cumulative_size: parent.cumulative_size + v.cumulative_size,
        tx_count,
        flags: 0,
    };
    info.set_cumulative_difficulty(parent.cumulative_difficulty() + u128::from(v.difficulty));
    info
}

fn into_verified(
    p: PreparedBlock,
    height: u64,
    v: &HeaderVerdict,
    already_generated_coins: u64,
) -> VerifiedBlock {
    VerifiedBlock {
        height,
        hash: p.hash,
        block: p.block,
        block_blob: p.blob,
        miner_tx_hash: p.miner_tx_hash,
        difficulty: v.difficulty,
        already_generated_coins,
        block_size: v.cumulative_size,
        txs: p
            .txs
            .into_iter()
            .map(crate::prepare::PreparedTx::into_verified)
            .collect(),
    }
}

/// Transaction hashes and key images of stored blocks (pool maintenance).
fn included(blocks: &[VerifiedBlock]) -> (Vec<Hash>, Vec<KeyImage>) {
    let mut hashes = Vec::new();
    let mut images = Vec::new();
    for b in blocks {
        for t in &b.txs {
            hashes.push(t.hash);
            for i in &t.tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = i {
                    images.push(*key_image);
                }
            }
        }
    }
    (hashes, images)
}

/// Rebuilds the [`VerifiedBlock`] of a stored main-chain block (needed by `pop_blocks`).
/// [`ChainError::Pruned`] if the node pruned the block's signatures.
pub fn load_verified(
    reader: &Reader,
    height: u64,
    comp: &mut Compressor,
) -> Result<VerifiedBlock, ChainError> {
    let info = reader
        .block_info(height)?
        .ok_or_else(|| ChainError::Corrupt(format!("no block info at {height}")))?;
    let raw = reader
        .raw_block(height, comp)
        .map_err(|e| match e {
            xkr_storage::Error::Pruned(h) => ChainError::Pruned(h),
            e => ChainError::Storage(e),
        })?
        .ok_or_else(|| ChainError::Corrupt(format!("no block at {height}")))?;
    let block = decode_block(&raw.block)
        .map_err(|e| ChainError::Corrupt(format!("block {height}: {e}")))?;
    let coinbase = encode_transaction(&block.miner_tx)?;
    let mut txs = Vec::with_capacity(raw.transactions.len());
    for blob in raw.transactions {
        let tx = decode_transaction(&blob)
            .map_err(|e| ChainError::Corrupt(format!("tx in block {height}: {e}")))?;
        let sig_bytes = tx.signature_section_len();
        txs.push(VerifiedTx {
            hash: transaction_hash_from_blob(&blob),
            prefix_len: blob.len().saturating_sub(sig_bytes),
            payment_id: payment_id(&tx.prefix.extra).map(|h| h.0),
            tx,
            blob,
        });
    }
    Ok(VerifiedBlock {
        height,
        hash: Hash(info.hash),
        block,
        block_blob: raw.block,
        miner_tx_hash: cn_fast_hash(&coinbase),
        difficulty: info.difficulty,
        already_generated_coins: info.already_generated_coins,
        block_size: info.block_size,
        txs,
    })
}
