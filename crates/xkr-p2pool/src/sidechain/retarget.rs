//! Difficulty of the next share (`SideChain::get_difficulty`, `src/side_chain.cpp`).
//!
//! The `(timestamp, cumulative difficulty)` pairs of the last `pplns_window` shares below and
//! including the tip, plus their uncles that lie inside the window, are sorted by time; the
//! oldest and newest 10 % are cut, and the work done between the remaining first and last
//! timestamp, divided by that time span, times the target share time gives the difficulty
//! (at least the sidechain minimum).

use xkr_types::Hash;

use super::SideChain;
use crate::block::PoolBlock;
use crate::difficulty::Difficulty;

/// One share's time and cumulative difficulty (C++ `DifficultyData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifficultyData {
    /// Share timestamp.
    pub timestamp: u64,
    /// Cumulative difficulty up to and including the share.
    pub cumulative_difficulty: Difficulty,
}

/// The arithmetic of C++ `get_difficulty` over collected pairs.
///
/// With `n` pairs and `cut = ceil(n / 10)`: `t1` / `t2` are the `cut - 1`-th and `n - cut`-th
/// smallest timestamps; `delta_t = max(t2 - t1, max(n - 2 cut + 1, 1))`; over the pairs with
/// `t1 <= timestamp <= t2`, `diff1` / `diff2` are the lowest and highest cumulative
/// difficulty; the result is `(diff2 - diff1) * block_time / delta_t` (product modulo
/// `2^128`), at least `min_difficulty`. `None` for no pairs or a timestamp more than
/// `u32::MAX` seconds after the oldest.
#[must_use]
pub fn next_difficulty(
    data: &[DifficultyData],
    block_time: u64,
    min_difficulty: Difficulty,
) -> Option<Difficulty> {
    let oldest = data.iter().map(|d| d.timestamp).min()?;
    let mut deltas: Vec<u32> = data
        .iter()
        .map(|d| u32::try_from(d.timestamp - oldest).ok())
        .collect::<Option<_>>()?;
    let n = deltas.len();
    let cut = n.div_ceil(10);
    let index1 = cut - 1;
    let index2 = n - cut;
    deltas.sort_unstable();
    let timestamp1 = oldest + u64::from(deltas[index1]);
    let timestamp2 = oldest + u64::from(deltas[index2]);
    let delta_index = if index2 > index1 {
        (index2 - index1) as u64
    } else {
        1
    };
    let delta_t = timestamp2.saturating_sub(timestamp1).max(delta_index);
    let mut diff1 = Difficulty::MAX;
    let mut diff2 = Difficulty::ZERO;
    for d in data {
        if (timestamp1..=timestamp2).contains(&d.timestamp) {
            diff1 = diff1.min(d.cumulative_difficulty);
            diff2 = diff2.max(d.cumulative_difficulty);
        }
    }
    let next = diff2
        .wrapping_sub(diff1)
        .wrapping_mul_u64(block_time)
        .checked_div_u64(delta_t)?;
    Some(next.max(min_difficulty))
}

impl SideChain {
    /// The pairs C++ `get_difficulty` collects below `tip`: for at most `pplns_window` shares
    /// (stopping at height 0) the share itself and each of its uncles less than
    /// `pplns_window` below `tip`. `None` if a parent or uncle is not stored.
    #[must_use]
    pub fn difficulty_data(&self, tip: &PoolBlock) -> Option<Vec<DifficultyData>> {
        let window = self.options.config.pplns_window;
        let mut data = Vec::new();
        let mut cur = tip;
        let mut depth = 0u64;
        loop {
            data.push(DifficultyData {
                timestamp: cur.timestamp,
                cumulative_difficulty: cur.cumulative_difficulty,
            });
            for uncle_id in &cur.uncles {
                let uncle = self.blocks.get(uncle_id)?;
                if tip.sidechain_height.wrapping_sub(uncle.sidechain_height) < window {
                    data.push(DifficultyData {
                        timestamp: uncle.timestamp,
                        cumulative_difficulty: uncle.cumulative_difficulty,
                    });
                }
            }
            depth += 1;
            if depth >= window || cur.sidechain_height == 0 {
                break;
            }
            cur = self.blocks.get(&cur.parent)?;
        }
        Some(data)
    }

    /// The difficulty of a share built on `tip`: its cached value, or computed from
    /// [`Self::difficulty_data`] with [`next_difficulty`]. Does not store it.
    #[must_use]
    pub fn compute_difficulty(&self, tip: &PoolBlock) -> Option<Difficulty> {
        if let Some(d) = tip.offchain.cached_next_difficulty {
            return Some(d);
        }
        let data = self.difficulty_data(tip)?;
        next_difficulty(
            &data,
            self.options.config.block_time,
            self.options.config.min_difficulty(),
        )
    }

    /// C++ `get_difficulty(tip)`: [`Self::compute_difficulty`] for a stored share, cached in
    /// its `cached_next_difficulty`.
    pub fn get_difficulty(&mut self, id: &Hash) -> Option<Difficulty> {
        let d = self.compute_difficulty(self.blocks.get(id)?)?;
        if let Some(b) = self.blocks.get_mut(id) {
            b.offchain.cached_next_difficulty = Some(d);
        }
        Some(d)
    }
}
