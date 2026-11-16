//! Built-in CPU miner (`xkrd --mine <address>` or `--mine-p2pool`, `--mine-threads N`; console
//! `start_mining` / `stop_mining`; `docs/mining.md`).
//!
//! The hashing threads and the miner loop are `xkr-miner`'s (the same code as the standalone
//! `xkr-miner` binary); this module is the [`ChainJobSource`] that feeds them from the node
//! itself. One controller thread (`xkr-miner`) runs the loop; `N` hashing threads
//! (`xkr-mine-i`) hash with the algorithm of the block major version (CN-Turtle-Lite v2 for
//! v5 and later, CN-Lite v1 for v4, CryptoNight v0 for v1-v3).
//!
//! Two modes ([`MineMode`]):
//!
//! * **solo** (the default): templates straight from the chain ([`Chain::block_template`], the
//!   code behind `getblocktemplate`) paid to the miner's address, with the merge-mining tag
//!   fixed (C++ `adjustMergeMiningTag`), refreshed when the tip changes (checked twice a
//!   second) or every 30 s (new pool transactions, timestamp). A found block goes through the
//!   same path as `submitblock` (P2P manager -> chain thread -> relay with hop 0). At trivial
//!   difficulty it waits [`MIN_BLOCK_SPACING`] between blocks so testnet blocks stay spaced
//!   out.
//! * **p2pool** (`--p2pool` running): the built-in p2pool's current template, in process
//!   ([`PoolLink`]), as the stratum server hands it to a miner: paid to the node's p2pool
//!   payout address (the fee address, as the Hugin relay's shares). The job target is the
//!   stratum template target (C++ `StratumServer::on_block`); a result at the main-chain
//!   difficulty goes to the node as a block, one at the sidechain difficulty into the sidechain
//!   (`ShareSink`, as a stratum share: "Share found" line, broadcast). One share per template:
//!   the next job comes with the template p2pool builds on its new sidechain tip. While p2pool
//!   is not running (the node is synchronizing) the miner waits.
//!
//! Both modes pause while the node is not synchronized (P2P status `downloading` or not
//! `synced`), log the hashrate every 10 s and publish [`MiningStatus`] for `/status` and the
//! console. [`MinerControl`] starts and stops the miner while the node runs.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use xkr_chain::{
    AddBlockOutcome, AddBlockResult, BlockTemplate, Chain, MinerAddress, set_merge_mining_tag,
};
use xkr_miner::{
    AesImpl, Event, Found, Job, JobSource, MinerOptions, PowAlgo, Stats, SubmitResult, Target,
    Work, block_job,
};
use xkr_p2pool::Template as PoolTemplate;
use xkr_p2pool::stratum::job::template_target;
use xkr_rpc::{MiningStatus, NetStatus, RpcCommand};
use xkr_serialization::{block_hash, encode_block};
use xkr_types::{Block, Hash, RawBlock};

use crate::fee::{DEFAULT_FEE_NAME, Fee, short_address};
use crate::p2pool::{LinkState, PoolAccess, PoolLink, difficulty_u64};

/// Template refresh interval without a new tip.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Hashrate log interval.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);
/// How often the source looks at the tip (solo) or the p2pool template (p2pool).
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Wait after a template could not be built.
const TEMPLATE_RETRY: Duration = Duration::from_secs(1);
/// Minimum time between two blocks (solo) or shares (p2pool) found by this miner. Only
/// matters at trivial difficulty (the testnet's fixed difficulty 1, where every hash is a
/// solution).
const MIN_BLOCK_SPACING: Duration = Duration::from_secs(1);
/// The extra nonce of the built-in miner's p2pool jobs. Stratum miners get theirs from a
/// random start (`StratumServer::on_block`), so the blobs differ.
pub const POOL_EXTRA_NONCE: u32 = u32::MAX;

/// Hashing threads without `--mine-threads` or a count after `start_mining`: half of the
/// available cores, at least 1.
#[must_use]
pub fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    (cores / 2).max(1)
}

/// Who p2pool shares pay, for the start of p2pool mining (at most 60 columns a line): the
/// donation default said plainly with how to change it, else the shortened payout address.
#[must_use]
pub fn p2pool_payout_lines(fee: &Fee) -> Vec<String> {
    match &fee.address {
        Some(_) if fee.donation => vec![
            "p2pool pays the node's p2pool address, now a donation".to_owned(),
            format!("to {DEFAULT_FEE_NAME}. Set yours with: fee-address <address>"),
        ],
        Some(a) => vec![format!("payout {}", short_address(a))],
        None => vec![
            "p2pool has no payout address, so no templates.".to_owned(),
            "Set one with: fee-address <address>".to_owned(),
        ],
    }
}

/// Where the miner's work comes from and who it pays.
#[derive(Debug, Clone)]
pub enum MineMode {
    /// Blocks from the node's chain, paid to this address.
    Solo {
        /// The address text (for logs and `/status`).
        address: String,
        /// Its keys.
        miner: MinerAddress,
    },
    /// Shares on the built-in p2pool, paid to the node's p2pool payout address.
    P2pool(PoolLink),
}

impl MineMode {
    /// `solo` or `p2pool` (`/status` `mining.mode`, the console).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Solo { .. } => "solo",
            Self::P2pool(_) => "p2pool",
        }
    }
}

/// Miner settings.
#[derive(Debug, Clone)]
pub struct MinerConfig {
    /// Solo to an address, or via the built-in p2pool.
    pub mode: MineMode,
    /// Hashing threads.
    pub threads: usize,
}

/// The work being hashed.
#[derive(Debug)]
enum CurrentWork {
    /// A block template of the chain (solo).
    Block(Box<SoloWork>),
    /// A p2pool template (p2pool).
    Share(Arc<PoolTemplate>),
}

/// A solo block template with its merge-mining tag set.
#[derive(Debug)]
struct SoloWork {
    block: Block,
    template: BlockTemplate,
}

/// The job being mined.
#[derive(Debug)]
struct Current {
    job_id: String,
    work: CurrentWork,
    built: Instant,
}

impl Current {
    /// The previous block of a solo template.
    fn prev_hash(&self) -> Option<Hash> {
        match &self.work {
            CurrentWork::Block(w) => Some(w.template.prev_hash),
            CurrentWork::Share(_) => None,
        }
    }
}

/// What the p2pool side offers right now.
enum PoolStep {
    /// A new job.
    Job(Job),
    /// Keep hashing what runs (or keep waiting).
    Same,
    /// Nothing to mine, for this reason.
    Wait(String),
}

/// Jobs from the node's own chain or its built-in p2pool for the `xkr-miner` loop; found
/// blocks go back through the node's `submitblock` path, found shares through p2pool's share
/// path. Keeps `/status.mining` up to date.
#[derive(Debug)]
pub struct ChainJobSource {
    cfg: MinerConfig,
    chain: Arc<Chain>,
    commands: mpsc::UnboundedSender<RpcCommand>,
    net: watch::Receiver<Arc<NetStatus>>,
    status: watch::Sender<Arc<MiningStatus>>,
    current: Option<Current>,
    next_job: u64,
    last_found: Option<Instant>,
    /// p2pool: the template the last job was made from (one share per template).
    pool_template: Option<u32>,
    /// p2pool: why the miner waits, as last said.
    waiting: Option<String>,
    stats: MiningStatus,
}

impl ChainJobSource {
    /// A source mining in `cfg.mode` on `chain`.
    #[must_use]
    pub fn new(
        cfg: MinerConfig,
        chain: Arc<Chain>,
        commands: mpsc::UnboundedSender<RpcCommand>,
        net: watch::Receiver<Arc<NetStatus>>,
        status: watch::Sender<Arc<MiningStatus>>,
    ) -> Self {
        let address = match &cfg.mode {
            MineMode::Solo { address, .. } => address.clone(),
            MineMode::P2pool(_) => String::new(),
        };
        let initial = MiningStatus {
            mode: cfg.mode.name().to_owned(),
            address,
            threads: cfg.threads.max(1),
            ..MiningStatus::default()
        };
        Self {
            cfg,
            chain,
            commands,
            net,
            status,
            current: None,
            next_job: 1,
            last_found: None,
            pool_template: None,
            waiting: None,
            stats: initial,
        }
    }

    fn behind(&self) -> bool {
        let net = self.net.borrow();
        net.downloading || !net.synced
    }

    /// The p2pool link in p2pool mode.
    fn pool_link(&self) -> Option<PoolLink> {
        match &self.cfg.mode {
            MineMode::P2pool(link) => Some(link.clone()),
            MineMode::Solo { .. } => None,
        }
    }

    fn spaced(&self) -> bool {
        self.last_found
            .is_none_or(|t| t.elapsed() >= MIN_BLOCK_SPACING)
    }

    fn needs_refresh(&self) -> bool {
        match &self.current {
            None => true,
            Some(c) => {
                c.prev_hash() != Some(self.chain.tip().hash)
                    || c.built.elapsed() >= REFRESH_INTERVAL
            }
        }
    }

    fn next_id(&mut self) -> String {
        let id = self.next_job.to_string();
        self.next_job += 1;
        id
    }

    /// Builds a new solo template and its job.
    fn refresh(&mut self) -> Result<Job, String> {
        let MineMode::Solo { miner, .. } = &self.cfg.mode else {
            return Err("not mining solo".to_owned());
        };
        let template = self.chain.block_template(miner, &[]).map_err(|e| {
            // A miner thread that outlives the stop sees the closed database.
            if matches!(&e, xkr_chain::TemplateError::Chain(c) if c.is_closed()) {
                tracing::debug!(error = %e, "no block template, the node is stopping");
            } else {
                tracing::warn!(error = %e, "cannot build a block template");
            }
            e.to_string()
        })?;
        let unhashable = || "cannot serialize the template for hashing".to_owned();
        let mut block = template.block.clone();
        set_merge_mining_tag(&mut block).map_err(|_| unhashable())?;
        let id = self.next_id();
        let job = block_job(&block, id.clone(), template.height, template.difficulty)
            .map_err(|_| unhashable())?;
        let changed = self
            .current
            .as_ref()
            .is_none_or(|c| c.prev_hash() != Some(template.prev_hash));
        if changed {
            tracing::debug!(
                height = template.height,
                difficulty = template.difficulty,
                txs = template.transactions.len(),
                "new mining job"
            );
        }
        self.stats.height = template.height;
        self.stats.difficulty = template.difficulty;
        self.current = Some(Current {
            job_id: id,
            work: CurrentWork::Block(Box::new(SoloWork { block, template })),
            built: Instant::now(),
        });
        Ok(job)
    }

    /// p2pool: a job from a template not mined yet, or why there is none.
    fn pool_step(&mut self, link: &PoolLink) -> PoolStep {
        let access = match link.state() {
            LinkState::Running(a) => a,
            LinkState::Waiting => {
                return PoolStep::Wait(
                    "waiting for p2pool (it starts once the node is synchronized)".to_owned(),
                );
            }
            LinkState::Down(reason) => return PoolStep::Wait(reason),
        };
        let Some(template) = access.current() else {
            return PoolStep::Wait("p2pool has no block template yet".to_owned());
        };
        if self.pool_template == Some(template.id) || !self.spaced() {
            return PoolStep::Same;
        }
        match self.pool_job(template) {
            Ok(job) => PoolStep::Job(job),
            Err(e) => PoolStep::Wait(e),
        }
    }

    /// The job of a p2pool template: its hashing blob with [`POOL_EXTRA_NONCE`], the stratum
    /// template target (the easier of the main-chain and the sidechain difficulty).
    fn pool_job(&mut self, template: Arc<PoolTemplate>) -> Result<Job, String> {
        let hb = template.hashing_blob(POOL_EXTRA_NONCE);
        let target = template_target(hb.mainchain_difficulty, hb.sidechain_difficulty);
        let algo = PowAlgo::for_major(template.block.major_version).unwrap_or(PowAlgo::CURRENT);
        // The blob starts with the merge-mining parent's major version.
        let major = hb.blob.first().copied().unwrap_or(0);
        let id = self.next_id();
        let job = Job::with_algo(
            id.clone(),
            hb.blob,
            hb.nonce_offset,
            Target::Share(target),
            hb.height,
            major,
            algo,
        )
        .map_err(|e| format!("p2pool job not usable: {e}"))?;
        if self.pool_template.is_none() {
            tracing::info!("mining via p2pool");
        }
        tracing::debug!(
            height = hb.height,
            sidechain_height = hb.sidechain_height,
            sidechain_difficulty = %hb.sidechain_difficulty,
            template = template.id,
            "new p2pool mining job"
        );
        self.pool_template = Some(template.id);
        self.waiting = None;
        self.stats.height = hb.height;
        self.stats.difficulty = difficulty_u64(hb.sidechain_difficulty);
        self.stats.address = template.block.miner_wallet.address();
        self.stats.last_error = None;
        self.current = Some(Current {
            job_id: id,
            work: CurrentWork::Share(template),
            built: Instant::now(),
        });
        Ok(job)
    }

    /// Same path as `submitblock`: P2P manager -> chain thread -> relay.
    async fn submit_raw(&self, block: RawBlock) -> Result<AddBlockResult, String> {
        xkr_rpc::submit_mined_block(&self.chain, &self.commands, block).await
    }

    /// A solo block found: it goes to the chain.
    async fn submit_block(&mut self, work: SoloWork, found: &Found) -> SubmitResult {
        self.last_found = Some(Instant::now());
        let SoloWork {
            mut block,
            template,
        } = work;
        block.header.nonce = found.nonce;
        let hash = block_hash(&block).unwrap_or(Hash::ZERO);
        let blob = match encode_block(&block) {
            Ok(b) => b,
            Err(e) => {
                self.stats.last_error = Some(e.to_string());
                return SubmitResult::Error(e.to_string());
            }
        };
        tracing::info!(height = template.height, %hash, nonce = found.nonce, "block found");
        let raw = RawBlock {
            block: blob,
            transactions: template.transactions,
        };
        // `current` is empty now, so the next `next_work` builds a new template.
        match self.submit_raw(raw).await {
            Ok(r) => match r.outcome {
                AddBlockOutcome::AddedToMain | AddBlockOutcome::AddedAndSwitched => {
                    self.stats.last_block_hash = Some(hash.to_string());
                    self.stats.last_error = None;
                    SubmitResult::Accepted {
                        block_hash: Some(hash),
                    }
                }
                AddBlockOutcome::AlreadyExists => SubmitResult::Stale("block already known".into()),
                other => {
                    tracing::warn!(%hash, outcome = ?other, "mined block not accepted");
                    let reason = format!("block {hash} not accepted: {other:?}");
                    self.stats.last_error = Some(reason.clone());
                    SubmitResult::Rejected(reason)
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "mined block could not be submitted");
                self.stats.last_error = Some(e.clone());
                SubmitResult::Error(e)
            }
        }
    }

    /// A p2pool result (`StratumServer::on_submit` for a local miner): a block at the
    /// main-chain difficulty goes to the node, a share at the sidechain difficulty into the
    /// sidechain. `current` is already taken.
    fn submit_pool(&mut self, template: &PoolTemplate, found: &Found) -> SubmitResult {
        let Some(link) = self.pool_link() else {
            return SubmitResult::Stale("not mining via p2pool".into());
        };
        let LinkState::Running(access) = link.state() else {
            return SubmitResult::Stale("p2pool is not running".into());
        };
        self.last_found = Some(Instant::now());
        submit_to_pool(&access, template, found)
    }

    fn publish(&self) {
        self.status.send_replace(Arc::new(self.stats.clone()));
    }
}

/// Hands a checked result to p2pool: the main-chain block first (C++ order), then the share.
fn submit_to_pool(access: &PoolAccess, template: &PoolTemplate, found: &Found) -> SubmitResult {
    let hash = Hash(found.hash);
    if template.mainchain_difficulty.check_pow(&hash) {
        tracing::info!(
            height = template.height(),
            "main-chain block found via p2pool, submitting"
        );
        access.submit_block(template.id, found.nonce, POOL_EXTRA_NONCE);
    }
    if access.submit_share(template.id, found.nonce, POOL_EXTRA_NONCE) {
        SubmitResult::Accepted { block_hash: None }
    } else {
        SubmitResult::Rejected("share not added to the sidechain".into())
    }
}

impl JobSource for ChainJobSource {
    async fn next_work(&mut self) -> Work {
        loop {
            if self.behind() {
                let had_job = self.current.take().is_some();
                if self.pool_template.take().is_some() || had_job {
                    tracing::info!("the node is not synchronized, mining paused");
                    return Work::Pause("the node is not synchronized".into());
                }
            } else if let Some(link) = self.pool_link() {
                match self.pool_step(&link) {
                    PoolStep::Job(job) => return Work::Job(job),
                    PoolStep::Same => {}
                    PoolStep::Wait(reason) => {
                        let had_job = self.current.take().is_some();
                        self.pool_template = None;
                        if had_job || self.waiting.as_deref() != Some(reason.as_str()) {
                            tracing::info!(reason = %reason, "mining paused");
                            self.waiting = Some(reason.clone());
                            self.stats.last_error = None;
                            self.publish();
                            return Work::Pause(reason);
                        }
                    }
                }
            } else if self.needs_refresh() && self.spaced() {
                match self.refresh() {
                    Ok(job) => return Work::Job(job),
                    Err(e) => {
                        self.stats.last_error = Some(e.clone());
                        self.publish();
                        if self.current.take().is_some() {
                            return Work::Pause(e);
                        }
                        tokio::time::sleep(TEMPLATE_RETRY).await;
                        continue;
                    }
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn submit(&mut self, found: &Found) -> SubmitResult {
        let Some(c) = self.current.as_ref().filter(|c| c.job_id == found.job.id) else {
            // A late solution of an older job, or of a p2pool template that has its share.
            return SubmitResult::Stale("solution of an older job".into());
        };
        if let CurrentWork::Share(t) = &c.work
            && !t.block.difficulty.check_pow(&Hash(found.hash))
        {
            // Met the 64-bit stratum target but not the exact sidechain difficulty: keep
            // hashing the same job.
            return SubmitResult::Stale("below the sidechain difficulty".into());
        }
        let Some(c) = self.current.take() else {
            return SubmitResult::Stale("solution of an older job".into());
        };
        match c.work {
            CurrentWork::Block(work) => self.submit_block(*work, found).await,
            CurrentWork::Share(template) => self.submit_pool(&template, found),
        }
    }

    fn on_stats(&mut self, stats: &Stats) {
        self.stats.hashrate = stats.hashrate;
        self.stats.total_hashes = stats.total_hashes;
        if matches!(self.cfg.mode, MineMode::P2pool(_)) {
            self.stats.shares_found = stats.accepted;
        } else {
            self.stats.blocks_found = stats.accepted;
        }
        self.stats.blocks_rejected = stats.rejected;
        self.stats.threads = stats.threads;
        self.publish();
    }
}

/// Starts the miner. Returns the controller thread (it stops the hashing threads on exit).
pub fn spawn(
    cfg: MinerConfig,
    chain: Arc<Chain>,
    commands: mpsc::UnboundedSender<RpcCommand>,
    net: watch::Receiver<Arc<NetStatus>>,
    status: watch::Sender<Arc<MiningStatus>>,
    shutdown: CancellationToken,
) -> std::io::Result<JoinHandle<()>> {
    let threads = cfg.threads.max(1);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    std::thread::Builder::new()
        .name("xkr-miner".into())
        .spawn(move || {
            let mode = cfg.mode.name();
            match &cfg.mode {
                MineMode::Solo { address, .. } => {
                    tracing::info!(mode, %address, threads, "miner started");
                }
                MineMode::P2pool(_) => tracing::info!(mode, threads, "miner started"),
            }
            let mut source = ChainJobSource::new(cfg, chain, commands, net, status);
            source.stats.active = true;
            source.publish();
            let opts = MinerOptions {
                threads,
                aes: AesImpl::detect(),
                report_interval: REPORT_INTERVAL,
                limit: 0,
                thread_name: "xkr-mine".into(),
            };
            // The hashrate every REPORT_INTERVAL while mining.
            let mut observer = |event: Event<'_>| match event {
                Event::Started { threads, aes } => {
                    tracing::debug!(threads, aes = aes.name(), "mining threads started");
                }
                Event::Report(s) if s.mining => {
                    tracing::info!(
                        mode,
                        hashrate = %format!("{:.1} H/s", s.hashrate),
                        height = s.height,
                        difficulty = s.difficulty,
                        threads = s.threads,
                        found = s.accepted,
                        "mining"
                    );
                }
                _ => {}
            };
            match runtime.block_on(xkr_miner::run(&mut source, &opts, &mut observer, &shutdown)) {
                Ok(totals) => tracing::info!(mode, found = totals.accepted, "miner stopped"),
                Err(e) => {
                    tracing::error!(error = %e, "cannot start the mining threads");
                    source.stats.last_error = Some(format!("cannot start the mining threads: {e}"));
                }
            }
            source.stats.active = false;
            source.stats.hashrate = 0.0;
            source.publish();
        })
}

/// The miner thread that runs, with its own stop.
#[derive(Debug)]
struct Running {
    stop: CancellationToken,
    thread: JoinHandle<()>,
}

/// Starts and stops the built-in miner while the node runs (`--mine`, `--mine-p2pool`, the
/// console's `start_mining` / `stop_mining`; C++ `DaemonCommandsHandler::start_mining`,
/// `stop_mining`). At most one miner runs; its stop is a child of the node's shutdown.
#[derive(Debug)]
pub struct MinerControl {
    chain: Arc<Chain>,
    commands: mpsc::UnboundedSender<RpcCommand>,
    net: watch::Receiver<Arc<NetStatus>>,
    status: watch::Sender<Arc<MiningStatus>>,
    shutdown: CancellationToken,
    /// The built-in p2pool, `None` without `--p2pool`.
    pool: Option<PoolLink>,
    running: Mutex<Option<Running>>,
}

impl MinerControl {
    /// No miner yet.
    #[must_use]
    pub fn new(
        chain: Arc<Chain>,
        commands: mpsc::UnboundedSender<RpcCommand>,
        net: watch::Receiver<Arc<NetStatus>>,
        status: watch::Sender<Arc<MiningStatus>>,
        shutdown: CancellationToken,
        pool: Option<PoolLink>,
    ) -> Self {
        Self {
            chain,
            commands,
            net,
            status,
            shutdown,
            pool,
            running: Mutex::new(None),
        }
    }

    /// The built-in p2pool's link, `None` without `--p2pool`.
    #[must_use]
    pub const fn pool(&self) -> Option<&PoolLink> {
        self.pool.as_ref()
    }

    /// Starts a miner with `cfg`. Refused while one runs, or while the node stops.
    pub fn start(&self, cfg: MinerConfig) -> Result<(), String> {
        if self.shutdown.is_cancelled() {
            return Err("the node is stopping".to_owned());
        }
        let mut running = self.lock();
        if running.as_ref().is_some_and(|r| !r.thread.is_finished()) {
            return Err("the miner runs already; stop it first with stop_mining".to_owned());
        }
        let stop = self.shutdown.child_token();
        let thread = spawn(
            cfg,
            self.chain.clone(),
            self.commands.clone(),
            self.net.clone(),
            self.status.clone(),
            stop.clone(),
        )
        .map_err(|e| format!("cannot start the miner: {e}"))?;
        *running = Some(Running { stop, thread });
        Ok(())
    }

    /// Stops the miner and waits for its threads; `false` when none ran.
    pub fn stop(&self) -> bool {
        let Some(r) = self.lock().take() else {
            return false;
        };
        let was_running = !r.thread.is_finished();
        r.stop.cancel();
        let _ = r.thread.join();
        was_running
    }

    /// The miner thread, for the node's stopping steps (the node's shutdown stops it).
    pub fn take_thread(&self) -> Option<JoinHandle<()>> {
        self.lock().take().map(|r| {
            r.stop.cancel();
            r.thread
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use xkr_serialization::block_pow_blob;
    use xkr_types::{BlockHeader, ParentBlock, Transaction};

    use super::*;

    #[test]
    fn job_nonce_offset_points_at_the_nonce() {
        let mut block = Block {
            header: BlockHeader {
                major_version: 5,
                minor_version: 0,
                timestamp: 1_700_000_000,
                prev_hash: Hash([3; 32]),
                nonce: 0,
            },
            parent_block: Some(ParentBlock {
                transaction_count: 1,
                ..ParentBlock::default()
            }),
            miner_tx: Transaction::default(),
            tx_hashes: vec![],
        };
        block.miner_tx.prefix.version = 1;
        set_merge_mining_tag(&mut block).unwrap();
        let job = block_job(&block, "1".into(), 1, 1).unwrap();
        let off = job.nonce_offset;
        block.header.nonce = 0x0403_0201;
        let blob = block_pow_blob(&block).unwrap();
        assert_eq!(&blob[off..off + 4], &[1, 2, 3, 4]);
        assert_eq!(job.blob_with_nonce(0x0403_0201), blob);
    }

    #[test]
    fn payout_lines_fit_and_say_the_donation() {
        let donation = Fee {
            address: Some(crate::fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        };
        assert_eq!(
            p2pool_payout_lines(&donation),
            [
                "p2pool pays the node's p2pool address, now a donation",
                "to nilsjr. Set yours with: fee-address <address>"
            ]
        );
        let own = Fee {
            donation: false,
            ..donation.clone()
        };
        assert_eq!(p2pool_payout_lines(&own), ["payout SEKReSw2cP…kcBJZGc"]);
        let none = Fee {
            address: None,
            donation: false,
        };
        for fee in [donation, own, none] {
            for line in p2pool_payout_lines(&fee) {
                assert!(line.chars().count() <= 60, "{line}");
            }
        }
        assert!(default_threads() >= 1);
    }

    #[test]
    fn mode_names() {
        let solo = MineMode::Solo {
            address: "SEKRx".into(),
            miner: xkr_rpc::parse_miner_address(crate::fee::DEFAULT_FEE_ADDRESS).unwrap(),
        };
        assert_eq!(solo.name(), "solo");
        assert_eq!(MineMode::P2pool(PoolLink::default()).name(), "p2pool");
        assert!(matches!(PoolLink::default().state(), LinkState::Waiting));
    }
}
