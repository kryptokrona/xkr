//! The stratum server miners connect to (`src/stratum_server.cpp`, `src/stratum_server.h`).
//!
//! Line-delimited JSON-RPC over TCP with the methods `login`, `submit`, `keepalived` and
//! `getjob`, the same answers as the C++ p2pool (`algo` `cryptonight_turtle`, 4- or 8-byte
//! targets, `seed_hash`), so xmrig, xmrig-proxy, xkr-miner and the Hugin relay mine on it
//! unchanged.
//!
//! | Module | Content (C++ mirrored) |
//! |---|---|
//! | [`job`] | constants, login parsing (user, `+N` difficulty), targets and their text, the JSON lines |
//! | [`autodiff`] | the per-miner difficulty from its share times (`update_auto_diff`) |
//! | [`stats`] | hashrate windows and share counters, the `local/stratum` API object |
//! | [`server`] | [`StratumServer`]: accept, bans, new jobs for every miner (`on_block`), share checks; [`StratumHandle`]: the same sessions on other streams (xkrd's `stratum` service over Hyperswarm) |
//!
//! The server takes its work from a [`TemplateSource`] (the block template) and hands found
//! main-chain blocks and sidechain shares to a [`ShareSink`] (the node). Every miner mines for
//! the node's own payout wallet: the address in the login is only read for a user name.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use xkr_types::Hash;

use crate::difficulty::Difficulty;

pub mod autodiff;
pub mod job;
pub mod server;
mod session;
pub mod stats;

pub use autodiff::AutoDiff;
pub use server::{StratumHandle, StratumServer, StreamEnd};
pub use stats::{StratumStats, WorkerStats};

/// Default listen address: local miners only (`--p2pool-stratum`; C++ listens on
/// `0.0.0.0:3333`).
pub const DEFAULT_STRATUM_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_STRATUM_PORT);

/// `DEFAULT_STRATUM_PORT`.
pub const DEFAULT_STRATUM_PORT: u16 = 3333;

/// `DEFAULT_STRATUM_BAN_TIME` (`src/params.h`): 600 s.
pub const DEFAULT_STRATUM_BAN_TIME: Duration = Duration::from_mins(10);

/// `STRATUM_BUF_SIZE`: the longest request line, its `\n` included.
pub const STRATUM_BUF_SIZE: usize = 1024;

/// A connection without a login this long after connecting is banned and closed
/// (`StratumServer::on_blobs_ready`).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Settings of the stratum server (`Params` stratum fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StratumServerConfig {
    /// Listen address (`--stratum`).
    pub listen: SocketAddr,
    /// How long a miner's address stays banned (`--stratum-ban-time`).
    pub ban_time: Duration,
    /// Per-miner difficulty from its share times (off with `--no-autodiff`).
    pub auto_diff: bool,
    /// Recompute the proof of work of every share, not only of sidechain shares
    /// (`--full-validation`).
    pub full_validation: bool,
    /// Answer `GET` and `HEAD` lines with a short HTTP page (off with `--no-stratum-http`).
    pub http: bool,
}

impl Default for StratumServerConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_STRATUM_ADDR,
            ban_time: DEFAULT_STRATUM_BAN_TIME,
            auto_diff: true,
            full_validation: false,
            http: true,
        }
    }
}

/// One miner's hashing blob of a block template (`BlockTemplate::get_hashing_blob`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashingBlob {
    /// The proof-of-work blob with this extra nonce in the coinbase and a zero nonce.
    pub blob: Vec<u8>,
    /// Where the 4 nonce bytes go (little-endian).
    pub nonce_offset: usize,
    /// The template this blob belongs to (its generation counter).
    pub template_id: u32,
    /// Main-chain height of the block.
    pub height: u64,
    /// Sidechain height of the share.
    pub sidechain_height: u64,
    /// Main-chain difficulty of the block.
    pub mainchain_difficulty: Difficulty,
    /// Sidechain difficulty of the share.
    pub sidechain_difficulty: Difficulty,
    /// The `seed_hash` of the job (zero for CN-Turtle).
    pub seed_hash: Hash,
}

/// The heights and difficulties of a template (`BlockTemplate::get_difficulties`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateDifficulties {
    /// Main-chain height of the block.
    pub height: u64,
    /// Sidechain height of the share.
    pub sidechain_height: u64,
    /// Main-chain difficulty of the block.
    pub mainchain_difficulty: Difficulty,
    /// Sidechain difficulty of the share.
    pub sidechain_difficulty: Difficulty,
}

impl From<&HashingBlob> for TemplateDifficulties {
    fn from(b: &HashingBlob) -> Self {
        Self {
            height: b.height,
            sidechain_height: b.sidechain_height,
            mainchain_difficulty: b.mainchain_difficulty,
            sidechain_difficulty: b.sidechain_difficulty,
        }
    }
}

/// The block template as the stratum server sees it (C++ `BlockTemplate`, `SideChain`,
/// `P2PServer::disconnected`). Implemented by the node's template code; the methods are called
/// from tokio tasks and from blocking threads and should return quickly.
pub trait TemplateSource: Send + Sync {
    /// The current template's hashing blob for `extra_nonce`; `None` while there is no
    /// template (the node is not synchronized yet).
    fn hashing_blob(&self, extra_nonce: u32) -> Option<HashingBlob>;

    /// `BlockTemplate::get_hashing_blobs`: `count` blobs of one template, for the extra nonces
    /// `extra_nonce_start`, `extra_nonce_start + 1`, ... (wrapping). The default asks
    /// [`Self::hashing_blob`] for each and gives up (empty) when the template changes in
    /// between.
    fn hashing_blobs(&self, extra_nonce_start: u32, count: u32) -> Vec<HashingBlob> {
        let mut out: Vec<HashingBlob> = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(b) = self.hashing_blob(extra_nonce_start.wrapping_add(i)) else {
                return Vec::new();
            };
            if out.first().is_some_and(|f| f.template_id != b.template_id) {
                return Vec::new();
            }
            out.push(b);
        }
        out
    }

    /// The hashing blob of an earlier template still kept, `None` once it is gone
    /// (`BlockTemplate::get_hashing_blob(template_id, extra_nonce, ...)`).
    fn hashing_blob_for(&self, template_id: u32, extra_nonce: u32) -> Option<HashingBlob>;

    /// Heights and difficulties of a kept template, `None` once it is gone (then a share for
    /// it is stale).
    fn difficulties(&self, template_id: u32) -> Option<TemplateDifficulties>;

    /// The sidechain's current difficulty (`SideChain::difficulty`), for effort and the worker
    /// list.
    fn sidechain_difficulty(&self) -> Difficulty;

    /// Height of the sidechain tip, `None` without one (`SideChain::chainTip`).
    fn sidechain_tip_height(&self) -> Option<u64>;

    /// The PPLNS window (`SideChain::chain_window_size`): a share whose template is this far
    /// below the tip does not go to the sidechain.
    fn chain_window_size(&self) -> u64;

    /// The P2P side had peers and has none now: logins are refused
    /// (`P2PServer::disconnected`).
    fn disconnected(&self) -> bool {
        false
    }
}

/// Where found work goes (C++ `p2pool::submit_block_async`, `p2pool::submit_sidechain_block`).
pub trait ShareSink: Send + Sync {
    /// The result meets the main-chain difficulty: build the block of `template_id` with this
    /// nonce and extra nonce and submit it. Must not block (queue the work); the node checks
    /// the block itself.
    fn submit_block(&self, template_id: u32, nonce: u32, extra_nonce: u32);

    /// A checked share that meets the sidechain difficulty: add it to the sidechain and
    /// broadcast it. `false` when that failed. Called on a blocking thread.
    fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool;
}
