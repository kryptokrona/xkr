//! What a pool needs from the node: the body of the daemon's `get_miner_data`
//! (`Core::getMinerDataBlockPart` + `getMinerTxBacklog`), main-chain headers by height or hash
//! (p2pool's `ChainMain` records, C++ `p2pool::get_block_headers_range` /
//! `getblockheaderbyhash`) and a watch channel that fires when the main-chain tip changes.
//!
//! The RPC method `get_miner_data` (`xkr-rpc`) and the embedded p2pool both read these, so they
//! always see the same values.

use tokio::sync::watch;
use xkr_consensus::params::{
    BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW, BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3,
    LWMA_2_DIFFICULTY_BLOCK_INDEX,
};
use xkr_consensus::reward::median;
use xkr_consensus::{block_major_version_for_height, difficulty_for_next_block};
use xkr_storage::BlockInfo;
use xkr_txpool::PoolTxSummary;
use xkr_types::Hash;

use crate::chain::{Chain, TipInfo};
use crate::error::ChainError;
use crate::headers::context_from_storage;

/// The data of `get_miner_data` for the next block, read from one storage snapshot so every
/// field describes the same top block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerData {
    /// Block major version of the next block.
    pub major_version: u8,
    /// Height of the next block (top index + 1).
    pub height: u64,
    /// Id of the top block (the next block's previous id).
    pub prev_id: Hash,
    /// Difficulty of the next block.
    pub difficulty: u64,
    /// `max(median of the last 100 block sizes, full reward zone)`.
    pub median_weight: u64,
    /// Coins generated up to and including the top block.
    pub already_generated_coins: u64,
    /// Median timestamp of the timestamp-check window (0 while the chain is shorter).
    pub median_timestamp: u64,
    /// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` of the network.
    pub unlock_window: u64,
    /// The transactions the next block would carry ([`Chain::template_backlog`]), uncapped.
    pub tx_backlog: Vec<PoolTxSummary>,
}

/// Why [`Chain::miner_data`] failed.
#[derive(Debug, thiserror::Error)]
pub enum MinerDataError {
    /// Storage failure.
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// The top block has no stored info.
    #[error("no block info at height {0}")]
    MissingTop(u64),
    /// `getDifficultyForNextBlock() == 0`.
    #[error("difficulty overhead")]
    DifficultyOverhead,
}

/// One main-chain block as a pool sees it (p2pool `ChainMain` without the reward).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MainChainHeader {
    /// Height (block index).
    pub height: u64,
    /// Block id.
    pub hash: Hash,
    /// Block timestamp.
    pub timestamp: u64,
    /// Difficulty the block was mined at.
    pub difficulty: u64,
    /// Cumulative difficulty up to and including the block.
    pub cumulative_difficulty: u128,
}

impl MainChainHeader {
    fn from_info(height: u64, info: &BlockInfo) -> Self {
        Self {
            height,
            hash: Hash(info.hash),
            timestamp: info.timestamp,
            difficulty: info.difficulty,
            cumulative_difficulty: info.cumulative_difficulty(),
        }
    }
}

impl Chain {
    /// `get_miner_data` for the next block. The backlog is exactly what `getblocktemplate`
    /// would select ([`Chain::template_backlog`]: template rules, priority order,
    /// deterministic while the pool is unchanged); the RPC caps its length, this does not.
    pub fn miner_data(&self) -> Result<MinerData, MinerDataError> {
        let params = self.params();
        let reader = self.reader()?;
        let top = reader.top_height().map_err(ChainError::from)?.unwrap_or(0);
        let height = top + 1;
        let h32 = u32::try_from(height).unwrap_or(u32::MAX);
        let top_info = reader
            .block_info(top)
            .map_err(ChainError::from)?
            .ok_or(MinerDataError::MissingTop(top))?;
        let ctx = context_from_storage(params, &reader, height)?;
        let difficulty = difficulty_for_next_block(
            params,
            h32,
            &ctx.difficulty_timestamps,
            &ctx.difficulty_cumulative,
        );
        if difficulty == 0 {
            return Err(MinerDataError::DifficultyOverhead);
        }
        let median_weight = self.block_median_size(&reader)?;
        // The window getBlockTemplate uses; a block below this median timestamp is refused.
        let window = if height >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
            BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3 as u64
        } else {
            BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW as u64
        };
        let median_timestamp = if height >= window {
            let infos = reader
                .block_infos(height - window, height)
                .map_err(ChainError::from)?;
            let stamps: Vec<u64> = infos.iter().map(|i| i.timestamp).collect();
            median(&stamps)
        } else {
            0
        };
        drop(reader);

        let tx_backlog = self.template_backlog()?;
        Ok(MinerData {
            major_version: block_major_version_for_height(params, h32),
            height,
            prev_id: Hash(top_info.hash),
            difficulty,
            median_weight,
            already_generated_coins: top_info.already_generated_coins,
            median_timestamp,
            unlock_window: u64::from(params.mined_money_unlock_window),
            tx_backlog,
        })
    }

    /// Main-chain headers of heights `start..end` that exist (an empty list above the top).
    pub fn main_chain_headers(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<MainChainHeader>, ChainError> {
        let reader = self.reader()?;
        let top = reader.top_height()?.unwrap_or(0);
        let end = end.min(top + 1);
        if start >= end {
            return Ok(Vec::new());
        }
        let infos = reader.block_infos(start, end)?;
        Ok(infos
            .iter()
            .zip(start..)
            .map(|(info, height)| MainChainHeader::from_info(height, info))
            .collect())
    }

    /// The main-chain header of the block `hash`, `None` if it is not on the main chain.
    pub fn main_chain_header_by_hash(
        &self,
        hash: &Hash,
    ) -> Result<Option<MainChainHeader>, ChainError> {
        let reader = self.reader()?;
        let Some(height) = reader.height_of(hash)? else {
            return Ok(None);
        };
        Ok(reader
            .block_info(height)?
            .map(|info| MainChainHeader::from_info(height, &info)))
    }

    /// A receiver that sees every new main-chain tip: after blocks are appended, after a switch
    /// to an alternative branch and after a failed switch restored the old chain. Only the
    /// final tip of each [`Chain::add_block`] / [`Chain::add_blocks`] call is published.
    #[must_use]
    pub fn subscribe_tip(&self) -> watch::Receiver<TipInfo> {
        self.tip_watch.subscribe()
    }

    /// Publishes the current tip if it differs from the last published one.
    pub(crate) fn publish_tip(&self) {
        let tip = self.tip();
        self.tip_watch.send_if_modified(|last| {
            if *last == tip {
                false
            } else {
                *last = tip;
                true
            }
        });
    }
}
