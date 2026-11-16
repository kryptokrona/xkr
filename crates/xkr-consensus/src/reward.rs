//! Emission, block-size limits, fees, dust, mixin and fusion rules: C++ `Currency`
//! (`currency.cpp`, `currency.h`), `getPenalizedAmount` (`cryptonote_basic_impl.cpp`),
//! `Mixins` (`mixins.h`), `decompose_amount_into_digits` (`cryptonote_format_utils.h`).

use xkr_types::{Transaction, TxInput};

use crate::params::{
    CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE, CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1,
    CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2, CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE,
    CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS, CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS,
    CRYPTONOTE_MAX_BLOCK_NUMBER, DEFAULT_DUST_THRESHOLD, DEFAULT_DUST_THRESHOLD_V2,
    DEFAULT_MIXIN_V0, DEFAULT_MIXIN_V1, DEFAULT_MIXIN_V2, DEFAULT_MIXIN_V3, DEFAULT_MIXIN_V4,
    DUST_THRESHOLD_V2_HEIGHT, EMISSION_SPEED_FACTOR, FUSION_DUST_THRESHOLD_HEIGHT_V2,
    FUSION_TX_MAX_SIZE, FUSION_TX_MIN_IN_OUT_COUNT_RATIO, FUSION_TX_MIN_INPUT_COUNT,
    GENESIS_BLOCK_REWARD, MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR,
    MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR, MAX_BLOCK_SIZE_INITIAL, MAXIMUM_MIXIN_V1,
    MAXIMUM_MIXIN_V2, MAXIMUM_MIXIN_V3, MAXIMUM_MIXIN_V4, MINIMUM_FEE, MINIMUM_MIXIN_V1,
    MINIMUM_MIXIN_V2, MINIMUM_MIXIN_V3, MINIMUM_MIXIN_V4, MIXIN_LIMITS_V1_HEIGHT,
    MIXIN_LIMITS_V2_HEIGHT, MIXIN_LIMITS_V3_HEIGHT, MIXIN_LIMITS_V4_HEIGHT, MONEY_SUPPLY,
};

/// `Currency::getBlockReward` failed: `currentBlockSize > 2 * max(medianSize, fullRewardZone)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block cumulative size {size} exceeds twice the effective median {median}")]
pub struct BlockTooBig {
    /// Cumulative block size.
    pub size: u64,
    /// Effective median (`max(median, full reward zone)`).
    pub median: u64,
}

/// C++ `Currency::blockGrantedFullRewardZoneByBlockVersion`.
#[must_use]
pub const fn full_reward_zone(major_version: u8) -> usize {
    match major_version {
        3.. => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE,
        2 => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V2,
        _ => CRYPTONOTE_BLOCK_GRANTED_FULL_REWARD_ZONE_V1,
    }
}

/// C++ `getPenalizedAmount(amount, medianSize, currentBlockSize)`:
/// `amount * size * (2 * median - size) / median / median` (128-bit product, two truncating
/// divisions) when `size > median`. Callers guarantee `size <= 2 * median` (C++ asserts);
/// a zero median returns 0 instead of dividing by zero.
#[must_use]
pub fn get_penalized_amount(amount: u64, median_size: u64, current_block_size: u64) -> u64 {
    if amount == 0 {
        return 0;
    }
    if current_block_size <= median_size {
        return amount;
    }
    if median_size == 0 {
        return 0;
    }
    let factor =
        current_block_size.wrapping_mul((2 * median_size).wrapping_sub(current_block_size));
    let product = u128::from(amount) * u128::from(factor);
    let m = u128::from(median_size);
    (product / m / m) as u64
}

/// C++ `Currency::getBlockReward(blockMajorVersion, medianSize, currentBlockSize,
/// alreadyGeneratedCoins, fee, reward, emissionChange)` -> `(reward, emission_change)`.
///
/// `base = (MONEY_SUPPLY - already_generated) >> EMISSION_SPEED_FACTOR`; the median is raised to
/// the full-reward zone of the version; base reward and (for major >= 2) the fee are penalized;
/// `emission_change = penalized_base - (fee - penalized_fee)` computed in `u64` and reinterpreted
/// as `i64` like C++.
pub fn get_block_reward(
    major_version: u8,
    median_size: u64,
    current_block_size: u64,
    already_generated_coins: u64,
    fee: u64,
) -> Result<(u64, i64), BlockTooBig> {
    let mut base_reward =
        MONEY_SUPPLY.wrapping_sub(already_generated_coins) >> EMISSION_SPEED_FACTOR;
    if already_generated_coins == 0 && GENESIS_BLOCK_REWARD != 0 {
        base_reward = GENESIS_BLOCK_REWARD;
    }
    let median = median_size.max(full_reward_zone(major_version) as u64);
    if current_block_size > 2 * median {
        return Err(BlockTooBig {
            size: current_block_size,
            median,
        });
    }
    let penalized_base = get_penalized_amount(base_reward, median, current_block_size);
    let penalized_fee = if major_version >= 2 {
        get_penalized_amount(fee, median, current_block_size)
    } else {
        fee
    };
    let emission_change = penalized_base.wrapping_sub(fee.wrapping_sub(penalized_fee)) as i64;
    Ok((penalized_base.wrapping_add(penalized_fee), emission_change))
}

/// C++ `Currency::maxBlockCumulativeSize(height)`: `MAX_BLOCK_SIZE_INITIAL + height * 100 KiB /
/// blocks-per-year`.
#[must_use]
pub fn max_block_cumulative_size(height: u64) -> u64 {
    MAX_BLOCK_SIZE_INITIAL as u64
        + height.wrapping_mul(MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR)
            / MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR
}

/// C++ `Core::calculateCumulativeBlocksizeLimit`: `2 * max(median, full reward zone)`.
#[must_use]
pub fn cumulative_block_size_limit(next_major_version: u8, median_size: u64) -> u64 {
    2 * median_size.max(full_reward_zone(next_major_version) as u64)
}

/// C++ `getMaximumTransactionAllowedSize` (`core.cpp`, pool policy):
/// `median * 2 - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE`.
#[must_use]
pub fn max_transaction_allowed_size(block_size_median: u64) -> u64 {
    (block_size_median * 2).saturating_sub(CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64)
}

/// C++ `common::medianValue`: sorts a copy; mean of the two middle values for even lengths
/// (integer division); 0 for an empty slice.
#[must_use]
pub fn median(values: &[u64]) -> u64 {
    match values.len() {
        0 => 0,
        1 => values[0],
        n => {
            let mut v = values.to_vec();
            v.sort_unstable();
            let h = n / 2;
            if n % 2 == 1 {
                v[h]
            } else {
                v[h - 1].wrapping_add(v[h]) / 2
            }
        }
    }
}

/// C++ `Currency::minimumFee`.
#[must_use]
pub const fn minimum_fee() -> u64 {
    MINIMUM_FEE
}

/// C++ `Currency::defaultDustThreshold(height)`.
#[must_use]
pub const fn default_dust_threshold(height: u32) -> u64 {
    if height >= DUST_THRESHOLD_V2_HEIGHT {
        DEFAULT_DUST_THRESHOLD_V2
    } else {
        DEFAULT_DUST_THRESHOLD
    }
}

/// C++ `Currency::defaultFusionDustThreshold(height)`.
#[must_use]
pub const fn default_fusion_dust_threshold(height: u32) -> u64 {
    if height >= FUSION_DUST_THRESHOLD_HEIGHT_V2 {
        DEFAULT_DUST_THRESHOLD_V2
    } else {
        DEFAULT_DUST_THRESHOLD
    }
}

/// C++ `decompose_amount_into_digits` collecting chunks and dust in emission order
/// (`decomposeAmount`).
#[must_use]
pub fn decompose_amount(amount: u64, dust_threshold: u64) -> Vec<u64> {
    let mut out = Vec::new();
    if amount == 0 {
        return out;
    }
    let mut amount = amount;
    let mut is_dust_handled = false;
    let mut dust: u64 = 0;
    let mut order: u64 = 1;
    while amount != 0 {
        let chunk = (amount % 10).wrapping_mul(order);
        amount /= 10;
        order = order.wrapping_mul(10);
        if dust.wrapping_add(chunk) <= dust_threshold {
            dust = dust.wrapping_add(chunk);
        } else {
            if !is_dust_handled && dust != 0 {
                out.push(dust);
                is_dust_handled = true;
            }
            if chunk != 0 {
                out.push(chunk);
            }
        }
    }
    if !is_dust_handled && dust != 0 {
        out.push(dust);
    }
    out
}

/// C++ `Currency::isFusionTransaction(inputsAmounts, outputsAmounts, size, height)`.
/// `outputs` must already be in ascending order to match (C++ compares with the sorted
/// decomposition, not with a sorted copy of the outputs).
#[must_use]
pub fn is_fusion_transaction_amounts(
    inputs: &[u64],
    outputs: &[u64],
    size: usize,
    height: u32,
) -> bool {
    if size > FUSION_TX_MAX_SIZE {
        return false;
    }
    if inputs.len() < FUSION_TX_MIN_INPUT_COUNT {
        return false;
    }
    if inputs.len() < outputs.len() * FUSION_TX_MIN_IN_OUT_COUNT_RATIO {
        return false;
    }
    let threshold = default_fusion_dust_threshold(height);
    let mut input_amount: u64 = 0;
    for a in inputs {
        if *a < threshold {
            return false;
        }
        input_amount = input_amount.wrapping_add(*a);
    }
    let mut expected = decompose_amount(input_amount, threshold);
    expected.sort_unstable();
    expected == outputs
}

/// C++ `Currency::isFusionTransaction(transaction, size, height)` (`size` = serialized length).
#[must_use]
pub fn is_fusion_transaction(tx: &Transaction, size: usize, height: u32) -> bool {
    let inputs: Vec<u64> = tx
        .prefix
        .inputs
        .iter()
        .filter_map(|i| match i {
            TxInput::Key { amount, .. } => Some(*amount),
            TxInput::Coinbase { .. } => None,
        })
        .collect();
    let outputs: Vec<u64> = tx.prefix.outputs.iter().map(|o| o.amount).collect();
    is_fusion_transaction_amounts(&inputs, &outputs, size, height)
}

/// Allowed mixin range at a height (C++ `Mixins::getMixinAllowableRange`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixinLimits {
    /// Minimum mixin (ring size - 1).
    pub min: u64,
    /// Maximum mixin.
    pub max: u64,
    /// Wallet default.
    pub default: u64,
}

/// C++ `Mixins::getMixinAllowableRange(height)`.
#[must_use]
pub const fn mixin_limits(height: u64) -> MixinLimits {
    if height >= MIXIN_LIMITS_V4_HEIGHT as u64 {
        MixinLimits {
            min: MINIMUM_MIXIN_V4,
            max: MAXIMUM_MIXIN_V4,
            default: DEFAULT_MIXIN_V4,
        }
    } else if height >= MIXIN_LIMITS_V3_HEIGHT as u64 {
        MixinLimits {
            min: MINIMUM_MIXIN_V3,
            max: MAXIMUM_MIXIN_V3,
            default: DEFAULT_MIXIN_V3,
        }
    } else if height >= MIXIN_LIMITS_V2_HEIGHT as u64 {
        MixinLimits {
            min: MINIMUM_MIXIN_V2,
            max: MAXIMUM_MIXIN_V2,
            default: DEFAULT_MIXIN_V2,
        }
    } else if height >= MIXIN_LIMITS_V1_HEIGHT as u64 {
        MixinLimits {
            min: MINIMUM_MIXIN_V1,
            max: MAXIMUM_MIXIN_V1,
            default: DEFAULT_MIXIN_V1,
        }
    } else {
        MixinLimits {
            min: 0,
            max: u64::MAX,
            default: DEFAULT_MIXIN_V0,
        }
    }
}

/// Mixin outside the allowed range (C++ `Mixins::validate(transaction, min, max)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MixinError {
    /// "transaction mixin is too large".
    #[error("transaction mixin {mixin} above maximum {max}")]
    TooLarge {
        /// Mixin of the transaction.
        mixin: u64,
        /// Allowed maximum.
        max: u64,
    },
    /// "transaction mixin is too small".
    #[error("transaction mixin {mixin} below minimum {min}")]
    TooSmall {
        /// Mixin of the transaction.
        mixin: u64,
        /// Allowed minimum.
        min: u64,
    },
}

/// C++ `Mixins::validate(const CachedTransaction&, minMixin, maxMixin)`: mixin is the largest
/// ring size over key inputs minus one (ring size starts at 1).
pub fn validate_mixin(tx: &Transaction, limits: MixinLimits) -> Result<(), MixinError> {
    let mixin = tx.max_ring_size() as u64 - 1;
    if mixin > limits.max {
        return Err(MixinError::TooLarge {
            mixin,
            max: limits.max,
        });
    }
    if mixin < limits.min {
        return Err(MixinError::TooSmall {
            mixin,
            min: limits.min,
        });
    }
    Ok(())
}

/// C++ `Mixins::validate(transactions, height)`: every transaction against the limits at
/// `height`.
pub fn validate_mixins(txs: &[Transaction], height: u64) -> Result<(), MixinError> {
    let limits = mixin_limits(height);
    txs.iter().try_for_each(|tx| validate_mixin(tx, limits))
}

/// C++ `isTransactionSpendTimeUnlocked(unlockTime, blockIndex)`
/// (`database_blockchain_cache.cpp` / `blockchain_cache.cpp`): below `CRYPTONOTE_MAX_BLOCK_NUMBER`
/// the unlock time is a block index (`block_index + 1 >= unlock_time`), otherwise a unix time
/// compared with the caller-supplied wall clock (`now + 90 >= unlock_time`).
#[must_use]
pub fn is_spend_time_unlocked(unlock_time: u64, block_index: u32, now: u64) -> bool {
    if unlock_time < u64::from(CRYPTONOTE_MAX_BLOCK_NUMBER) {
        u64::from(block_index) + CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS >= unlock_time
    } else {
        now + CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS >= unlock_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reward_examples_from_mainnet() {
        // block 1: nothing generated yet except the genesis coinbase (2 980 232)
        assert_eq!(
            get_block_reward(1, 0, 368, 2_980_232, 0).unwrap().0,
            47_683_714
        );
        // block 2 617 400 (f_block_json): median 735, size 19 932, generated before it
        // 71 291 741 161 622 - 13 689 171.
        let r = get_block_reward(5, 735, 19_932, 71_291_741_161_622 - 13_689_171, 0).unwrap();
        assert_eq!(r, (13_689_171, 13_689_171));
    }

    #[test]
    fn penalty() {
        assert_eq!(get_penalized_amount(1000, 100_000, 100_000), 1000);
        assert_eq!(get_penalized_amount(1000, 100_000, 200_000), 0);
        assert_eq!(get_penalized_amount(1000, 100_000, 150_000), 750);
        assert!(get_block_reward(5, 0, 200_001, 0, 0).is_err());
    }

    #[test]
    fn decomposition() {
        assert_eq!(
            decompose_amount(1_234_567, 0),
            vec![7, 60, 500, 4_000, 30_000, 200_000, 1_000_000]
        );
        assert_eq!(
            decompose_amount(1_234_567, 10),
            vec![7, 60, 500, 4_000, 30_000, 200_000, 1_000_000]
        );
        assert_eq!(
            decompose_amount(1_234_567, 100),
            vec![67, 500, 4_000, 30_000, 200_000, 1_000_000]
        );
        assert_eq!(decompose_amount(0, 10), Vec::<u64>::new());
    }

    #[test]
    fn mixin_ranges() {
        assert_eq!(mixin_limits(439_999).min, 0);
        assert_eq!(mixin_limits(440_000).max, 100);
        assert_eq!(
            mixin_limits(620_000),
            MixinLimits {
                min: 7,
                max: 7,
                default: 7
            }
        );
        assert_eq!(
            mixin_limits(800_000),
            MixinLimits {
                min: 3,
                max: 3,
                default: 3
            }
        );
        assert_eq!(
            mixin_limits(1_250_000),
            MixinLimits {
                min: 1,
                max: 5,
                default: 5
            }
        );
    }

    #[test]
    fn unlock_rules() {
        assert!(is_spend_time_unlocked(21, 20, 0));
        assert!(!is_spend_time_unlocked(22, 20, 0));
        assert!(is_spend_time_unlocked(1_600_000_000, 0, 1_600_000_000 - 90));
        assert!(!is_spend_time_unlocked(
            1_600_000_000,
            0,
            1_600_000_000 - 91
        ));
    }

    #[test]
    fn medians() {
        assert_eq!(median(&[]), 0);
        assert_eq!(median(&[5]), 5);
        assert_eq!(median(&[3, 1, 2]), 2);
        assert_eq!(median(&[4, 1, 2, 3]), 2);
        assert_eq!(max_block_cumulative_size(0), 100_000);
        assert_eq!(max_block_cumulative_size(350_400), 100_000 + 102_400);
    }
}
