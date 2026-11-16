//! Difficulty: C++ `Currency::getNextDifficulty` / `Currency::nextDifficulty`
//! (`currency.cpp`) and `nextDifficultyV3..V6` (`difficulty.cpp`), with the window selection of
//! `DatabaseBlockchainCache::getDifficultyForNextBlock`.
//!
//! All arithmetic mirrors the C++ types: `u64` with wrapping where C++ uses unsigned
//! arithmetic, `i64` (wrapping) for LWMA-2, truncating division. Where C++ would divide by zero
//! (undefined behaviour, never seen on mainnet) these functions return 0, which callers treat
//! like C++'s `DIFFICULTY_OVERHEAD`.

use core::ops::Range;

use crate::forks::{FORK_LWMA_2, FORK_LWMA_2_V2, FORK_LWMA_2_V3, FORK_LWMA_3};
use crate::params::{
    CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3, DIFFICULTY_BLOCKS_COUNT_V3, DIFFICULTY_CUT,
    DIFFICULTY_CUT_V1, DIFFICULTY_CUT_V2, DIFFICULTY_LAG, DIFFICULTY_LAG_V1, DIFFICULTY_LAG_V2,
    DIFFICULTY_TARGET, DIFFICULTY_WINDOW, DIFFICULTY_WINDOW_V1, DIFFICULTY_WINDOW_V2,
    DIFFICULTY_WINDOW_V3, LWMA_2_DIFFICULTY_BLOCK_INDEX, NetworkParams,
    ZAWY_DIFFICULTY_BLOCK_INDEX, ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION, ZAWY_DIFFICULTY_V2,
};
use crate::version::block_major_version_for_height;

/// C++ `Currency::difficultyWindowByBlockVersion`.
#[must_use]
pub const fn difficulty_window_by_version(major_version: u8) -> usize {
    match major_version {
        3.. => DIFFICULTY_WINDOW,
        2 => DIFFICULTY_WINDOW_V2,
        _ => DIFFICULTY_WINDOW_V1,
    }
}

/// C++ `Currency::difficultyLagByBlockVersion`.
#[must_use]
pub const fn difficulty_lag_by_version(major_version: u8) -> usize {
    match major_version {
        3.. => DIFFICULTY_LAG,
        2 => DIFFICULTY_LAG_V2,
        _ => DIFFICULTY_LAG_V1,
    }
}

/// C++ `Currency::difficultyCutByBlockVersion`.
#[must_use]
pub const fn difficulty_cut_by_version(major_version: u8) -> usize {
    match major_version {
        3.. => DIFFICULTY_CUT,
        2 => DIFFICULTY_CUT_V2,
        _ => DIFFICULTY_CUT_V1,
    }
}

/// C++ `Currency::difficultyBlocksCountByBlockVersion(version, height)`; `height` is the
/// **previous** block index, as passed by `getDifficultyForNextBlock`.
#[must_use]
pub const fn difficulty_blocks_count(major_version: u8, height: u32) -> usize {
    if height as u64 >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        return DIFFICULTY_BLOCKS_COUNT_V3 as usize;
    }
    difficulty_window_by_version(major_version) + difficulty_lag_by_version(major_version)
}

/// Block heights whose timestamps and cumulative difficulties feed the difficulty of the block
/// at `next_height` (C++ `getDifficultyForNextBlock(next_height - 1)` with
/// `getLastUnits(count, next_height - 1, UseGenesis{false})`): the last `count` blocks ending at
/// `next_height - 1`, never including the genesis block.
#[must_use]
pub fn difficulty_window(params: &NetworkParams, next_height: u32) -> Range<u32> {
    if next_height == 0 {
        return 0..0;
    }
    let prev = next_height - 1;
    let version = block_major_version_for_height(params, next_height);
    let count = difficulty_blocks_count(version, prev) as u32;
    let start = next_height.saturating_sub(count).max(1);
    start..next_height
}

/// Difficulty of the block at `next_height` given the timestamps and cumulative difficulties
/// of [`difficulty_window`] (oldest first). C++ `getDifficultyForNextBlock`.
#[must_use]
pub fn difficulty_for_next_block(
    params: &NetworkParams,
    next_height: u32,
    timestamps: &[u64],
    cumulative_difficulties: &[u64],
) -> u64 {
    let version = block_major_version_for_height(params, next_height);
    next_difficulty(
        params,
        version,
        next_height.saturating_sub(1),
        timestamps,
        cumulative_difficulties,
    )
}

/// C++ `Currency::getNextDifficulty(version, blockIndex, timestamps, cumulativeDifficulties)`.
/// `block_index` is the index of the **previous** block.
///
/// The algorithm comes from the fork table: C++ compares the previous index with
/// `LWMA_*_INDEX` (`blockIndex >= F`), which is [`crate::forks::ForkSchedule::is_fork_active`]
/// for the new block (`blockIndex + 1 > F`).
#[must_use]
pub fn next_difficulty(
    params: &NetworkParams,
    version: u8,
    block_index: u32,
    timestamps: &[u64],
    cumulative_difficulties: &[u64],
) -> u64 {
    if params.fixed_difficulty != 0 {
        return params.fixed_difficulty;
    }
    let forks = params.forks();
    let next = u64::from(block_index) + 1;
    if forks.is_fork_active(FORK_LWMA_3, next) {
        next_difficulty_v6(timestamps, cumulative_difficulties)
    } else if forks.is_fork_active(FORK_LWMA_2_V3, next) {
        next_difficulty_v5(timestamps, cumulative_difficulties)
    } else if forks.is_fork_active(FORK_LWMA_2_V2, next) {
        next_difficulty_v4(timestamps, cumulative_difficulties)
    } else if forks.is_fork_active(FORK_LWMA_2, next) {
        next_difficulty_v3(timestamps, cumulative_difficulties)
    } else {
        next_difficulty_original(version, block_index, timestamps, cumulative_difficulties)
    }
}

/// `(totalWork * T, high word)`.
fn mul128(a: u64, b: u64) -> (u64, u64) {
    let r = u128::from(a) * u128::from(b);
    (r as u64, (r >> 64) as u64)
}

/// The cut/sort/span computation shared by both halves of C++ `Currency::nextDifficulty`.
/// Returns `Err(result)` for the early returns (`1` for short windows, `0` on overflow).
fn span_and_work(
    window: usize,
    cut: usize,
    timestamps: &[u64],
    cumulative: &[u64],
) -> Result<(u64, u64, u64), u64> {
    let length = timestamps.len().min(cumulative.len());
    if length <= 1 {
        return Err(1);
    }
    let mut sorted = timestamps[..length].to_vec();
    sorted.sort_unstable();
    let (cut_begin, cut_end) = if length <= window - 2 * cut {
        (0, length)
    } else {
        let b = (length - (window - 2 * cut)).div_ceil(2);
        (b, b + (window - 2 * cut))
    };
    let mut time_span = sorted[cut_end - 1].wrapping_sub(sorted[cut_begin]);
    if time_span == 0 {
        time_span = 1;
    }
    let total_work = cumulative[cut_end - 1].wrapping_sub(cumulative[cut_begin]);
    let (low, high) = mul128(total_work, DIFFICULTY_TARGET);
    if high != 0 || u64::MAX - low < time_span - 1 {
        return Err(0);
    }
    Ok((low, high, time_span))
}

/// C++ `Currency::nextDifficulty` (the pre-LWMA algorithm, including both Zawy variants).
///
/// For block versions >= `ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION` (3) it returns
/// `work * T / span` over the 17-block window right away, so the `ZAWY_DIFFICULTY_BLOCK_INDEX`
/// branch is only reachable for v1/v2 blocks (never on mainnet after height 3).
#[must_use]
pub fn next_difficulty_original(
    version: u8,
    block_index: u32,
    timestamps: &[u64],
    cumulative_difficulties: &[u64],
) -> u64 {
    let window = difficulty_window_by_version(version);
    let cut = difficulty_cut_by_version(version);
    let n = timestamps
        .len()
        .min(cumulative_difficulties.len())
        .min(window);
    let (low, high, time_span) =
        match span_and_work(window, cut, &timestamps[..n], &cumulative_difficulties[..n]) {
            Ok(v) => v,
            Err(v) => return v,
        };

    let zawy_version = if ZAWY_DIFFICULTY_V2 != 0 {
        2
    } else {
        ZAWY_DIFFICULTY_DIFFICULTY_BLOCK_VERSION
    };
    if version >= zawy_version && zawy_version != 0 {
        if high != 0 {
            return 0;
        }
        return low / time_span;
    }

    if ZAWY_DIFFICULTY_BLOCK_INDEX != 0 && ZAWY_DIFFICULTY_BLOCK_INDEX <= block_index {
        // Hard-coded DIFFICULTY_WINDOW = 17, CUT = 0, LAG = 0 over the *last* 17 entries of
        // the original (untruncated) vectors.
        let all = timestamps.len().min(cumulative_difficulties.len());
        let t_window = 17.min(n);
        let start = all - t_window;
        let (low, high, time_span) = match span_and_work(
            17,
            0,
            &timestamps[start..all],
            &cumulative_difficulties[start..all],
        ) {
            Ok(v) => v,
            Err(v) => return v,
        };
        if high != 0 {
            return 0;
        }
        return (low / time_span).max(100);
    }

    low.div_ceil(time_span)
}

/// C++ `nextDifficultyV6` (LWMA-3, block index >= 2 200 000). Unsigned arithmetic.
#[must_use]
pub fn next_difficulty_v6(timestamps: &[u64], cumulative_difficulties: &[u64]) -> u64 {
    let t = DIFFICULTY_TARGET;
    let len = timestamps.len().min(cumulative_difficulties.len());
    if len <= 10 {
        return 10_000;
    }
    let mut n = DIFFICULTY_WINDOW_V3;
    if (len as u64) < DIFFICULTY_BLOCKS_COUNT_V3 {
        n = len as u64 - 1;
    }
    let ni = n as usize;
    let mut l: u64 = 0;
    let mut sum_3_st: u64 = 0;
    let mut previous = timestamps[0];
    for (i, &ts) in timestamps.iter().enumerate().take(ni + 1).skip(1) {
        let this = if ts > previous {
            ts
        } else {
            previous.wrapping_add(1)
        };
        let st = (6 * t).min(this.wrapping_sub(previous));
        previous = this;
        l = l.wrapping_add(st.wrapping_mul(i as u64));
        if i as u64 > n - 3 {
            sum_3_st = sum_3_st.wrapping_add(st);
        }
    }
    let total = cumulative_difficulties[ni].wrapping_sub(cumulative_difficulties[0]);
    let numerator = total.wrapping_mul(t).wrapping_mul(n + 1).wrapping_mul(99);
    let denominator = 100u64.wrapping_mul(2).wrapping_mul(l);
    let mut next_d = numerator.checked_div(denominator).unwrap_or(0);
    let prev_d = cumulative_difficulties[ni].wrapping_sub(cumulative_difficulties[ni - 1]);
    next_d = (prev_d.wrapping_mul(67) / 100).max(next_d.min(prev_d.wrapping_mul(150) / 100));
    if sum_3_st < (8 * t) / 10 {
        next_d = next_d.max(prev_d.wrapping_mul(108) / 100);
    }
    next_d
}

/// Which LWMA-2 flavour (they differ in solve-time clamping and the fast-rise rule).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lwma2 {
    V3,
    V4,
    V5,
}

fn lwma2(timestamps: &[u64], cumulative_difficulties: &[u64], flavour: Lwma2) -> u64 {
    let t = DIFFICULTY_TARGET as i64;
    let n = DIFFICULTY_WINDOW_V3 as i64;
    let ftl = CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3 as i64;
    let len = timestamps.len().min(cumulative_difficulties.len()) as u64;
    match flavour {
        Lwma2::V5 if len < (n + 1) as u64 => return 10_000,
        Lwma2::V3 | Lwma2::V4 if len <= n as u64 => return 1_000,
        _ => {}
    }
    let ni = n as usize;
    let mut l: i64 = 0;
    let mut sum_3_st: i64 = 0;
    for i in 1..=ni {
        let raw = (timestamps[i] as i64).wrapping_sub(timestamps[i - 1] as i64);
        let st = match flavour {
            // std::max(-FTL, std::min(ST, 6 * T))
            Lwma2::V3 => (-ftl).max(raw.min(6 * t)),
            // clamp(-6 * T, ST, 6 * T) with TurtleCoin's clamp(n, lower, upper) =
            // max(lower, min(n, upper)) = max(ST, -6T): the upper bound is never applied.
            Lwma2::V4 => raw.max((-6 * t).min(6 * t)),
            // std::max(-4 * T, std::min(ST, 6 * T))
            Lwma2::V5 => (-4 * t).max(raw.min(6 * t)),
        };
        l = l.wrapping_add(st.wrapping_mul(i as i64));
        if i as i64 > n - 3 {
            sum_3_st = sum_3_st.wrapping_add(st);
        }
    }
    let total = cumulative_difficulties[ni].wrapping_sub(cumulative_difficulties[0]) as i64;
    let numerator = total.wrapping_mul(t).wrapping_mul(n + 1).wrapping_mul(99);
    let mut next_d = numerator.checked_div(100 * 2 * l).unwrap_or(0);
    let prev_d = cumulative_difficulties[ni].wrapping_sub(cumulative_difficulties[ni - 1]) as i64;
    match flavour {
        Lwma2::V3 => {
            next_d =
                (prev_d.wrapping_mul(70) / 100).max(next_d.min(prev_d.wrapping_mul(107) / 100));
            if sum_3_st < (8 * t) / 10 {
                next_d = prev_d.wrapping_mul(110) / 100;
            }
        }
        Lwma2::V4 | Lwma2::V5 => {
            next_d =
                (prev_d.wrapping_mul(67) / 100).max(next_d.min(prev_d.wrapping_mul(150) / 100));
            let bump = if flavour == Lwma2::V4 { 110 } else { 108 };
            if sum_3_st < (8 * t) / 10 {
                next_d = next_d.max(prev_d.wrapping_mul(bump) / 100);
            }
        }
    }
    next_d as u64
}

/// C++ `nextDifficultyV5` (LWMA-2, block index >= 800 000).
#[must_use]
pub fn next_difficulty_v5(timestamps: &[u64], cumulative_difficulties: &[u64]) -> u64 {
    lwma2(timestamps, cumulative_difficulties, Lwma2::V5)
}

/// C++ `nextDifficultyV4` (LWMA-2, block index >= 700 000).
#[must_use]
pub fn next_difficulty_v4(timestamps: &[u64], cumulative_difficulties: &[u64]) -> u64 {
    lwma2(timestamps, cumulative_difficulties, Lwma2::V4)
}

/// C++ `nextDifficultyV3` (LWMA-2, block index >= 620 000).
#[must_use]
pub fn next_difficulty_v3(timestamps: &[u64], cumulative_difficulties: &[u64]) -> u64 {
    lwma2(timestamps, cumulative_difficulties, Lwma2::V3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::MAINNET;

    #[test]
    fn windows() {
        assert_eq!(difficulty_window(&MAINNET, 1), 1..1);
        assert_eq!(difficulty_window(&MAINNET, 2), 1..2);
        assert_eq!(difficulty_window(&MAINNET, 1000), 983..1000);
        assert_eq!(difficulty_window(&MAINNET, 620_000), 619_983..620_000);
        assert_eq!(difficulty_window(&MAINNET, 620_001), 619_940..620_001);
        assert_eq!(difficulty_window(&MAINNET, 2_617_000), 2_616_939..2_617_000);
    }

    #[test]
    fn short_windows() {
        assert_eq!(next_difficulty_original(5, 1, &[], &[]), 1);
        assert_eq!(next_difficulty_v6(&[1; 10], &[1; 10]), 10_000);
        assert_eq!(next_difficulty_v5(&[1; 60], &[1; 60]), 10_000);
        assert_eq!(next_difficulty_v4(&[1; 60], &[1; 60]), 1_000);
    }

    #[test]
    fn steady_chain_keeps_difficulty() {
        let ts: Vec<u64> = (0..61).map(|i| 1_000_000 + i * DIFFICULTY_TARGET).collect();
        let cd: Vec<u64> = (0..61).map(|i| 1_000 * (i + 1)).collect();
        // LWMA: (60_000 * 90 * 61 * 99) / (200 * 90 * 1830) = 990
        assert_eq!(next_difficulty_v6(&ts, &cd), 990);
        assert_eq!(next_difficulty_v5(&ts, &cd), 990);
    }
}
