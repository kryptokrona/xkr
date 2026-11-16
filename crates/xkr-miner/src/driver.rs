//! The miner loop (C++ `src/miner/miner_manager.cpp` `MinerManager::eventLoop`): work from a
//! [`JobSource`] to the hashing threads, their solutions back to the source, and statistics.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xkr_crypto::cryptonight::AesImpl;

use crate::hasher::{HashConfig, HashPool};
use crate::job::{Found, Job};
use crate::source::{JobSource, SubmitResult, Work};
use crate::stats::{RateMeter, Stats};

/// Settings of [`run`].
#[derive(Debug, Clone)]
pub struct MinerOptions {
    /// Hashing threads.
    pub threads: usize,
    /// AES implementation of the hashers.
    pub aes: AesImpl,
    /// How often the hashrate is measured and reported.
    pub report_interval: Duration,
    /// Stop after this many accepted solutions (blocks solo, shares in a pool); 0 is no limit
    /// (C++ `--limit`).
    pub limit: u64,
    /// Hashing threads are named `<prefix>-<index>`.
    pub thread_name: String,
}

impl MinerOptions {
    /// `threads` threads, [`AesImpl::detect`], a report every 60 s (C++ `printHashRate`), no
    /// limit.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        Self {
            threads,
            aes: AesImpl::detect(),
            report_interval: Duration::from_mins(1),
            limit: 0,
            thread_name: "xkr-mine".into(),
        }
    }
}

/// What happened, for logs and status output.
#[derive(Debug)]
pub enum Event<'a> {
    /// The threads are running.
    Started {
        /// Hashing threads.
        threads: usize,
        /// AES implementation.
        aes: AesImpl,
    },
    /// A new job is being hashed.
    Job(&'a Job),
    /// Hashing stopped until the next job.
    Paused(&'a str),
    /// The periodic report.
    Report(&'a Stats),
    /// A solution was handed to the source.
    Submitted {
        /// The solution.
        found: &'a Found,
        /// What became of it.
        result: &'a SubmitResult,
        /// The statistics after it.
        stats: &'a Stats,
    },
    /// `limit` solutions were accepted; the miner stops.
    LimitReached(u64),
    /// The miner stopped.
    Stopped(&'a Stats),
}

/// Receives [`Event`]s.
pub trait Observer {
    /// One event.
    fn event(&mut self, event: Event<'_>);
}

impl<F: FnMut(Event<'_>)> Observer for F {
    fn event(&mut self, event: Event<'_>) {
        self(event);
    }
}

/// Mines with `opts.threads` threads on the work of `source` until `shutdown` or the limit.
/// Returns the final statistics; an error only when the threads cannot be started.
pub async fn run<S: JobSource, O: Observer>(
    source: &mut S,
    opts: &MinerOptions,
    observer: &mut O,
    shutdown: &CancellationToken,
) -> io::Result<Stats> {
    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<Found>();
    let pool = HashPool::start(
        &HashConfig {
            threads: opts.threads,
            aes: opts.aes,
            thread_name: opts.thread_name.clone(),
        },
        found_tx,
    )?;
    let mut stats = Stats {
        threads: pool.threads(),
        ..Stats::default()
    };
    let mut current: Option<Arc<Job>> = None;
    let mut meter = RateMeter::new(0);
    let period = opts.report_interval.max(Duration::from_millis(100));
    let mut report = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    report.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    observer.event(Event::Started {
        threads: pool.threads(),
        aes: pool.aes(),
    });
    source.on_stats(&stats);

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(found) = found_rx.recv() => {
                if found.job.target.is_block() {
                    // Only the first solution of the current block job counts; the threads
                    // stop hashing it at once (C++ `Miner::setStateBlockFound`).
                    if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &found.job)) {
                        continue;
                    }
                    pool.set_job(None);
                    current = None;
                    stats.mining = false;
                }
                let result = source.submit(&found).await;
                match &result {
                    SubmitResult::Accepted { block_hash } => {
                        stats.accepted += 1;
                        if let Some(h) = block_hash {
                            stats.last_block_hash = Some(h.to_string());
                        }
                    }
                    SubmitResult::Rejected(reason) => {
                        stats.rejected += 1;
                        stats.last_error = Some(reason.clone());
                    }
                    SubmitResult::Stale(_) => stats.stale += 1,
                    SubmitResult::Error(e) => {
                        stats.errors += 1;
                        stats.last_error = Some(e.clone());
                    }
                }
                stats.total_hashes = pool.hashes();
                observer.event(Event::Submitted {
                    found: &found,
                    result: &result,
                    stats: &stats,
                });
                source.on_stats(&stats);
                if opts.limit != 0 && stats.accepted >= opts.limit {
                    observer.event(Event::LimitReached(opts.limit));
                    break;
                }
            }
            work = source.next_work() => {
                match work {
                    Work::Job(job) => {
                        let job = Arc::new(job);
                        stats.mining = true;
                        stats.height = job.height;
                        stats.difficulty = job.target.difficulty();
                        stats.job_id = Some(job.id.clone());
                        pool.set_job(Some(job.clone()));
                        observer.event(Event::Job(&job));
                        current = Some(job);
                    }
                    Work::Pause(reason) => {
                        pool.set_job(None);
                        current = None;
                        stats.mining = false;
                        observer.event(Event::Paused(&reason));
                    }
                }
                source.on_stats(&stats);
            }
            _ = report.tick() => {
                let total = pool.hashes();
                stats.hashrate = meter.update(total);
                stats.total_hashes = total;
                observer.event(Event::Report(&stats));
                source.on_stats(&stats);
            }
        }
    }

    pool.stop();
    stats.mining = false;
    stats.hashrate = 0.0;
    source.on_stats(&stats);
    observer.event(Event::Stopped(&stats));
    Ok(stats)
}
