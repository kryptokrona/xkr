//! Fork status and the block-rate monitor (`docs/forks.md`): what `/getinfo`
//! (`fork_readiness`) and `/status` (`fork`, `block_rate`) report and what the P2P manager logs.
//!
//! * **Readiness**: the share of the last [`FORK_READINESS_WINDOW`] main-chain blocks whose
//!   minor version signals the next block-version fork
//!   ([`xkr_consensus::forks::signals_fork`]). Reading the minor version needs the block blob,
//!   so the count is cached per tip (one recount of at most 720 headers per new block, nothing
//!   between blocks).
//! * **Block rate** (Monero's `block_rate_notify`): blocks whose timestamp lies in the last hour
//!   against the expected `3600 / DIFFICULTY_TARGET` = 40.

use xkr_consensus::forks::{FORK_READINESS_WINDOW, signals_fork};
use xkr_consensus::params::DIFFICULTY_TARGET;
use xkr_serialization::varint::read_varint;
use xkr_types::Hash;

use crate::chain::Chain;
use crate::error::ChainError;

/// Seconds the block-rate monitor looks back.
pub const BLOCK_RATE_WINDOW_SECS: u64 = 3600;
/// Below `expected / BLOCK_RATE_FACTOR` blocks per window the rate is [`BlockRateStatus::Low`],
/// above `expected * BLOCK_RATE_FACTOR` it is [`BlockRateStatus::High`]. With 40 expected blocks
/// (Poisson) a count below 13 or above 120 is practically never chance: a hashrate jump or drop
/// beyond what LWMA-3 follows within the hour, timestamp skew, or a network split.
pub const BLOCK_RATE_FACTOR: u64 = 3;
/// Most headers the monitor reads (bounds the work when blocks come much faster than expected).
const BLOCK_RATE_MAX_SCAN: u64 = BLOCK_RATE_WINDOW_SECS / DIFFICULTY_TARGET * BLOCK_RATE_FACTOR * 4;

/// Cached readiness count (see the module docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SignalCache {
    tip: Hash,
    target: u8,
    window: u64,
    signalling: u64,
}

/// Fork state of the main chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForkStatus {
    /// Height (top index) of the main-chain tip the status describes.
    pub tip_height: u64,
    /// Block major version of the next block.
    pub next_block_major_version: u8,
    /// Highest fork index in force for the next block (`None` below the first fork).
    pub active_index: Option<u8>,
    /// Next scheduled fork, if any.
    pub next_index: Option<u8>,
    /// Its height `F` (block `F + 1` is the first under the new rules).
    pub next_height: Option<u64>,
    /// Highest fork index this software supports.
    pub supported_index: u8,
    /// Block-version fork the readiness counts signals for (scheduled or not).
    pub signal_index: Option<u8>,
    /// Blocks in the readiness window (at most [`FORK_READINESS_WINDOW`]).
    pub window: u64,
    /// Blocks of the window that signal [`Self::signal_index`].
    pub signalling: u64,
    /// `signalling / window`, `None` without a target or an empty window.
    pub readiness: Option<f64>,
}

/// [`BlockRate::status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRateStatus {
    /// Within the expected range.
    Normal,
    /// Far fewer blocks than expected.
    Low,
    /// Far more blocks than expected.
    High,
    /// The network has a fixed difficulty (testnet): the rate says nothing.
    FixedDifficulty,
}

impl BlockRateStatus {
    /// `normal`, `low`, `high`, `fixed-difficulty`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Low => "low",
            Self::High => "high",
            Self::FixedDifficulty => "fixed-difficulty",
        }
    }

    /// Whether the monitor should warn.
    #[must_use]
    pub const fn is_abnormal(self) -> bool {
        matches!(self, Self::Low | Self::High)
    }
}

/// Blocks of the last hour against the target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockRate {
    /// Seconds looked back ([`BLOCK_RATE_WINDOW_SECS`]).
    pub window_secs: u64,
    /// Main-chain blocks with a timestamp in the window (capped by the scan limit).
    pub blocks: u64,
    /// `window_secs / DIFFICULTY_TARGET`.
    pub expected: u64,
    /// `blocks / expected`.
    pub ratio: f64,
    /// Verdict.
    pub status: BlockRateStatus,
}

/// Major and minor version: the first two varints of every block blob.
fn header_versions(blob: &[u8]) -> Option<(u8, u8)> {
    let mut pos = 0;
    let major = read_varint(blob, &mut pos, 8).ok()?;
    let minor = read_varint(blob, &mut pos, 8).ok()?;
    Some((major as u8, minor as u8))
}

impl Chain {
    /// The fork state for `/getinfo` and `/status` (readiness cached per tip).
    #[allow(clippy::cast_precision_loss)]
    pub fn fork_status(&self) -> Result<ForkStatus, ChainError> {
        let forks = self.params().forks();
        let tip = self.tip();
        let next = tip.height + 1;
        let next_fork = forks.next_fork(next);
        let target = forks.signal_target(next).map(|f| f.minor_version_signal);
        let (window, signalling) = match target {
            Some(t) => self.signal_count(tip.hash, tip.height, t)?,
            None => (0, 0),
        };
        Ok(ForkStatus {
            tip_height: tip.height,
            next_block_major_version: forks.block_major_version_for(next),
            active_index: forks.fork_at(next).map(|f| f.index),
            next_index: next_fork.map(|f| f.index),
            next_height: next_fork.and_then(|f| f.height),
            supported_index: forks.supported_index(),
            signal_index: target,
            window,
            signalling,
            readiness: (target.is_some() && window > 0).then(|| signalling as f64 / window as f64),
        })
    }

    /// `(window, signalling)` for the blocks `top + 1 - window ..= top` (genesis excluded).
    fn signal_count(&self, tip: Hash, top: u64, target: u8) -> Result<(u64, u64), ChainError> {
        if let Some(c) = *self.signal_cache.lock()
            && c.tip == tip
            && c.target == target
        {
            return Ok((c.window, c.signalling));
        }
        let start = top.saturating_sub(FORK_READINESS_WINDOW - 1).max(1);
        let reader = self.reader()?;
        let mut window = 0;
        let mut signalling = 0;
        self.with_compressor(|comp| -> Result<(), ChainError> {
            for h in start..=top {
                let Some(blob) = reader.block_blob(h, comp)? else {
                    continue;
                };
                window += 1;
                if header_versions(&blob).is_some_and(|(_, minor)| signals_fork(minor, target)) {
                    signalling += 1;
                }
            }
            Ok(())
        })??;
        *self.signal_cache.lock() = Some(SignalCache {
            tip,
            target,
            window,
            signalling,
        });
        Ok((window, signalling))
    }

    /// Main-chain blocks with a timestamp in `(now - 3600, now]`, judged against the target.
    #[allow(clippy::cast_precision_loss)]
    pub fn block_rate(&self, now: u64) -> Result<BlockRate, ChainError> {
        let expected = BLOCK_RATE_WINDOW_SECS / DIFFICULTY_TARGET;
        let since = now.saturating_sub(BLOCK_RATE_WINDOW_SECS);
        let reader = self.reader()?;
        let top = reader.top_height()?.unwrap_or(0);
        let start = top.saturating_sub(BLOCK_RATE_MAX_SCAN - 1).max(1);
        let blocks = if top == 0 {
            0
        } else {
            reader
                .block_infos(start, top + 1)?
                .iter()
                .filter(|i| i.timestamp > since && i.timestamp <= now)
                .count() as u64
        };
        let status = if self.params().fixed_difficulty != 0 {
            BlockRateStatus::FixedDifficulty
        } else if blocks * BLOCK_RATE_FACTOR < expected {
            BlockRateStatus::Low
        } else if blocks > expected * BLOCK_RATE_FACTOR {
            BlockRateStatus::High
        } else {
            BlockRateStatus::Normal
        };
        Ok(BlockRate {
            window_secs: BLOCK_RATE_WINDOW_SECS,
            blocks,
            expected,
            ratio: blocks as f64 / expected as f64,
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::header_versions;

    #[test]
    fn versions_from_a_blob() {
        assert_eq!(header_versions(&[6, 13, 0xaa]), Some((6, 13)));
        assert_eq!(header_versions(&[5]), None);
    }
}
