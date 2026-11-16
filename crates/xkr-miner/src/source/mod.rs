//! Where jobs come from and where solutions go.
//!
//! | Source | Jobs | Solutions |
//! |---|---|---|
//! | [`solo::SoloSource`] | `getblocktemplate` from a daemon (xkrd or C++ `kryptokronad`), refreshed when `getlastblockheader` shows a new top block (C++ `src/miner/`) | `submitblock` |
//! | [`stratum::StratumSource`] | a stratum pool's `job`s | `submit` |
//! | xkrd's `ChainJobSource` (`crates/xkrd/src/miner.rs`) | the node's own chain | the node's `submitblock` path |
//!
//! The miner loop ([`crate::run`]) asks the source for work and hands it solutions; the source
//! decides when the work changes.

pub mod solo;
pub mod stratum;

use std::future::Future;

use xkr_types::Hash;

use crate::job::{Found, Job};
use crate::stats::Stats;

/// What the hashing threads should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    /// Hash this job (replaces the previous one).
    Job(Job),
    /// Stop hashing until the next job, for this reason (daemon unreachable, node not
    /// synchronized, no job from the pool).
    Pause(String),
}

/// What became of a solution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitResult {
    /// Taken: a block on the main chain (with its hash), or a share by the pool.
    Accepted {
        /// The block hash, for a block.
        block_hash: Option<Hash>,
    },
    /// Refused by the node or the pool (its reason).
    Rejected(String),
    /// The job was no longer current (not submitted, already known, or a stale pool job).
    Stale(String),
    /// Could not be submitted (no answer).
    Error(String),
}

/// A source of jobs and a sink for their solutions.
///
/// [`JobSource::next_work`] is polled in a `select!` with the solutions and the report timer,
/// so it must be cancellation-safe: dropped at an `.await`, the next call starts over without
/// losing state.
pub trait JobSource: Send {
    /// Waits until the work changes and returns it. The first call returns the first job (or
    /// a pause) as soon as there is one.
    fn next_work(&mut self) -> impl Future<Output = Work> + Send;

    /// Submits a solution of a job this source issued.
    fn submit(&mut self, found: &Found) -> impl Future<Output = SubmitResult> + Send;

    /// The statistics after every job change, solution and report (xkrd publishes them as
    /// `/status.mining`). Nothing by default.
    fn on_stats(&mut self, stats: &Stats) {
        let _ = stats;
    }
}
