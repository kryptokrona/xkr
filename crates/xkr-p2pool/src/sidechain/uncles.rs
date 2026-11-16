//! Uncles: the same-chain test shared by template and verification, and the sidechain data of
//! a new share (`SideChain::fill_sidechain_data`, `src/side_chain.cpp`).

use xkr_types::Hash;

use super::SideChain;
use crate::block::{MAX_UNCLES_PER_BLOCK, PoolBlock};
use crate::config::UNCLE_BLOCK_DEPTH;
use crate::keys::get_tx_keys;
use crate::mainchain::MainChainView;
use crate::order::hash_cmp;
use crate::pplns::{PplnsError, PplnsWindow};

impl SideChain {
    /// The same-chain test of `fill_sidechain_data` and `verify`: walk `from` (the tip, or the
    /// new share's parent) down to the uncle's height (it must reach exactly that height),
    /// then for at most `U` steps, while the walked share is at most `U` below `new_height`,
    /// compare parents: the uncle is on the chain if both have the same parent.
    pub(super) fn uncle_on_same_chain(
        &self,
        from: &PoolBlock,
        uncle: &PoolBlock,
        new_height: u64,
    ) -> bool {
        let mut walk = Some(from);
        while let Some(t) = walk {
            if t.sidechain_height <= uncle.sidechain_height {
                break;
            }
            walk = self.parent_of(t);
        }
        if walk.is_none_or(|t| t.sidechain_height != uncle.sidechain_height) {
            return false;
        }
        let mut uncle_walk = Some(uncle);
        for _ in 0..UNCLE_BLOCK_DEPTH {
            let (Some(a), Some(b)) = (walk, uncle_walk) else {
                break;
            };
            if a.sidechain_height + UNCLE_BLOCK_DEPTH < new_height {
                break;
            }
            if a.parent == b.parent {
                return true;
            }
            walk = self.parent_of(a);
            uncle_walk = self.parent_of(b);
        }
        false
    }

    /// The ids of `start` and its ancestors and their uncles, for `min(U, height + 1)` shares
    /// (the "already mined" list of `fill_sidechain_data` and `verify`).
    pub(super) fn mined_blocks(&self, start: Option<&PoolBlock>, count: u64) -> Vec<Hash> {
        let mut mined = Vec::with_capacity(UNCLE_BLOCK_DEPTH as usize * 2 + 1);
        let mut walk = start;
        let mut i = 0;
        while let Some(t) = walk {
            if i >= count {
                break;
            }
            mined.push(t.sidechain_id);
            mined.extend_from_slice(&t.uncles);
            walk = self.parent_of(t);
            i += 1;
        }
        mined
    }

    /// C++ `fill_sidechain_data(block, shares)`: the sidechain part of a new share on the
    /// current tip (`block.prev_id` and the miner wallet already set): tx key seed and keys,
    /// parent, height, uncles, difficulty and cumulative difficulty; returns its PPLNS window.
    ///
    /// Without a tip it is the genesis share. Otherwise the uncles are the verified, valid
    /// shares at the tip's height and the `U - 1` heights below that are not already mined by
    /// the tip or its ancestors and are on the same chain, in hash order, at most 5.
    pub fn fill_sidechain_data(
        &self,
        block: &mut PoolBlock,
        main: &dyn MainChainView,
    ) -> Result<PplnsWindow, PplnsError> {
        block.uncles.clear();
        let Some(tip) = self.chain_tip() else {
            block.parent = Hash::ZERO;
            block.sidechain_height = 0;
            block.difficulty = self.options.config.min_difficulty();
            block.cumulative_difficulty = block.difficulty;
            block.txkey_sec_seed = self.consensus_id;
            (block.txkey_pub, block.txkey_sec) = get_tx_keys(&block.txkey_sec_seed, &block.prev_id);
            return self.get_shares(block, main);
        };

        block.txkey_sec_seed = if block.prev_id == tip.prev_id {
            tip.txkey_sec_seed
        } else {
            tip.calculate_tx_key_seed(self.options.unlock_window)
        };
        (block.txkey_pub, block.txkey_sec) = get_tx_keys(&block.txkey_sec_seed, &block.prev_id);
        block.parent = tip.sidechain_id;
        block.sidechain_height = tip.sidechain_height + 1;

        let n = UNCLE_BLOCK_DEPTH.min(tip.sidechain_height + 1);
        let mined = self.mined_blocks(Some(tip), n);
        for i in 0..n {
            for uncle in self.blocks_at_height(tip.sidechain_height - i) {
                if !uncle.offchain.verified
                    || uncle.offchain.invalid
                    || mined.contains(&uncle.sidechain_id)
                {
                    continue;
                }
                if self.uncle_on_same_chain(tip, uncle, block.sidechain_height) {
                    block.uncles.push(uncle.sidechain_id);
                }
            }
        }
        block.uncles.sort_by(hash_cmp);
        block.uncles.dedup();
        block.uncles.truncate(MAX_UNCLES_PER_BLOCK as usize);

        block.difficulty = self.difficulty;
        block.cumulative_difficulty = tip.cumulative_difficulty.wrapping_add(block.difficulty);
        for id in &block.uncles {
            if let Some(u) = self.blocks.get(id) {
                block.cumulative_difficulty =
                    block.cumulative_difficulty.wrapping_add(u.difficulty);
            }
        }
        self.get_shares(block, main)
    }
}
