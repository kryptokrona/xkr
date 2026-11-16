//! What the sidechain needs to know about the main chain (`ChainMain` and `MinerData`,
//! `src/common.h`; `p2pool::get_seed`, `get_difficulty_at_height`, `chainmain_get_by_hash`,
//! `handle_miner_data`, `cleanup_mainchain_data`, `src/p2pool.cpp`).
//!
//! The sidechain asks three things: the main-chain difficulty at a height (the PPLNS weight cap
//! uses the difficulty at the seed height of a share), the main-chain blocks it knows by hash
//! (fork choice between alternative sidechains), and the current miner data (height, top block,
//! next difficulty). [`MainChainView`] is that interface; [`MainChainCache`] is the C++ cache of
//! headers with the same keys and pruning, filled by xkrd from its own chain.

use std::collections::{BTreeMap, HashMap};

use xkr_types::Hash;

use crate::difficulty::Difficulty;

/// `SEEDHASH_EPOCH_BLOCKS`.
pub const SEEDHASH_EPOCH_BLOCKS: u64 = 2048;
/// `SEEDHASH_EPOCH_LAG`.
pub const SEEDHASH_EPOCH_LAG: u64 = 64;
/// `BLOCK_HEADERS_REQUIRED` before it is raised for the sidechain's window.
pub const BLOCK_HEADERS_REQUIRED: u64 = 720;

/// C++ `get_seed_height(height)`: `(height - 65) & !2047` above 64, else 0.
#[must_use]
pub const fn seed_height(height: u64) -> u64 {
    if height > SEEDHASH_EPOCH_LAG {
        (height - SEEDHASH_EPOCH_LAG - 1) & !(SEEDHASH_EPOCH_BLOCKS - 1)
    } else {
        0
    }
}

/// One main-chain block as the sidechain sees it (C++ `ChainMain`). Fields the source did not
/// fill stay zero, as in C++ (the entry for the next height only has its difficulty, the entry
/// for the top block from miner data only its id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChainMain {
    /// Block difficulty (for the next height: the difficulty of the next block).
    pub difficulty: Difficulty,
    /// Height.
    pub height: u64,
    /// Timestamp.
    pub timestamp: u64,
    /// Block reward.
    pub reward: u64,
    /// Block id.
    pub id: Hash,
}

/// The parts of the daemon's miner data the sidechain uses (C++ `MinerData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MainChainTip {
    /// Height of the next block (top + 1).
    pub height: u64,
    /// Id of the top block.
    pub prev_id: Hash,
    /// Difficulty of the next block.
    pub difficulty: Difficulty,
}

/// The main chain as the sidechain queries it.
pub trait MainChainView {
    /// The block at `height`, if known (`m_mainchainByHeight`).
    fn block_at_height(&self, height: u64) -> Option<ChainMain>;
    /// The block with id `id`, if known (`chainmain_get_by_hash`).
    fn block_by_hash(&self, id: &Hash) -> Option<ChainMain>;
    /// The current miner data (`miner_data()`).
    fn tip(&self) -> MainChainTip;

    /// C++ `get_difficulty_at_height`: the difficulty at `height`, if known and non-zero.
    fn difficulty_at_height(&self, height: u64) -> Option<Difficulty> {
        self.block_at_height(height)
            .map(|c| c.difficulty)
            .filter(|d| !d.is_zero())
    }

    /// C++ `get_seed(height)`: the id of the block at the seed height of `height`, if known.
    fn seed(&self, height: u64) -> Option<Hash> {
        self.block_at_height(seed_height(height)).map(|c| c.id)
    }
}

/// The C++ main-chain header cache (`m_mainchainByHeight`, `m_mainchainByHash`).
#[derive(Debug, Clone, Default)]
pub struct MainChainCache {
    by_height: BTreeMap<u64, ChainMain>,
    by_hash: HashMap<Hash, ChainMain>,
    tip: MainChainTip,
    headers_required: u64,
}

impl MainChainCache {
    /// An empty cache keeping `max(720, headers_required)` heights below the tip (C++ raises
    /// `BLOCK_HEADERS_REQUIRED` to the sidechain's `monero_headers_required()`).
    #[must_use]
    pub fn new(headers_required: u64) -> Self {
        Self {
            headers_required: headers_required.max(BLOCK_HEADERS_REQUIRED),
            ..Self::default()
        }
    }

    /// Heights kept below the tip.
    #[must_use]
    pub const fn headers_required(&self) -> u64 {
        self.headers_required
    }

    /// Number of heights held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_height.len()
    }

    /// No heights held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_height.is_empty()
    }

    /// A full block header (C++ `parse_block_header`, `parse_block_headers_range`): stored by
    /// height and by id.
    pub fn insert_header(&mut self, header: ChainMain) {
        self.by_height.insert(header.height, header);
        self.by_hash.insert(header.id, header);
    }

    /// A new main-chain block without its id (C++ `handle_chain_main`): height, timestamp and
    /// reward are set on the entry at that height, which keeps its id and difficulty.
    pub fn insert_chain_main(&mut self, height: u64, timestamp: u64, reward: u64) {
        let c = self.by_height.entry(height).or_default();
        c.height = height;
        c.timestamp = timestamp;
        c.reward = reward;
        let c = *c;
        self.by_hash.insert(c.id, c);
    }

    /// New miner data (C++ `handle_miner_data`): the next height gets the next difficulty, the
    /// top block gets its id, then old heights are pruned.
    pub fn set_tip(&mut self, tip: MainChainTip) {
        self.tip = tip;
        let next = self.by_height.entry(tip.height).or_default();
        next.height = tip.height;
        next.difficulty = tip.difficulty;
        if let Some(prev_height) = tip.height.checked_sub(1) {
            let prev_block = self.by_height.entry(prev_height).or_default();
            prev_block.height = prev_height;
            prev_block.id = tip.prev_id;
            let prev_block = *prev_block;
            self.by_hash.insert(prev_block.id, prev_block);
        }
        self.cleanup(tip.height);
    }

    /// C++ `cleanup_mainchain_data(height)`: drops every height more than `headers_required`
    /// below `height`, except the seed heights of that range.
    pub fn cleanup(&mut self, height: u64) {
        let mut seed = seed_height(height);
        let oldest_seed = if height > self.headers_required {
            seed_height(height - self.headers_required)
        } else {
            0
        };
        let mut kept_seeds = vec![seed];
        while seed > oldest_seed.max(SEEDHASH_EPOCH_BLOCKS - 1) {
            seed -= SEEDHASH_EPOCH_BLOCKS;
            kept_seeds.push(seed);
        }
        let old: Vec<u64> = self
            .by_height
            .range(..height.saturating_sub(self.headers_required))
            .map(|(h, _)| *h)
            .filter(|h| !kept_seeds.contains(h))
            .collect();
        for h in old {
            if let Some(c) = self.by_height.remove(&h) {
                self.by_hash.remove(&c.id);
            }
        }
    }
}

impl MainChainView for MainChainCache {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        self.by_height.get(&height).copied()
    }

    fn block_by_hash(&self, id: &Hash) -> Option<ChainMain> {
        self.by_hash.get(id).copied()
    }

    fn tip(&self) -> MainChainTip {
        self.tip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_heights() {
        assert_eq!(seed_height(0), 0);
        assert_eq!(seed_height(64), 0);
        assert_eq!(seed_height(65), 0);
        assert_eq!(seed_height(2048 + 64), 0);
        assert_eq!(seed_height(2048 + 65), 2048);
        assert_eq!(seed_height(2_620_644), 2_619_392);
        assert_eq!(seed_height(2_621_142), 2_619_392);
    }

    #[test]
    fn cache_keeps_recent_heights_and_seeds() {
        let mut c = MainChainCache::new(100);
        assert_eq!(c.headers_required(), BLOCK_HEADERS_REQUIRED);
        let top = 10_000u64;
        for h in 0..top {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&h.to_le_bytes());
            c.insert_header(ChainMain {
                difficulty: Difficulty::from_u64(1000 + h),
                height: h,
                timestamp: 1_000_000 + h * 90,
                reward: 5,
                id: Hash(id),
            });
        }
        let mut prev = [0u8; 32];
        prev[..8].copy_from_slice(&(top - 1).to_le_bytes());
        c.set_tip(MainChainTip {
            height: top,
            prev_id: Hash(prev),
            difficulty: Difficulty::from_u64(77),
        });
        assert_eq!(c.difficulty_at_height(top), Some(Difficulty::from_u64(77)));
        assert_eq!(c.block_at_height(top - 1).unwrap().id, Hash(prev));
        // The last 720 heights stay.
        assert!(c.block_at_height(top - 720).is_some());
        assert!(c.block_at_height(top - 721).is_none());
        // Seed heights of the kept range stay too.
        let seed = seed_height(top);
        let oldest = seed_height(top - 720);
        assert!(c.block_at_height(seed).is_some());
        assert!(c.block_at_height(oldest).is_some());
        assert!(c.block_at_height(oldest - 2048).is_none());
        assert_eq!(c.seed(top), c.block_at_height(seed).map(|b| b.id));
        assert!(
            c.block_by_hash(&c.block_at_height(oldest).unwrap().id)
                .is_some()
        );
        let mut gone = [0u8; 32];
        gone[..8].copy_from_slice(&5u64.to_le_bytes());
        assert!(c.block_by_hash(&Hash(gone)).is_none());
        // A zero difficulty is "unknown".
        c.insert_chain_main(top + 5, 1, 2);
        assert_eq!(c.difficulty_at_height(top + 5), None);
        assert_eq!(c.block_at_height(top + 5).unwrap().reward, 2);
    }
}
