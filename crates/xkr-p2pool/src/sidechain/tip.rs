//! Fork choice (`SideChain::update_chain_tip`, `SideChain::is_longer_chain`,
//! `src/side_chain.cpp`).
//!
//! Two shares on the same chain (a common ancestor within `pplns_window` of both) compare by
//! cumulative difficulty. Shares on different chains compare by the total difficulty of their
//! last `pplns_window` shares (with uncles), and the candidate chain must also be anchored in
//! the main chain as this node knows it: recent main-chain blocks, at least half as many known
//! ones as the current chain, timestamps close to those blocks and a plausible time span.

use std::collections::HashSet;

use xkr_types::Hash;

use super::{SideChain, SidechainEvent};
use crate::block::PoolBlock;
use crate::config::{MAIN_CHAIN_BLOCK_TIME, UNCLE_BLOCK_DEPTH};
use crate::difficulty::Difficulty;
use crate::mainchain::MainChainView;

/// Largest accepted gap between a share's timestamp and that of its main-chain block (3 h).
const MAX_TIMESTAMP_DEVIATION: u64 = 10_800;

/// The result of [`SideChain::is_longer_chain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChainComparison {
    /// The candidate is the better tip.
    pub longer: bool,
    /// The two are not on the same chain (or there was no tip).
    pub alternative: bool,
}

/// `90th percentile - 10th percentile` of the values (C++ `span80`), 0 for degenerate input.
fn span80(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let cut = values.len().div_ceil(10);
    let pos1 = cut - 1;
    let pos2 = values.len() - cut;
    if pos1 >= pos2 {
        return 0;
    }
    values.sort_unstable();
    values[pos2] - values[pos1]
}

/// The main-chain blocks a candidate chain refers to (C++ `add_monero_block_from` of the
/// candidate).
#[derive(Default)]
struct CandidateAnchors {
    known: HashSet<Hash>,
    max_height: u64,
    max_deviation: u64,
}

impl CandidateAnchors {
    fn add(&mut self, main: &dyn MainChainView, id: &Hash, timestamp: u64) {
        let Some(data) = main.block_by_hash(id) else {
            return;
        };
        if data.timestamp != 0 {
            self.max_deviation = self.max_deviation.max(timestamp.abs_diff(data.timestamp));
        }
        if self.known.insert(*id) {
            self.max_height = self.max_height.max(data.height);
        }
    }
}

impl SideChain {
    /// C++ `is_longer_chain(block, candidate)`: is `candidate` a better tip than `block`?
    #[must_use]
    pub fn is_longer_chain(
        &self,
        block: Option<&PoolBlock>,
        candidate: &PoolBlock,
        main: &dyn MainChainView,
    ) -> ChainComparison {
        if !candidate.offchain.verified || candidate.offchain.invalid {
            return ChainComparison::default();
        }
        let Some(block) = block else {
            return ChainComparison {
                longer: true,
                alternative: true,
            };
        };
        let window = self.options.config.pplns_window;

        // Same chain: a common ancestor close enough to both.
        let mut block_ancestor = Some(block);
        while let Some(a) = block_ancestor {
            if a.sidechain_height <= candidate.sidechain_height {
                break;
            }
            block_ancestor = self.parent_of(a);
        }
        if let Some(ba) = block_ancestor {
            let mut candidate_ancestor = Some(candidate);
            while let Some(c) = candidate_ancestor {
                if c.sidechain_height <= ba.sidechain_height {
                    break;
                }
                candidate_ancestor = self.parent_of(c);
            }
            let mut pair = (Some(ba), candidate_ancestor);
            while let (Some(a), Some(c)) = pair {
                if a.parent == c.parent {
                    if block.sidechain_height - a.sidechain_height >= window
                        || candidate.sidechain_height - c.sidechain_height >= window
                    {
                        break;
                    }
                    return ChainComparison {
                        longer: block.cumulative_difficulty < candidate.cumulative_difficulty,
                        alternative: false,
                    };
                }
                pair = (self.parent_of(a), self.parent_of(c));
            }
        }

        // Different chains.
        let not_longer = ChainComparison {
            longer: false,
            alternative: true,
        };
        let mut block_total = Difficulty::ZERO;
        let mut candidate_total = Difficulty::ZERO;
        let mut current_known: HashSet<Hash> = HashSet::new();
        let mut anchors = CandidateAnchors::default();
        let mut candidate_min_height = 0u64;
        let mut candidate_timestamps = Vec::with_capacity(window as usize * 2);
        let mut old_chain = Some(block);
        let mut new_chain = Some(candidate);
        for _ in 0..window {
            if old_chain.is_none() && new_chain.is_none() {
                break;
            }
            if let Some(o) = old_chain {
                block_total = block_total.wrapping_add(o.difficulty);
                let mut note = |id: &Hash| {
                    if !current_known.contains(id) && main.block_by_hash(id).is_some() {
                        current_known.insert(*id);
                    }
                };
                for uncle_id in &o.uncles {
                    if let Some(u) = self.blocks.get(uncle_id)
                        && block.sidechain_height.wrapping_sub(u.sidechain_height) < window
                    {
                        block_total = block_total.wrapping_add(u.difficulty);
                        note(&u.prev_id);
                    }
                }
                note(&o.prev_id);
                old_chain = self.parent_of(o);
            }
            if let Some(n) = new_chain {
                candidate_min_height = if candidate_min_height == 0 {
                    n.txin_gen_height
                } else {
                    candidate_min_height.min(n.txin_gen_height)
                };
                candidate_total = candidate_total.wrapping_add(n.difficulty);
                candidate_timestamps.push(n.timestamp);
                for uncle_id in &n.uncles {
                    if let Some(u) = self.blocks.get(uncle_id)
                        && candidate.sidechain_height.wrapping_sub(u.sidechain_height) < window
                    {
                        candidate_min_height = candidate_min_height.min(u.txin_gen_height);
                        candidate_total = candidate_total.wrapping_add(u.difficulty);
                        candidate_timestamps.push(u.timestamp);
                        anchors.add(main, &u.prev_id, u.timestamp);
                    }
                }
                anchors.add(main, &n.prev_id, n.timestamp);
                new_chain = self.parent_of(n);
            }
        }

        if block_total >= candidate_total {
            return not_longer;
        }
        let main_height = main.tip().height;
        if anchors.max_height.wrapping_add(10) < main_height {
            return not_longer;
        }
        let limit = self.options.config.main_chain_headers_required();
        if candidate_min_height.wrapping_add(limit) < main_height {
            return not_longer;
        }
        if anchors.known.len() * 2 < current_known.len()
            || anchors.max_height < candidate_min_height
        {
            return not_longer;
        }
        if anchors.max_deviation > MAX_TIMESTAMP_DEVIATION {
            return not_longer;
        }
        let span = span80(&mut candidate_timestamps);
        let main_span = (anchors.max_height + 1 - candidate_min_height)
            .wrapping_mul(MAIN_CHAIN_BLOCK_TIME)
            .wrapping_mul(8)
            / 10;
        if span > u64::MAX / 3
            || span * 3 < main_span.wrapping_mul(2)
            || span * 3 > main_span.wrapping_mul(4)
        {
            return not_longer;
        }
        ChainComparison {
            longer: true,
            alternative: true,
        }
    }

    /// C++ `update_chain_tip(block)`: a verified, valid share less than `pplns_window` deep
    /// becomes the tip if it is a longer chain and its next difficulty can be computed; then
    /// old shares are pruned and the seen set cleaned. A share up to `U - 1` below the tip
    /// that is not longer is reported as a possible uncle.
    pub(super) fn update_chain_tip(&mut self, id: Hash, main: &dyn MainChainView) {
        let Some(block) = self.blocks.get(&id) else {
            return;
        };
        if !block.offchain.verified
            || block.offchain.invalid
            || block.offchain.depth >= self.options.config.pplns_window
            || self.tip == Some(id)
        {
            return;
        }
        let tip = self.chain_tip();
        let cmp = self.is_longer_chain(tip, block, main);
        let (height, main_height) = (block.sidechain_height, block.txin_gen_height);
        let tip_height = tip.map(|t| t.sidechain_height);

        if cmp.longer {
            if let Some(next_difficulty) = self.get_difficulty(&id) {
                self.tip = Some(id);
                self.difficulty = next_difficulty;
                if let Some(b) = self.blocks.get_mut(&id) {
                    b.offchain.want_broadcast = true;
                }
                self.events.push(SidechainEvent::NewTip {
                    id,
                    height,
                    next_difficulty,
                    main_height,
                    alternative: cmp.alternative,
                });
                let now = self.now;
                self.prune_old_blocks(now);
                self.cleanup_incoming_blocks(now);
            }
        } else if let Some(t) = tip_height
            && height <= t
            && height + UNCLE_BLOCK_DEPTH > t
        {
            self.events.push(SidechainEvent::PossibleUncle(id));
        }

        if let Some(b) = self.blocks.get_mut(&id)
            && b.offchain.want_broadcast
            && !b.offchain.broadcasted
        {
            b.offchain.broadcasted = true;
            self.events.push(SidechainEvent::Broadcast(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::span80;

    #[test]
    fn span_of_the_middle_80_percent() {
        assert_eq!(span80(&mut []), 0);
        assert_eq!(span80(&mut [5]), 0);
        assert_eq!(span80(&mut [9, 1]), 8);
        let mut v: Vec<u64> = (0..20).rev().map(|x| x * 10).collect();
        // 20 values: cut 2, positions 1 and 18.
        assert_eq!(span80(&mut v), 180 - 10);
    }
}
