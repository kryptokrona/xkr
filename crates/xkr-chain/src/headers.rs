//! In-memory window of recent [`BlockInfo`]s and the consensus context built from it.
//!
//! The "consensus context service" of Cuprate in miniature: difficulty, timestamp and
//! block-size windows are sliced from a `Vec<BlockInfo>` that the writer extends block by block
//! (including blocks of the batch being verified that are not committed yet) instead of
//! reading storage for every block.

use xkr_consensus::NetworkParams;
use xkr_consensus::difficulty::difficulty_window;
use xkr_consensus::params::CRYPTONOTE_REWARD_BLOCKS_WINDOW;
use xkr_consensus::validation::timestamp_check_window;
use xkr_storage::{BlockInfo, Reader};

use crate::error::ChainError;

/// Blocks kept in memory. Must cover the largest window: `DIFFICULTY_WINDOW_V1 +
/// DIFFICULTY_LAG_V1` = 2895 blocks (major version 1/2), 100 sizes, 60 timestamps.
pub const DEFAULT_HEADER_CACHE: usize = 4096;

/// The chain data `Core::addBlock` reads for one block, owned.
#[derive(Debug, Clone, Default)]
pub struct ContextData {
    /// Height of the block being validated.
    pub height: u64,
    /// Timestamps of the timestamp-check window (genesis included).
    pub recent_timestamps: Vec<u64>,
    /// Timestamps of the difficulty window (genesis excluded).
    pub difficulty_timestamps: Vec<u64>,
    /// Cumulative difficulties of the difficulty window (`u64`, saturating, like C++).
    pub difficulty_cumulative: Vec<u64>,
    /// Block sizes of the last `CRYPTONOTE_REWARD_BLOCKS_WINDOW` blocks (genesis included).
    pub recent_block_sizes: Vec<u64>,
    /// `getAlreadyGeneratedCoins(height - 1)`.
    pub already_generated_coins: u64,
    /// Cumulative difficulty of the previous block.
    pub prev_cumulative_difficulty: u128,
    /// `cumulative_tx_count` of the previous block.
    pub prev_cumulative_tx_count: u64,
    /// `cumulative_size` of the previous block.
    pub prev_cumulative_size: u64,
}

/// Lowest height whose header the context of the block at `height` needs.
#[must_use]
pub fn window_start(params: &NetworkParams, height: u64) -> u64 {
    let h32 = u32::try_from(height).unwrap_or(u32::MAX);
    let ts = height.saturating_sub(timestamp_check_window(h32) as u64);
    let diff = u64::from(difficulty_window(params, h32).start);
    let sizes = height.saturating_sub(CRYPTONOTE_REWARD_BLOCKS_WINDOW as u64);
    ts.min(diff).min(sizes)
}

/// Builds the context of the block at `height` (>= 1) from `headers`, the infos of heights
/// `base..height` (contiguous, ending with the parent). `base` must be `<=`
/// [`window_start`]`(params, height)`.
pub fn context_data(
    params: &NetworkParams,
    height: u64,
    base: u64,
    headers: &[BlockInfo],
) -> Result<ContextData, ChainError> {
    if height == 0 || base > window_start(params, height) || base + headers.len() as u64 != height {
        return Err(ChainError::Corrupt(format!(
            "header window {base}..{} cannot serve height {height}",
            base + headers.len() as u64
        )));
    }
    let at = |h: u64| &headers[(h - base) as usize];
    let h32 = u32::try_from(height).unwrap_or(u32::MAX);
    let ts_start = height.saturating_sub(timestamp_check_window(h32) as u64);
    let diff = difficulty_window(params, h32);
    let size_start = height.saturating_sub(CRYPTONOTE_REWARD_BLOCKS_WINDOW as u64);
    let parent = at(height - 1);
    let cd = |i: &BlockInfo| u64::try_from(i.cumulative_difficulty()).unwrap_or(u64::MAX);
    Ok(ContextData {
        height,
        recent_timestamps: (ts_start..height).map(|h| at(h).timestamp).collect(),
        difficulty_timestamps: (u64::from(diff.start)..u64::from(diff.end))
            .map(|h| at(h).timestamp)
            .collect(),
        difficulty_cumulative: (u64::from(diff.start)..u64::from(diff.end))
            .map(|h| cd(at(h)))
            .collect(),
        recent_block_sizes: (size_start..height).map(|h| at(h).block_size).collect(),
        already_generated_coins: parent.already_generated_coins,
        prev_cumulative_difficulty: parent.cumulative_difficulty(),
        prev_cumulative_tx_count: parent.cumulative_tx_count,
        prev_cumulative_size: parent.cumulative_size,
    })
}

/// Context for the block at `height` read straight from storage (RPC, pool, alt chains).
pub fn context_from_storage(
    params: &NetworkParams,
    reader: &Reader,
    height: u64,
) -> Result<ContextData, ChainError> {
    let base = window_start(params, height);
    let infos = reader.block_infos(base, height)?;
    context_data(params, height, base, &infos)
}

/// The last blocks of the main chain (`base..=top`), never empty once genesis is stored.
#[derive(Debug, Clone)]
pub struct HeaderCache {
    base: u64,
    infos: Vec<BlockInfo>,
    capacity: usize,
}

impl HeaderCache {
    /// Loads the last `capacity` infos ending at `top`.
    pub fn load(reader: &Reader, top: u64, capacity: usize) -> Result<Self, ChainError> {
        let base = (top + 1).saturating_sub(capacity as u64);
        let infos = reader.block_infos(base, top + 1)?;
        if infos.len() as u64 != top + 1 - base {
            return Err(ChainError::Corrupt(format!(
                "missing block infos in {base}..={top}"
            )));
        }
        Ok(Self {
            base,
            infos,
            capacity,
        })
    }

    /// Height of the newest block.
    #[must_use]
    pub fn top_height(&self) -> u64 {
        self.base + self.infos.len() as u64 - 1
    }

    /// Info of the newest block.
    #[must_use]
    pub fn top(&self) -> &BlockInfo {
        self.infos.last().expect("header cache is never empty")
    }

    /// Info at `height` if cached.
    #[must_use]
    pub fn get(&self, height: u64) -> Option<&BlockInfo> {
        height
            .checked_sub(self.base)
            .and_then(|i| self.infos.get(i as usize))
    }

    /// Appends the info of the next block.
    pub fn push(&mut self, info: BlockInfo) {
        self.infos.push(info);
        if self.infos.len() > 2 * self.capacity {
            let drop = self.infos.len() - self.capacity;
            self.infos.drain(..drop);
            self.base += drop as u64;
        }
    }

    /// Forgets blocks above `top`. Returns `false` if `top` is below the cached range (the
    /// caller must reload from storage).
    pub fn truncate_to(&mut self, top: u64) -> bool {
        if top < self.base {
            return false;
        }
        self.infos.truncate((top - self.base + 1) as usize);
        true
    }

    /// Context of the block at `height = top + 1`.
    pub fn next_context(&self, params: &NetworkParams) -> Result<ContextData, ChainError> {
        let height = self.top_height() + 1;
        let start = window_start(params, height).max(self.base);
        let from = (start - self.base) as usize;
        context_data(params, height, start, &self.infos[from..])
    }
}
