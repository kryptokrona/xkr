//! Pool mining over stratum (`xkr_stratum::StratumPool`: `login`, pushed `job`s, `submit`,
//! `keepalived`, TLS, fail-over; the protocol xmrig and the cryptonote pools speak).
//!
//! A pool job is `{ job_id, blob, target, height?, algo? }`. The blob is the hashing blob of the
//! merge-mining parent block (its first byte is the parent's major version, 0 from p2pool, not
//! the Kryptokrona block's): the nonce goes at the usual offset after the three header varints
//! and `prev_id` ([`xkr_stratum::blob::nonce_offset`]). The algorithm is the job's `algo`
//! (p2pool sends `cryptonight_turtle`), else the one of the block at its `height` on the
//! network, else the current one (CN-Turtle-Lite v2). A share must meet the compact target
//! ([`xkr_stratum::target::parse_target`]); it goes back with the 4 nonce bytes and the hash as
//! hex.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xkr_consensus::Network;
use xkr_stratum::blob::{major_version, nonce_offset};
use xkr_stratum::target::parse_target;
use xkr_stratum::{LoginExtras, PoolJob, ShareSubmit, StratumConfig, StratumPool, SubmitOutcome};

use super::{JobSource, SubmitResult, Work};
use crate::job::{Found, Job, PowAlgo, Target};

/// A mainnet pool job as a [`Job`] ([`pool_job_on`]).
pub fn pool_job(pj: &PoolJob) -> Result<Job, String> {
    pool_job_on(pj, Network::Mainnet)
}

/// A pool job of `network` as a [`Job`]; the reason when it cannot be mined (an `algo` this
/// miner does not have is refused rather than hashed with another one).
pub fn pool_job_on(pj: &PoolJob, network: Network) -> Result<Job, String> {
    let blob = hex::decode(&pj.blob).map_err(|e| format!("job blob: {e}"))?;
    let target = parse_target(&pj.target).ok_or_else(|| format!("job target {:?}", pj.target))?;
    let major = major_version(&blob).ok_or("job blob without a major version")?;
    let height = pj.raw.get("height").and_then(Value::as_u64).unwrap_or(0);
    let algo = match pj.raw.get("algo").and_then(Value::as_str) {
        Some(name) => {
            PowAlgo::from_name(name).ok_or_else(|| format!("unsupported algorithm {name:?}"))?
        }
        None if height > 0 => PowAlgo::for_height(network, height),
        None => PowAlgo::CURRENT,
    };
    let offset = nonce_offset(&blob);
    Job::with_algo(
        pj.job_id.clone(),
        blob,
        offset,
        Target::Share(target),
        height,
        major,
        algo,
    )
    .map_err(|e| e.to_string())
}

/// Jobs from a stratum pool; solutions are submitted to it as shares.
#[derive(Debug)]
pub struct StratumSource {
    pool: Arc<StratumPool>,
    /// Network of the jobs (their algorithm by height when they do not name one).
    network: Network,
    jobs: watch::Receiver<Option<Arc<PoolJob>>>,
    /// The value present at the start has been looked at.
    primed: bool,
    /// The pool task ended (no more jobs).
    ended: bool,
    has_job: bool,
}

impl StratumSource {
    /// Starts the pool client (needs a tokio runtime); it stops with `shutdown`.
    #[must_use]
    pub fn spawn(cfg: StratumConfig, extras: LoginExtras, shutdown: CancellationToken) -> Self {
        Self::new(StratumPool::spawn_with(cfg, extras, shutdown))
    }

    /// A source on a running pool client.
    #[must_use]
    pub fn new(pool: Arc<StratumPool>) -> Self {
        let jobs = pool.jobs();
        Self {
            pool,
            network: Network::Mainnet,
            jobs,
            primed: false,
            ended: false,
            has_job: false,
        }
    }

    /// Jobs are of `network` (mainnet by default).
    #[must_use]
    pub fn on_network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    /// The pool client.
    #[must_use]
    pub fn pool(&self) -> &Arc<StratumPool> {
        &self.pool
    }
}

impl JobSource for StratumSource {
    async fn next_work(&mut self) -> Work {
        loop {
            if self.ended {
                std::future::pending::<()>().await;
            }
            if self.primed && self.jobs.changed().await.is_err() {
                self.ended = true;
                if self.has_job {
                    self.has_job = false;
                    return Work::Pause("the pool client stopped".into());
                }
                continue;
            }
            self.primed = true;
            let latest = self.jobs.borrow_and_update().clone();
            match latest {
                None => {
                    if self.has_job {
                        self.has_job = false;
                        return Work::Pause("no job from the pool".into());
                    }
                }
                Some(pj) => match pool_job_on(&pj, self.network) {
                    Ok(job) => {
                        self.has_job = true;
                        return Work::Job(job);
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_id = %pj.job_id,
                            error = %e,
                            "unusable job from the pool"
                        );
                    }
                },
            }
        }
    }

    async fn submit(&mut self, found: &Found) -> SubmitResult {
        let share = ShareSubmit {
            job_id: found.job.id.clone(),
            nonce: found.nonce_hex(),
            result: found.hash_hex(),
        };
        match self.pool.submit(share).await {
            SubmitOutcome::Accepted => SubmitResult::Accepted { block_hash: None },
            SubmitOutcome::Duplicate => SubmitResult::Rejected("duplicate share".into()),
            SubmitOutcome::Rejected { reason, error } => match reason.as_str() {
                "stale_job" => SubmitResult::Stale("job no longer valid at the pool".into()),
                "not_logged_in" | "socket_not_writable" | "timeout" => SubmitResult::Error(reason),
                _ => SubmitResult::Rejected(error.unwrap_or(reason)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn pool_jobs() {
        let blob = format!(
            "0700{}{}{}{}",
            "01",
            "33".repeat(32),
            "00000000",
            "44".repeat(36)
        );
        let pj = PoolJob::from_value(json!({
            "job_id": "j1", "blob": blob, "target": "285c8f02", "height": 12
        }))
        .unwrap();
        let job = pool_job(&pj).unwrap();
        assert_eq!(job.id, "j1");
        assert_eq!(job.nonce_offset, 35);
        assert_eq!(job.major, 7);
        // no `algo`: the block at height 12 (mainnet major 5 from height 5 on)
        assert_eq!(job.algo, PowAlgo::CnTurtleLiteV2);
        assert_eq!(job.height, 12);
        assert_eq!(job.target.difficulty(), 100);
        let short_blob = format!("0700{}{}", "01", "33".repeat(32));
        let short = PoolJob::from_value(json!({
            "job_id": "j2", "blob": short_blob, "target": "ffffffff"
        }))
        .unwrap();
        assert!(pool_job(&short).is_err(), "no room for the nonce");
        let zero = PoolJob::from_value(json!({
            "job_id": "j3", "blob": blob, "target": "00000000"
        }))
        .unwrap();
        assert!(pool_job(&zero).is_err());
    }

    /// A job as p2pool's `StratumServer` sends it (`stratum_server.cpp`: `blob`, `job_id`,
    /// `target`, `algo: "cryptonight_turtle"`, `height`, `seed_hash`): the blob is the
    /// merge-mining parent's hashing blob with major version 0 (major, minor, timestamp
    /// varint, `prev_id`, nonce at 39, Merkle root at 43, transaction count).
    fn p2pool_job() -> Value {
        let blob = format!(
            "0000{}{}{}{}{}",
            "e0d1c4c906",
            "5b".repeat(32),
            "00000000",
            "a7".repeat(32),
            "02"
        );
        json!({
            "blob": blob,
            "job_id": "1",
            "target": "b88d0600",
            "algo": "cryptonight_turtle",
            "height": 21,
            "seed_hash": "0000000000000000000000000000000000000000000000000000000000000000"
        })
    }

    #[test]
    fn p2pool_jobs_hash_with_the_algorithm_they_name() {
        let pj = PoolJob::from_value(p2pool_job()).unwrap();
        // the parent's major version 0 has no algorithm of its own
        assert!(Job::new("x".into(), vec![0; 76], 39, Target::Share(1), 0, 0).is_err());
        let job = pool_job_on(&pj, Network::Testnet).unwrap();
        assert_eq!(job.major, 0);
        assert_eq!(job.algo(), PowAlgo::CnTurtleLiteV2);
        assert_eq!(job.nonce_offset, 39);
        assert_eq!(job.height, 21);
        assert_eq!(job.blob.len(), 76);
        // the nonce lands where p2pool puts it (before the Merkle root at 43)
        let with_nonce = job.blob_with_nonce(0x0403_0201);
        assert_eq!(&with_nonce[39..43], &[1, 2, 3, 4]);
        assert_eq!(&with_nonce[43..75], job.blob[43..75].to_vec().as_slice());

        // without `algo`: by height, else the current algorithm
        let mut raw = p2pool_job();
        raw.as_object_mut().unwrap().remove("algo");
        let by_height = pool_job_on(&PoolJob::from_value(raw.clone()).unwrap(), Network::Mainnet);
        assert_eq!(by_height.unwrap().algo(), PowAlgo::CnTurtleLiteV2);
        raw.as_object_mut().unwrap().remove("height");
        let current = pool_job(&PoolJob::from_value(raw).unwrap()).unwrap();
        assert_eq!(current.algo(), PowAlgo::CURRENT);
        assert_eq!(current.height, 0);

        // an algorithm this miner does not have is refused
        let mut raw = p2pool_job();
        raw["algo"] = json!("rx/0");
        let e = pool_job(&PoolJob::from_value(raw).unwrap()).unwrap_err();
        assert!(e.contains("rx/0"), "{e}");
    }

    #[test]
    fn algorithm_names() {
        for name in [
            "cryptonight_turtle",
            "cn-pico/trtl",
            "CN/TURTLE",
            " cn-turtle-lite/2 ",
        ] {
            assert_eq!(
                PowAlgo::from_name(name),
                Some(PowAlgo::CnTurtleLiteV2),
                "{name}"
            );
        }
        assert_eq!(PowAlgo::from_name("cn-lite/1"), Some(PowAlgo::CnLiteV1));
        assert_eq!(PowAlgo::from_name("cn/0"), Some(PowAlgo::CnV0));
        assert_eq!(PowAlgo::from_name("rx/0"), None);
        assert_eq!(PowAlgo::from_name("cryptonight"), None);
        // the fork table: mainnet heights 2-4 are majors 2-4, then 5 and later
        assert_eq!(PowAlgo::for_height(Network::Mainnet, 3), PowAlgo::CnV0);
        assert_eq!(PowAlgo::for_height(Network::Mainnet, 4), PowAlgo::CnLiteV1);
        assert_eq!(
            PowAlgo::for_height(Network::Mainnet, 2_620_300),
            PowAlgo::CnTurtleLiteV2
        );
        assert_eq!(
            PowAlgo::for_height(Network::Testnet, 1),
            PowAlgo::CnTurtleLiteV2
        );
        for algo in [PowAlgo::CnV0, PowAlgo::CnLiteV1, PowAlgo::CnTurtleLiteV2] {
            assert_eq!(PowAlgo::from_name(algo.name()), Some(algo));
        }
    }
}
