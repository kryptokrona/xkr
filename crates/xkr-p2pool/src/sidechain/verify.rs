//! Share verification (`SideChain::verify`, `SideChain::verify_loop`, `src/side_chain.cpp`).

use xkr_types::Hash;

use super::{SideChain, SidechainEvent};
use crate::block::PoolBlock;
use crate::config::UNCLE_BLOCK_DEPTH;
use crate::difficulty::Difficulty;
use crate::forks::{COINBASE_TX_VERSION_2, coinbase_tx_version};
use crate::keys::eph_public_key;
use crate::mainchain::MainChainView;
use crate::order::hash_less;
use crate::pplns::{decompose_outputs, split_reward};

/// The outcome of one verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Parent or an uncle is not stored or not verified yet.
    NotYet,
    /// Verified and invalid (permanent).
    Invalid,
    /// Verified and valid.
    Valid,
}

impl SideChain {
    /// C++ `verify(block)`: sets `verified` / `invalid` of a stored share.
    pub(super) fn verify(&mut self, id: &Hash, main: &dyn MainChainView) {
        let Some(block) = self.blocks.get(id) else {
            return;
        };
        let (verdict, parent_difficulty) = self.check(block, main);
        if let Some((parent, d)) = parent_difficulty
            && let Some(p) = self.blocks.get_mut(&parent)
        {
            p.offchain.cached_next_difficulty = Some(d);
        }
        if let Some(b) = self.blocks.get_mut(id) {
            match verdict {
                Verdict::NotYet => b.offchain.verified = false,
                Verdict::Invalid => {
                    b.offchain.verified = true;
                    b.offchain.invalid = true;
                }
                Verdict::Valid => {
                    b.offchain.verified = true;
                    b.offchain.invalid = false;
                }
            }
        }
    }

    /// The checks of C++ `verify`, in order. Also returns the parent's next difficulty when it
    /// was computed here, for the cache.
    fn check(
        &self,
        block: &PoolBlock,
        main: &dyn MainChainView,
    ) -> (Verdict, Option<(Hash, Difficulty)>) {
        let cfg = &self.options.config;
        let min_diff = cfg.min_difficulty();

        // Genesis share.
        if block.sidechain_height == 0 {
            let ok = block.parent.is_zero()
                && block.uncles.is_empty()
                && block.difficulty == min_diff
                && block.cumulative_difficulty == min_diff
                && block.txkey_sec_seed == self.consensus_id;
            return (if ok { Verdict::Valid } else { Verdict::Invalid }, None);
        }

        // Deep share: below what any window can reach, accepted as the network did.
        if block.offchain.depth > cfg.unchecked_depth() {
            return (Verdict::Valid, None);
        }

        if block.parent.is_zero() {
            return (Verdict::Invalid, None);
        }
        let Some(parent) = self
            .blocks
            .get(&block.parent)
            .filter(|p| p.offchain.verified)
        else {
            return (Verdict::NotYet, None);
        };
        if parent.offchain.invalid {
            return (Verdict::Invalid, None);
        }

        let seed = if block.prev_id == parent.prev_id {
            parent.txkey_sec_seed
        } else {
            parent.calculate_tx_key_seed(self.options.unlock_window)
        };
        if block.txkey_sec_seed != seed {
            return (Verdict::Invalid, None);
        }
        if block.sidechain_height != parent.sidechain_height + 1 {
            return (Verdict::Invalid, None);
        }
        if block.uncles.windows(2).any(|w| !hash_less(&w[0], &w[1])) {
            return (Verdict::Invalid, None);
        }

        let mut expected_cumulative = parent.cumulative_difficulty.wrapping_add(block.difficulty);
        let mined = if block.uncles.is_empty() {
            Vec::new()
        } else {
            self.mined_blocks(
                Some(parent),
                UNCLE_BLOCK_DEPTH.min(block.sidechain_height + 1),
            )
        };
        for uncle_id in &block.uncles {
            if uncle_id.is_zero() || mined.contains(uncle_id) {
                return (Verdict::Invalid, None);
            }
            let Some(uncle) = self.blocks.get(uncle_id).filter(|u| u.offchain.verified) else {
                return (Verdict::NotYet, None);
            };
            if uncle.offchain.invalid {
                return (Verdict::Invalid, None);
            }
            if uncle.sidechain_height >= block.sidechain_height
                || uncle.sidechain_height + UNCLE_BLOCK_DEPTH < block.sidechain_height
            {
                return (Verdict::Invalid, None);
            }
            if !self.uncle_on_same_chain(parent, uncle, block.sidechain_height) {
                return (Verdict::Invalid, None);
            }
            expected_cumulative = expected_cumulative.wrapping_add(uncle.difficulty);
        }

        if block.cumulative_difficulty != expected_cumulative {
            return (Verdict::Invalid, None);
        }

        // Difficulty and payouts are checked only inside the PPLNS window.
        if block.offchain.depth >= cfg.pplns_window {
            return (Verdict::Valid, None);
        }

        let mut computed = None;
        let difficulty = if self.tip == Some(parent.sidechain_id) {
            self.difficulty
        } else {
            let Some(d) = self.compute_difficulty(parent) else {
                return (Verdict::Invalid, None);
            };
            computed = Some((parent.sidechain_id, d));
            d
        };
        if difficulty != block.difficulty {
            return (Verdict::Invalid, computed);
        }

        let Ok(window) = self.get_shares(block, main) else {
            return (Verdict::Invalid, computed);
        };
        let total = block
            .outputs
            .iter()
            .fold(0u64, |a, o| a.wrapping_add(o.reward));
        let Ok(rewards) = split_reward(total, &window.shares) else {
            return (Verdict::Invalid, computed);
        };
        let expected = decompose_outputs(&window.shares, &rewards);
        if expected.len() != block.outputs.len() || expected.len() != block.eph_public_keys.len() {
            return (Verdict::Invalid, computed);
        }
        let tagged = coinbase_tx_version(block.major_version) >= COINBASE_TX_VERSION_2;
        for (i, ((amount, wallet), (out, key))) in expected
            .iter()
            .zip(block.outputs.iter().zip(&block.eph_public_keys))
            .enumerate()
        {
            if *amount != out.reward {
                return (Verdict::Invalid, computed);
            }
            let Some((eph, view_tag)) = eph_public_key(wallet, &block.txkey_sec, i as u64) else {
                return (Verdict::Invalid, computed);
            };
            if tagged && view_tag != out.view_tag {
                return (Verdict::Invalid, computed);
            }
            if eph != *key {
                return (Verdict::Invalid, computed);
            }
        }
        (Verdict::Valid, computed)
    }

    /// C++ `verify_loop(block)`: verifies `start`, then every stored share waiting on a newly
    /// valid one (children one height up, shares naming it as an uncle up to `U` heights up,
    /// their depths raised to match), and offers the highest newly valid share as the tip.
    pub(super) fn verify_loop(&mut self, start: Hash, main: &dyn MainChainView) {
        let mut stack = vec![start];
        let mut highest: Option<Hash> = None;
        while let Some(id) = stack.pop() {
            if self.blocks.get(&id).is_none_or(|b| b.offchain.verified) {
                continue;
            }
            self.verify(&id, main);
            let Some(b) = self.blocks.get(&id) else {
                continue;
            };
            if !b.offchain.verified {
                continue;
            }
            let (height, depth) = (b.sidechain_height, b.offchain.depth);
            if b.offchain.invalid {
                self.events.push(SidechainEvent::Invalid { id, height });
                continue;
            }
            self.events.push(SidechainEvent::Verified { id, height });

            let current = highest.and_then(|h| self.blocks.get(&h));
            if self.is_longer_chain(current, b, main).longer {
                highest = Some(id);
            }

            if let Some(b) = self.blocks.get_mut(&id)
                && b.offchain.want_broadcast
                && !b.offchain.broadcasted
            {
                b.offchain.broadcasted = true;
                if depth < UNCLE_BLOCK_DEPTH {
                    self.events.push(SidechainEvent::Broadcast(id));
                }
            }
            self.events.push(SidechainEvent::StoreInCache(id));

            for i in 1..=UNCLE_BLOCK_DEPTH {
                for child_id in self.ids_at(height + i) {
                    let Some(c) = self.blocks.get_mut(&child_id) else {
                        continue;
                    };
                    if i == 1 && c.parent == id {
                        if c.offchain.depth + 1 < depth {
                            c.offchain.depth = depth - 1;
                        }
                        stack.push(child_id);
                    } else if c.uncles.contains(&id) {
                        if c.offchain.depth + i < depth {
                            c.offchain.depth = depth - i;
                        }
                        stack.push(child_id);
                    }
                }
            }
        }
        if let Some(h) = highest {
            self.update_chain_tip(h, main);
        }
    }
}
