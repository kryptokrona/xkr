//! The sync engine (`src/wallet_backend/wallet_synchronizer.cpp` `WalletSynchronizer`, and the
//! fetch/process split of wallet-backend-js `WalletSynchronizer`).
//!
//! Two tasks run per wallet:
//! * the **fetch task** keeps the daemon status fresh (`/info` every
//!   [`SyncConfig::info_interval`], `Nigel`'s background poll), asks `/getwalletsyncdata` for
//!   the blocks after the newest known hash and hands them to the process task through a
//!   bounded queue ([`MAXIMUM_SYNC_QUEUE_SIZE`] blocks). When there is nothing new and the
//!   wallet is at the node's height it checks the sent transactions with
//!   `/get_transactions_status` (`checkLockedTransactions`): those the node no longer knows are
//!   cancelled and their inputs unlocked.
//! * the **process task** scans the blocks off the wallet lock ([`process::scan_block`]),
//!   fetches global indexes for blocks with own outputs (only for wallets that can spend, and
//!   for whole ranges of [`process::GLOBAL_INDEXES_OBSCURITY`] blocks), and applies each block
//!   under the lock ([`process::apply_block`]): new transactions and outputs, spends found
//!   through key images, and rollback when the node's chain changed.
//!
//! A reset, a node change or a failed batch bumps the state's generation; batches fetched
//! before are dropped and the fetch task starts again from the stored progress.

pub mod process;
pub mod status;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use xkr_consensus::params::DIFFICULTY_TARGET;
use xkr_daemon_client::{DaemonError, Info, NodeFee, Transport, WalletBlock, WalletSyncRequest};
use xkr_types::Hash;
use xkr_wallet_core::Address;

use crate::events::WalletEvent;
use crate::wallet::Shared;
use process::{Applied, Found};

/// `Constants::MAXIMUM_SYNC_QUEUE_SIZE`: blocks waiting to be processed at most.
pub const MAXIMUM_SYNC_QUEUE_SIZE: usize = 1000;

/// Timing of the sync engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// Pause when there is nothing new or the node does not answer (C++ `mainLoop`: 5 s).
    pub poll_interval: Duration,
    /// How often `/info` is asked while syncing (C++ `Nigel`: 10 s).
    pub info_interval: Duration,
    /// Blocks per `/getwalletsyncdata` request (the node sends at most 100).
    pub block_count: u64,
    /// Blocks waiting to be processed at most.
    pub queue_blocks: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            info_interval: Duration::from_secs(10),
            block_count: 100,
            queue_blocks: MAXIMUM_SYNC_QUEUE_SIZE,
        }
    }
}

/// What the wallet knows about its daemon (`Nigel`'s cached values).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DaemonStatus {
    /// The last `/info` answer.
    pub info: Option<Info>,
    /// The node fee from `/fee`.
    pub fee: Option<NodeFee>,
    /// Whether the last request was answered.
    pub online: bool,
    /// The last error, while offline.
    pub error: Option<String>,
}

impl DaemonStatus {
    /// `Nigel::localDaemonBlockCount`: the node's top block index.
    #[must_use]
    pub fn local_block_count(&self) -> u64 {
        self.info.as_ref().map_or(0, Info::local_block_count)
    }

    /// `Nigel::networkBlockCount`: the network's top block index as the node sees it. The C++
    /// wallets use it as the height for unlock checks and the mixin rules.
    #[must_use]
    pub fn network_block_count(&self) -> u64 {
        self.info.as_ref().map_or(0, Info::network_block_count)
    }

    /// `Nigel::peerCount`.
    #[must_use]
    pub fn peer_count(&self) -> u64 {
        self.info.as_ref().map_or(0, Info::peer_count)
    }

    /// `Nigel::hashrate`: difficulty / `DIFFICULTY_TARGET`.
    #[must_use]
    pub fn hashrate(&self) -> u64 {
        self.info
            .as_ref()
            .map_or(0, |i| i.difficulty / DIFFICULTY_TARGET)
    }

    /// `Nigel::nodeFee`: the fee every transaction pays to the node, if the node asks for one
    /// with a valid standard address.
    #[must_use]
    pub fn payable_fee(&self) -> Option<(Address, u64)> {
        self.fee.as_ref().and_then(NodeFee::payable)
    }
}

/// Blocks fetched for one generation of the state.
#[derive(Debug)]
struct Batch {
    generation: u64,
    blocks: Vec<WalletBlock>,
}

/// The running fetch and process tasks of a wallet; dropping it aborts them.
#[derive(Debug)]
pub(crate) struct SyncTasks {
    stop: watch::Sender<bool>,
    handles: Vec<JoinHandle<()>>,
}

impl SyncTasks {
    /// `WalletSynchronizer::start`.
    pub(crate) fn start<T: Transport + 'static>(shared: &Arc<Shared<T>>) -> Self {
        let (stop, stop_rx) = watch::channel(false);
        let config = &shared.config.sync;
        let per_batch = usize::try_from(config.block_count.max(1)).unwrap_or(usize::MAX);
        let capacity = (config.queue_blocks / per_batch).max(1);
        let (tx, rx) = mpsc::channel(capacity);
        let fetch = tokio::spawn(fetch_loop(Arc::clone(shared), tx, stop_rx.clone()));
        let process = tokio::spawn(process_loop(Arc::clone(shared), rx, stop_rx));
        Self {
            stop,
            handles: vec![fetch, process],
        }
    }

    /// `WalletSynchronizer::stop`: asks both tasks to stop and waits for them.
    pub(crate) async fn stop(mut self) {
        let _ = self.stop.send(true);
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
    }
}

impl Drop for SyncTasks {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// Waits `duration` or until stop is asked; true if stop was asked.
async fn pause(stop: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(duration) => *stop.borrow(),
        _ = stop.changed() => true,
    }
}

/// Asks `/info` and updates the daemon status (`Nigel::getDaemonInfo`), reporting online and
/// offline changes.
pub(crate) async fn refresh_info<T: Transport>(shared: &Shared<T>) -> Result<Info, DaemonError> {
    let node = shared.node();
    let result = node.client.info().await;
    let event = {
        let mut d = shared.daemon.write();
        let was_online = d.online;
        match &result {
            Ok(info) => {
                d.info = Some(info.clone());
                d.online = true;
                d.error = None;
                (!was_online).then_some(WalletEvent::DaemonOnline)
            }
            Err(e) => {
                d.online = false;
                d.error = Some(e.to_string());
                was_online.then(|| WalletEvent::DaemonOffline {
                    error: e.to_string(),
                })
            }
        }
    };
    if let Some(event) = event {
        shared.emit(event);
    }
    result
}

/// Asks `/fee` (`Nigel::getFeeInfo`); a node that does not answer has no fee.
pub(crate) async fn refresh_fee<T: Transport>(shared: &Shared<T>) {
    let node = shared.node();
    let fee = node.client.fee().await.ok();
    shared.daemon.write().fee = fee;
}

/// Bumps the generation if it is still `generation`, so the fetch task starts again from the
/// stored progress.
fn invalidate<T>(shared: &Shared<T>, generation: u64) {
    let mut state = shared.state.lock();
    if state.generation == generation {
        state.generation += 1;
    }
}

/// `WalletSynchronizer::mainLoop` / `downloadBlocks`.
async fn fetch_loop<T: Transport + 'static>(
    shared: Arc<Shared<T>>,
    queue: mpsc::Sender<Batch>,
    mut stop: watch::Receiver<bool>,
) {
    let config = shared.config.sync.clone();
    let mut last_info: Option<Instant> = None;
    let mut generation = u64::MAX;
    let mut last_fetched: Option<Hash> = None;
    loop {
        if *stop.borrow() {
            return;
        }
        if last_info.is_none_or(|t| t.elapsed() >= config.info_interval) {
            if refresh_info(&shared).await.is_err() {
                if pause(&mut stop, config.poll_interval).await {
                    return;
                }
                continue;
            }
            last_info = Some(Instant::now());
        }
        let (
            gen_now,
            checkpoints,
            start_height,
            start_timestamp,
            wallet_height,
            is_view,
            last_hash,
            no_addresses,
        ) = {
            let s = shared.state.lock();
            (
                s.generation,
                s.sync.checkpoints(),
                s.start_height,
                s.start_timestamp,
                s.sync.height(),
                s.is_view_wallet,
                s.sync.last_hash(),
                s.subwallets.is_empty(),
            )
        };
        if gen_now != generation {
            generation = gen_now;
            last_fetched = None;
        }
        // A walletd container without addresses has nothing to scan for (C++ stops its
        // synchronizer until an address is added).
        if no_addresses {
            if pause(&mut stop, config.poll_interval).await {
                return;
            }
            continue;
        }
        // A node behind the wallet would answer from the start height and throw away the
        // wallet's progress; wait until it catches up.
        if shared.daemon.read().local_block_count() < wallet_height {
            if pause(&mut stop, config.poll_interval).await {
                return;
            }
            last_info = None;
            continue;
        }
        let mut request_checkpoints = Vec::with_capacity(checkpoints.len() + 1);
        request_checkpoints.extend(last_fetched);
        request_checkpoints.extend(checkpoints.into_iter().filter(|c| Some(*c) != last_fetched));
        let first_request = request_checkpoints.is_empty();
        let request = WalletSyncRequest {
            checkpoints: request_checkpoints,
            start_height,
            start_timestamp,
            block_count: Some(config.block_count),
        };
        let node = shared.node();
        let mut blocks = match node.client.wallet_sync_data(&request).await {
            Ok(blocks) => blocks,
            Err(e) => {
                tracing::debug!("getwalletsyncdata: {e}");
                if e.is_unreachable() {
                    last_info = None;
                }
                if pause(&mut stop, config.poll_interval).await {
                    return;
                }
                continue;
            }
        };
        // A node answers from the block after the first known hash, except that a known
        // genesis block comes back again; it carries nothing new.
        let known = last_fetched.or(last_hash);
        if blocks
            .first()
            .is_some_and(|b| b.height == 0 && Some(b.hash) == known)
        {
            blocks.remove(0);
        }
        if blocks.is_empty() {
            let wallet_top = shared.state.lock().sync.height();
            let at_top = wallet_top >= shared.daemon.read().local_block_count();
            if at_top && !is_view {
                check_locked_transactions(&shared).await;
            }
            if pause(&mut stop, config.poll_interval).await {
                return;
            }
            last_info = None;
            continue;
        }
        // The first request (no checkpoints) must start where asked.
        if first_request && start_timestamp == 0 && blocks[0].height != start_height {
            tracing::debug!(
                "expected block {start_height} from the daemon, got {}",
                blocks[0].height
            );
            if pause(&mut stop, config.poll_interval).await {
                return;
            }
            continue;
        }
        last_fetched = blocks.last().map(|b| b.hash);
        let batch = Batch { generation, blocks };
        tokio::select! {
            sent = queue.send(batch) => if sent.is_err() { return },
            _ = stop.changed() => return,
        }
    }
}

/// `WalletSynchronizer::checkLockedTransactions`.
async fn check_locked_transactions<T: Transport>(shared: &Shared<T>) {
    let hashes = shared.state.lock().locked_transaction_hashes();
    if hashes.is_empty() {
        return;
    }
    let node = shared.node();
    let Ok(status) = node.client.transactions_status(&hashes).await else {
        return;
    };
    if status.unknown.is_empty() {
        return;
    }
    let unknown: HashSet<Hash> = status.unknown;
    let cancelled = shared.state.lock().remove_cancelled(&unknown);
    if !cancelled.is_empty() {
        shared.mark_changed(true);
    }
    for tx in cancelled {
        shared.emit(WalletEvent::Cancelled(tx.hash));
    }
}

/// Global indexes fetched during one batch, by range start.
type IndexCache = HashMap<u64, HashMap<Hash, Vec<u64>>>;

/// The process task.
async fn process_loop<T: Transport + 'static>(
    shared: Arc<Shared<T>>,
    mut queue: mpsc::Receiver<Batch>,
    mut stop: watch::Receiver<bool>,
) {
    let mut cache = IndexCache::new();
    loop {
        let batch = tokio::select! {
            batch = queue.recv() => match batch {
                Some(batch) => batch,
                None => return,
            },
            _ = stop.changed() => return,
        };
        cache.clear();
        if !process_batch(&shared, batch, &mut cache).await
            && pause(&mut stop, shared.config.sync.poll_interval).await
        {
            return;
        }
    }
}

/// Processes one batch; false if it had to be given up (it is fetched again).
async fn process_batch<T: Transport + 'static>(
    shared: &Arc<Shared<T>>,
    batch: Batch,
    cache: &mut IndexCache,
) -> bool {
    let mut pending: Vec<WalletBlock> = batch.blocks;
    while !pending.is_empty() {
        let (generation, view_secret, keys, keys_version, is_view) = {
            let s = shared.state.lock();
            (
                s.generation,
                s.view_secret,
                Arc::clone(&s.spend_keys),
                s.keys_version,
                s.is_view_wallet,
            )
        };
        if generation != batch.generation {
            return true;
        }
        let blocks = std::mem::take(&mut pending);
        let scanned = tokio::task::spawn_blocking(move || {
            let found: Vec<Vec<Found>> = blocks
                .iter()
                .map(|b| process::scan_block(&view_secret, &keys, b))
                .collect();
            (blocks, found)
        })
        .await;
        let Ok((blocks, found)) = scanned else {
            return false;
        };
        let mut items: VecDeque<(WalletBlock, Vec<Found>)> =
            blocks.into_iter().zip(found).collect();
        while let Some((block, mut found)) = items.pop_front() {
            if !is_view && !found.is_empty() {
                let resolved =
                    resolve_global_indexes(shared, block.height, &mut found, cache).await;
                if let Err(e) = resolved {
                    tracing::warn!(
                        "Failed to get correct global indexes from daemon at block {}: {e}",
                        block.height
                    );
                    invalidate(shared, batch.generation);
                    return false;
                }
            }
            let applied = {
                let mut s = shared.state.lock();
                if s.generation != batch.generation {
                    return true;
                }
                if s.keys_version == keys_version {
                    Some(process::apply_block(&mut s, &block, found))
                } else {
                    // The subwallets changed while scanning: scan the rest again.
                    pending.push(block.clone());
                    pending.extend(items.drain(..).map(|(b, _)| b));
                    None
                }
            };
            let Some(applied) = applied else {
                break;
            };
            match applied {
                Applied::Gap => {
                    invalidate(shared, batch.generation);
                    return false;
                }
                Applied::Duplicate => {}
                Applied::Block {
                    transactions,
                    rolled_back,
                } => {
                    shared.mark_changed(!transactions.is_empty());
                    if let Some(height) = rolled_back {
                        let _ = shared.block_hashes.lock().split_off(&height);
                        shared.emit(WalletEvent::Rollback { height });
                    }
                    for tx in transactions {
                        shared.emit(WalletEvent::Transaction(tx));
                    }
                    if block.height >= shared.daemon.read().network_block_count() {
                        shared.emit(WalletEvent::Synced {
                            height: block.height,
                        });
                    }
                }
            }
        }
    }
    true
}

/// `WalletSynchronizer::getGlobalIndexes` for the outputs of the block at `height`.
async fn resolve_global_indexes<T: Transport + 'static>(
    shared: &Shared<T>,
    height: u64,
    found: &mut [Found],
    cache: &mut IndexCache,
) -> Result<(), String> {
    let (start, end) = process::global_index_range(height);
    for attempt in 0..2 {
        if attempt == 1 || !cache.contains_key(&start) {
            let node = shared.node();
            let indexes = node
                .client
                .global_indexes_for_range(start, end)
                .await
                .map_err(|e| e.to_string())?;
            cache.insert(start, indexes);
        }
        if cache
            .get(&start)
            .is_some_and(|indexes| process::assign_global_indexes(found, indexes))
        {
            return Ok(());
        }
    }
    Err("a transaction or index is missing from the answer".to_owned())
}
