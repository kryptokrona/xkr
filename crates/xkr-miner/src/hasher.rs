//! The hashing threads (C++ `src/miner/miner.cpp` `Miner::runWorkers` / `workerFunc`, xkrd's
//! built-in miner).
//!
//! `N` threads hash the current job. Each starts at a random nonce per job and steps by `N`
//! (thread `i` starts at `random * N + i`), keeps one [`CnHasher`] per algorithm (no allocation
//! per hash), writes its nonce into a private copy of the blob and reports every hash that
//! reaches the target. After a block solution a thread waits for the next job; after a share
//! it keeps going. The job is swapped through a generation counter the threads read before
//! every hash.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use rand_core::RngCore;
use tokio::sync::mpsc;
use xkr_crypto::cryptonight::{AesImpl, CnHasher};

use crate::job::{Found, Job, PowAlgo};

/// How long an idle thread sleeps before it looks for a job again.
const IDLE: Duration = Duration::from_millis(50);

/// Settings of [`HashPool`].
#[derive(Debug, Clone)]
pub struct HashConfig {
    /// Hashing threads (at least 1).
    pub threads: usize,
    /// AES implementation of the hashers.
    pub aes: AesImpl,
    /// Thread names are `<prefix>-<index>`.
    pub thread_name: String,
}

impl HashConfig {
    /// `threads` threads with [`AesImpl::detect`], named `xkr-mine-<i>`.
    #[must_use]
    pub fn new(threads: usize) -> Self {
        Self {
            threads,
            aes: AesImpl::detect(),
            thread_name: "xkr-mine".into(),
        }
    }
}

/// State shared with the threads.
#[derive(Debug, Default)]
struct Shared {
    job: RwLock<Option<Arc<Job>>>,
    generation: AtomicU64,
    hashes: AtomicU64,
    stop: AtomicBool,
}

/// The hashing threads. Dropping it stops them without waiting; [`HashPool::stop`] waits.
#[derive(Debug)]
pub struct HashPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
    aes: AesImpl,
}

impl HashPool {
    /// Starts the threads, idle until [`HashPool::set_job`]. Solutions go to `found`.
    pub fn start(cfg: &HashConfig, found: mpsc::UnboundedSender<Found>) -> io::Result<Self> {
        let threads = cfg.threads.max(1);
        let shared = Arc::new(Shared::default());
        let mut pool = Self {
            shared: shared.clone(),
            workers: Vec::with_capacity(threads),
            aes: cfg.aes,
        };
        for i in 0..threads {
            let shared = shared.clone();
            let found = found.clone();
            let aes = cfg.aes;
            let handle = std::thread::Builder::new()
                .name(format!("{}-{i}", cfg.thread_name))
                .spawn(move || worker(&shared, &found, i as u32, threads as u32, aes))?;
            pool.workers.push(handle);
        }
        // Only the threads hold senders now: the channel closes when they are gone.
        drop(found);
        Ok(pool)
    }

    /// Hands `job` to the threads (`None` makes them idle).
    pub fn set_job(&self, job: Option<Arc<Job>>) {
        *self
            .shared
            .job
            .write()
            .unwrap_or_else(PoisonError::into_inner) = job;
        self.shared.generation.fetch_add(1, Ordering::Release);
    }

    /// Hashes since start.
    #[must_use]
    pub fn hashes(&self) -> u64 {
        self.shared.hashes.load(Ordering::Relaxed)
    }

    /// Number of threads.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.workers.len()
    }

    /// The AES implementation of the hashers.
    #[must_use]
    pub fn aes(&self) -> AesImpl {
        self.aes
    }

    /// Stops the threads and waits for them (each finishes the hash it is on).
    pub fn stop(mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for HashPool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// One hashing thread: nonces `start, start + step, ...` of the current job.
fn worker(
    shared: &Shared,
    found: &mpsc::UnboundedSender<Found>,
    index: u32,
    step: u32,
    aes: AesImpl,
) {
    let mut rng = xkr_crypto::os_rng();
    let mut seen = u64::MAX;
    let mut job: Option<Arc<Job>> = None;
    let mut hasher: Option<(PowAlgo, CnHasher)> = None;
    let mut blob = Vec::new();
    let mut nonce = 0u32;
    // A block job this thread solved: wait for the next one.
    let mut solved = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let generation = shared.generation.load(Ordering::Acquire);
        if generation != seen {
            seen = generation;
            job.clone_from(&shared.job.read().unwrap_or_else(PoisonError::into_inner));
            solved = false;
            if let Some(j) = &job {
                blob.clone_from(&j.blob);
                nonce = rng.next_u32().wrapping_mul(step).wrapping_add(index);
                let algo = j.algo();
                if hasher.as_ref().is_none_or(|(a, _)| *a != algo) {
                    hasher = Some((algo, algo.hasher(aes)));
                }
            }
        }
        let (Some(j), Some((_, h))) = (&job, &mut hasher) else {
            std::thread::sleep(IDLE);
            continue;
        };
        if solved {
            std::thread::sleep(IDLE);
            continue;
        }
        blob[j.nonce_offset..j.nonce_offset + 4].copy_from_slice(&nonce.to_le_bytes());
        let Ok(hash) = h.hash(&blob) else {
            // `Job::new` checks the length the algorithm needs; nothing to hash here.
            solved = true;
            continue;
        };
        shared.hashes.fetch_add(1, Ordering::Relaxed);
        if j.target.is_met(&hash) {
            let f = Found {
                job: j.clone(),
                nonce,
                hash,
            };
            if found.send(f).is_err() {
                return;
            }
            solved = j.target.is_block();
        }
        nonce = nonce.wrapping_add(step);
    }
}

#[cfg(test)]
mod tests {
    use crate::job::Target;

    use super::*;

    /// Two threads on a share job with the easiest target: every hash is a share, nonces are
    /// distinct and every reported hash is the hash of its nonce; a block job yields one
    /// solution per thread and then the threads wait.
    #[test]
    fn shares_and_blocks() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cfg = HashConfig {
            threads: 2,
            aes: AesImpl::detect(),
            thread_name: "test-mine".into(),
        };
        let pool = HashPool::start(&cfg, tx).unwrap();
        assert_eq!(pool.threads(), 2);
        let job =
            Arc::new(Job::new("s".into(), vec![7; 76], 39, Target::Share(u64::MAX), 1, 7).unwrap());
        pool.set_job(Some(job.clone()));
        let mut got = Vec::new();
        while got.len() < 6 {
            got.push(rx.blocking_recv().unwrap());
        }
        pool.set_job(None);
        let mut nonces: Vec<u32> = got.iter().map(|f| f.nonce).collect();
        nonces.sort_unstable();
        nonces.dedup();
        assert_eq!(nonces.len(), 6);
        for f in &got {
            assert_eq!(f.hash, job.hash(f.nonce, AesImpl::Software));
        }

        let block =
            Arc::new(Job::new("b".into(), vec![7; 76], 39, Target::Block(1), 1, 7).unwrap());
        pool.set_job(Some(block.clone()));
        // Shares of the old job may still arrive from hashes that were under way.
        let mut solutions = 0;
        while solutions < 2 {
            let f = rx.blocking_recv().unwrap();
            if Arc::ptr_eq(&f.job, &block) {
                solutions += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
        while let Ok(f) = rx.try_recv() {
            assert!(
                !Arc::ptr_eq(&f.job, &block),
                "threads stop after a block solution"
            );
        }
        assert!(pool.hashes() >= 8);
        pool.stop();
    }
}
