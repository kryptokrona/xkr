//! Solo mining against a daemon (C++ `src/miner/miner_manager.cpp` `MinerManager` and
//! `blockchain_monitor.cpp` `BlockchainMonitor`).
//!
//! `getblocktemplate` with `reserve_size` 0 for the mining address, the merge-mining tag fixed
//! ([`crate::job::set_merge_mining_tag`]), the proof-of-work blob hashed with the algorithm of
//! the block's major version, and a found block sent back with `submitblock`. Every
//! `--scan-time` the top block is read with `getlastblockheader`; a new top means a new
//! template, as does a submitted block. Works against xkrd and the C++ `kryptokronad`.

use std::time::Duration;

use xkr_daemon_client::{DaemonClient, DaemonError, HttpTransport, Transport};
use xkr_serialization::{block_hash, decode_block, encode_block};
use xkr_types::Block;
use xkr_wallet_core::Address;

use super::{JobSource, SubmitResult, Work};
use crate::job::{Found, Job, block_job, set_merge_mining_tag};

/// Wait before asking for a template again after a failure (C++ sleeps 1 s).
const RETRY: Duration = Duration::from_secs(1);

/// C++ `--first-block-timestamp` / `--block-timestamp-interval`
/// (`MinerManager::adjustBlockTemplate`): the first mined block gets `first` as timestamp,
/// every later one the previous block's timestamp plus `interval`. `first == 0` leaves the
/// template's timestamps alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timestamps {
    /// Timestamp of the first mined block (0: unchanged).
    pub first: u64,
    /// Step for every later block (0: the template's own timestamp).
    pub interval: i64,
}

impl Timestamps {
    /// The timestamp to put into a template, given the timestamp of the last block this miner
    /// submitted successfully (0: none yet). `None` keeps the template's.
    #[must_use]
    pub fn adjust(&self, last_block_timestamp: u64) -> Option<u64> {
        if self.first == 0 {
            return None;
        }
        if last_block_timestamp == 0 {
            return Some(self.first);
        }
        (self.interval != 0).then(|| last_block_timestamp.wrapping_add_signed(self.interval))
    }
}

/// Settings of [`SoloSource`].
#[derive(Debug, Clone)]
pub struct SoloConfig {
    /// The address the block rewards go to.
    pub address: Address,
    /// How often the top block is checked (C++ `--scan-time`, default 1 s).
    pub scan_time: Duration,
    /// Timestamp overrides.
    pub timestamps: Timestamps,
}

/// The template being mined.
#[derive(Debug)]
struct Template {
    id: String,
    block: Block,
}

/// Jobs from a daemon's block templates; solutions go back with `submitblock`.
#[derive(Debug)]
pub struct SoloSource<T: Transport = HttpTransport> {
    client: DaemonClient<T>,
    cfg: SoloConfig,
    current: Option<Template>,
    need_template: bool,
    next_id: u64,
    last_block_timestamp: u64,
}

impl SoloSource<HttpTransport> {
    /// A source for the daemon at `url` (`host:port`, `http://…` or `https://…`).
    pub fn connect(url: &str, cfg: SoloConfig) -> Result<Self, DaemonError> {
        Ok(Self::new(DaemonClient::connect(url)?, cfg))
    }
}

impl<T: Transport> SoloSource<T> {
    /// A source using `client`.
    #[must_use]
    pub fn new(client: DaemonClient<T>, cfg: SoloConfig) -> Self {
        Self {
            client,
            cfg,
            current: None,
            need_template: true,
            next_id: 1,
            last_block_timestamp: 0,
        }
    }

    /// C++ `MinerManager::requestMiningParameters` + `adjustBlockTemplate`: a new template as
    /// a job.
    async fn fetch(&mut self) -> Result<Job, String> {
        let t = self
            .client
            .block_template(&self.cfg.address, 0)
            .await
            .map_err(|e| e.to_string())?;
        let mut block =
            decode_block(&t.blob).map_err(|e| format!("Couldn't parse block template: {e}"))?;
        if let Some(ts) = self.cfg.timestamps.adjust(self.last_block_timestamp) {
            block.header.timestamp = ts;
        }
        set_merge_mining_tag(&mut block).map_err(|e| e.to_string())?;
        let id = self.next_id.to_string();
        self.next_id += 1;
        let job =
            block_job(&block, id.clone(), t.height, t.difficulty).map_err(|e| e.to_string())?;
        self.current = Some(Template { id, block });
        Ok(job)
    }
}

impl<T: Transport> JobSource for SoloSource<T> {
    async fn next_work(&mut self) -> Work {
        loop {
            if self.need_template {
                match self.fetch().await {
                    Ok(job) => {
                        self.need_template = false;
                        tracing::debug!(
                            height = job.height,
                            difficulty = job.target.difficulty(),
                            "new block template"
                        );
                        return Work::Job(job);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to get block template - Is your daemon open?"
                        );
                        if self.current.take().is_some() {
                            return Work::Pause(format!("no block template: {e}"));
                        }
                        tokio::time::sleep(RETRY).await;
                        continue;
                    }
                }
            }
            // C++ `BlockchainMonitor::waitBlockchainUpdate`: the old template is mined while
            // the daemon does not answer.
            tokio::time::sleep(self.cfg.scan_time).await;
            match self.client.last_block_header().await {
                Ok(top) => {
                    let mined_on = self.current.as_ref().map(|c| c.block.header.prev_hash);
                    if mined_on != Some(top.hash) {
                        self.need_template = true;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to get block hash - Is your daemon open?");
                }
            }
        }
    }

    async fn submit(&mut self, found: &Found) -> SubmitResult {
        let Some(current) = self.current.as_ref().filter(|c| c.id == found.job.id) else {
            return SubmitResult::Stale("solution of an older template".into());
        };
        let mut block = current.block.clone();
        block.header.nonce = found.nonce;
        // C++ `MinerManager::eventLoop`: a new template after every mined block.
        self.need_template = true;
        let blob = match encode_block(&block) {
            Ok(b) => b,
            Err(e) => return SubmitResult::Error(format!("cannot serialize the block: {e}")),
        };
        let hash = block_hash(&block).ok();
        match self.client.submit_block(&blob).await {
            Ok(()) => {
                self.last_block_timestamp = block.header.timestamp;
                SubmitResult::Accepted { block_hash: hash }
            }
            Err(e) if e.rpc_code().is_some() => SubmitResult::Rejected(e.to_string()),
            Err(e) => SubmitResult::Error(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_rules() {
        let none = Timestamps::default();
        assert_eq!(none.adjust(0), None);
        assert_eq!(none.adjust(5), None);
        let first = Timestamps {
            first: 1000,
            interval: 0,
        };
        assert_eq!(first.adjust(0), Some(1000));
        assert_eq!(first.adjust(1000), None);
        let step = Timestamps {
            first: 1000,
            interval: -30,
        };
        assert_eq!(step.adjust(0), Some(1000));
        assert_eq!(step.adjust(1000), Some(970));
    }
}
