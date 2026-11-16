//! xkr-miner: the Kryptokrona CPU miner as a library, used by the `xkr-miner` binary and by
//! xkrd's built-in miner (`crates/xkrd/src/miner.rs`), so one hashing loop runs everywhere.
//!
//! | Module | Content | C++ |
//! |---|---|---|
//! | [`job`] | [`Job`] (blob, nonce offset, target, height, major version), [`Target`], [`Found`], the merge-mining tag and block jobs | `src/miner/miner.cpp`, `miner_manager.cpp` `adjustMergeMiningTag` |
//! | [`hasher`] | [`HashPool`]: hashing threads with a random start and a stride, one reusable `CnHasher` each | `Miner::runWorkers` / `workerFunc` |
//! | [`source`] | [`JobSource`]: solo against a daemon, a stratum pool | `MinerManager`, `BlockchainMonitor` |
//! | [`driver`] | [`run`]: jobs to the threads, solutions to the source, reports | `MinerManager::eventLoop`, `printHashRate` |
//! | [`stats`] | [`Stats`], hashrate | `utilities::get_mining_speed` |
//!
//! Hashing uses `xkr_crypto::cryptonight::CnHasher` (a scratchpad per thread, the CPU's AES
//! instructions when it has them); the algorithm follows the block major version (CryptoNight
//! v0 for 1-3, CryptoNight-Lite v1 for 4, CryptoNight-Turtle-Lite v2 from 5 on).
#![forbid(unsafe_code)]

pub mod driver;
pub mod hasher;
pub mod job;
pub mod source;
pub mod stats;

pub use driver::{Event, MinerOptions, Observer, run};
pub use hasher::{HashConfig, HashPool};
pub use job::{
    Found, Job, JobError, PowAlgo, Target, block_job, block_nonce_offset, set_merge_mining_tag,
};
pub use source::solo::{SoloConfig, SoloSource, Timestamps};
pub use source::stratum::{StratumSource, pool_job, pool_job_on};
pub use source::{JobSource, SubmitResult, Work};
pub use stats::{Stats, format_speed};
pub use xkr_crypto::cryptonight::AesImpl;
