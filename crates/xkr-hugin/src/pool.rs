//! Where mining jobs come from and where shares go: hugin-node's `PoolConnector` (`pools.js`).
//!
//! A hugin-node logs in to a CryptoNote stratum pool with the operator's payout address, hands
//! the pool's jobs to its clients (`job_request`), and submits the shares clients attach to their
//! messages under its own login. The protocol is line-delimited JSON-RPC over plain TCP, or TLS
//! for a pool given as `stratum+ssl://host:port` (`login`, `submit`, `keepalived`, server-pushed
//! `job`).
//!
//! [`JobSource`] abstracts this so the relay runs on xkrd's p2pool client
//! ([`crate::p2pool::P2poolSource`]) and tests run without a network.
//!
//! The stratum client itself ([`StratumPool`], [`PoolAddr`], [`tls_connector`]) and the job and
//! share types live in `xkr-stratum` and are re-exported here.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::future::BoxFuture;
use tokio::sync::watch;

pub use xkr_stratum::client::{
    DEFAULT_POOL_PORT, Failover, LoginExtras, PoolAddr, PoolJob, PoolStatus, ShareSubmit,
    StratumConfig, StratumPool, SubmitOutcome, VALID_JOBS, pool_addr, tls_connector,
};

/// The node's main-chain tip: what a job is dated with (`docs/hugin-relay.md` §11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainTip {
    /// Index of the top block.
    pub height: u64,
    /// Id of the top block.
    pub hash: [u8; 32],
}

/// How many main-chain blocks a job may be behind the tip and still pay for a post: a job
/// issued for the current tip or the previous one.
pub const MAX_JOB_AGE: u64 = 1;

/// Source of mining jobs and sink for shares.
pub trait JobSource: Send + Sync + fmt::Debug {
    /// The latest job (`None` until the first one).
    fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>>;
    /// Submits one share.
    fn submit(&self, share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome>;
    /// Logged in (shares can be submitted).
    fn is_ready(&self) -> bool;
    /// Submits one share mined on `blob` (the job blob of the message). The default ignores
    /// the blob.
    fn submit_for(&self, share: ShareSubmit, _blob: &[u8]) -> BoxFuture<'static, SubmitOutcome> {
        self.submit(share)
    }
    /// Whether accepted shares are submitted at all. When not, the relay checks each share
    /// itself (as always) and stores the message without a submit.
    fn submits_shares(&self) -> bool {
        true
    }
    /// Whether `job_id` with `blob` is one of the jobs this source handed out recently. A post
    /// with another job is answered `stale_job` (no strike) and the current job. The default
    /// knows every job.
    fn job_known(&self, _job_id: &str, _blob: &[u8]) -> bool {
        true
    }
    /// The node's main-chain tip now. The relay dates every job with the tip at the time it
    /// arrives and refuses posts on a job more than [`MAX_JOB_AGE`] blocks behind
    /// (`stale_job`). `None` (the default): no tip feed, jobs are not dated.
    fn tip(&self) -> Option<ChainTip> {
        None
    }
}

/// No pool: no jobs, every submit is refused (`no_pool_connector`). The relay still stores,
/// serves and gossips messages, but cannot accept new ones from clients.
#[derive(Debug)]
pub struct NoJobSource {
    rx: watch::Receiver<Option<Arc<PoolJob>>>,
    _tx: watch::Sender<Option<Arc<PoolJob>>>,
}

impl Default for NoJobSource {
    fn default() -> Self {
        let (tx, rx) = watch::channel(None);
        Self { rx, _tx: tx }
    }
}

impl JobSource for NoJobSource {
    fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>> {
        self.rx.clone()
    }

    fn submit(&self, _share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome> {
        Box::pin(async { SubmitOutcome::rejected("no_pool_connector", None) })
    }

    fn is_ready(&self) -> bool {
        false
    }
}

/// A job source driven by hand (tests, local experiments): set jobs with
/// [`ManualJobSource::set_job`]; submits are answered with [`ManualJobSource::set_outcome`] and
/// recorded.
#[derive(Debug)]
pub struct ManualJobSource {
    tx: watch::Sender<Option<Arc<PoolJob>>>,
    outcome: Mutex<SubmitOutcome>,
    submitted: Mutex<Vec<ShareSubmit>>,
    tip: Mutex<Option<ChainTip>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for ManualJobSource {
    fn default() -> Self {
        Self {
            tx: watch::channel(None).0,
            outcome: Mutex::new(SubmitOutcome::Accepted),
            submitted: Mutex::new(Vec::new()),
            tip: Mutex::new(None),
        }
    }
}

impl ManualJobSource {
    pub fn set_job(&self, job: PoolJob) {
        self.tx.send_replace(Some(Arc::new(job)));
    }

    pub fn set_outcome(&self, outcome: SubmitOutcome) {
        *locked(&self.outcome) = outcome;
    }

    /// Sets the main-chain tip ([`JobSource::tip`]); jobs set from now on are dated with it.
    pub fn set_tip(&self, tip: Option<ChainTip>) {
        *locked(&self.tip) = tip;
    }

    /// Shares submitted so far.
    #[must_use]
    pub fn submitted(&self) -> Vec<ShareSubmit> {
        locked(&self.submitted).clone()
    }
}

impl JobSource for ManualJobSource {
    fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>> {
        self.tx.subscribe()
    }

    fn submit(&self, share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome> {
        locked(&self.submitted).push(share);
        let outcome = locked(&self.outcome).clone();
        Box::pin(async move { outcome })
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn tip(&self) -> Option<ChainTip> {
        *locked(&self.tip)
    }
}

impl JobSource for StratumPool {
    fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>> {
        StratumPool::jobs(self)
    }

    fn submit(&self, share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome> {
        StratumPool::submit(self, share)
    }

    fn submit_for(&self, share: ShareSubmit, blob: &[u8]) -> BoxFuture<'static, SubmitOutcome> {
        self.submit_for_blob(share, &hex::encode(blob))
    }

    fn is_ready(&self) -> bool {
        StratumPool::is_ready(self)
    }
}
