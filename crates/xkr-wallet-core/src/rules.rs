//! Network policy a wallet applies before it builds a transaction: fee, mixin, transaction
//! version, unlock times and size limits.
//!
//! These follow today's network (transaction version 1, `MINIMUM_FEE`, the mixin limits of
//! `src/cryptonote_core/mixins.h`). Version 2 (fork 14) is chosen at run time from the daemon's
//! `tx_versions` ([`tx_version_for`]); no fork height is compiled in here.
//!
//! "Height" below is what the C++ wallets pass: the daemon's block count (`/info`
//! `network_height`, `/getheight` `height`), i.e. the top block index + 1.

use xkr_consensus::RingSizeRule;
use xkr_consensus::params::{
    BLOCK_MAJOR_VERSION_7, CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE,
    CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS, CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS,
    CRYPTONOTE_MAX_BLOCK_NUMBER, MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR,
    MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR, MAX_BLOCK_SIZE_INITIAL, MINIMUM_FEE,
};
use xkr_consensus::reward::{default_dust_threshold, mixin_limits};

use crate::transfer::TxVersion;

/// `y` of `utilities::getMaxTxSize`: the block size the wallet plans for at most.
const MAX_TX_SIZE_CAP: u64 = 125_000;

/// A transaction parameter the network would refuse. [`PolicyError::code`] is the C++
/// `WalletErrorCode`; the messages are the C++ ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    /// `FEE_TOO_SMALL` (17): below [`minimum_fee`].
    FeeTooSmall,
    /// `MIXIN_TOO_SMALL` (21).
    MixinTooSmall {
        /// Mixin asked for.
        mixin: u64,
        /// Smallest allowed.
        min: u64,
    },
    /// `MIXIN_TOO_BIG` (22).
    MixinTooBig {
        /// Mixin asked for.
        mixin: u64,
        /// Largest allowed.
        max: u64,
    },
    /// `TOO_MANY_INPUTS_TO_FIT_IN_BLOCK` (32): the serialized transaction is larger than
    /// [`max_tx_size`].
    TooLarge {
        /// Size of the transaction in bytes.
        size: u64,
        /// [`max_tx_size`] at the height.
        max: u64,
    },
}

impl PolicyError {
    /// The C++ error code (`src/errors/errors.h` `WalletErrorCode`).
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::FeeTooSmall => 17,
            Self::MixinTooSmall { .. } => 21,
            Self::MixinTooBig { .. } => 22,
            Self::TooLarge { .. } => 32,
        }
    }
}

impl std::fmt::Display for PolicyError {
    /// `src/errors/errors.cpp` `getErrorMessage`, `validate_parameters.cpp` `validateMixin`
    /// and `wallet_backend/transfer.cpp` `isTransactionPayloadTooBig`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeeTooSmall => f.write_str(
                "The fee given for this transaction is below the minimum allowed network fee.",
            ),
            Self::MixinTooSmall { mixin, min } => write!(
                f,
                "The mixin value given ({mixin}) is lower than the minimum mixin allowed ({min})"
            ),
            Self::MixinTooBig { mixin, max } => write!(
                f,
                "The mixin value given ({mixin}) is greater than the maximum mixin allowed ({max})"
            ),
            Self::TooLarge { size, max } => write!(
                f,
                "Transaction is too large: ({}). Max allowed size is {}. Decrease the amount you \
                 are sending, or perform some fusion transactions.",
                pretty_print_bytes(*size),
                pretty_print_bytes(*max)
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

/// `src/utilities/format_tools.cpp` `prettyPrintBytes`: `12.34 KB` (two decimals, 1024 steps,
/// up to `TB`).
#[must_use]
#[allow(clippy::cast_precision_loss)] // two decimals of a display value
pub fn pretty_print_bytes(bytes: u64) -> String {
    const SUFFIXES: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut suffix = 0;
    while value >= 1024.0 && suffix < SUFFIXES.len() - 1 {
        suffix += 1;
        value /= 1024.0;
    }
    format!("{value:.2} {}", SUFFIXES[suffix])
}

// ---------------------------------------------------------------------------------------------
// fee

/// `Currency::minimumFee` / `MINIMUM_FEE`: the smallest fee of a normal transaction (fusion
/// transactions pay none).
#[must_use]
pub const fn minimum_fee() -> u64 {
    MINIMUM_FEE
}

/// `src/errors/validate_parameters.cpp` `validateAmount`: the fee must be at least
/// [`minimum_fee`].
pub const fn check_fee(fee: u64) -> Result<(), PolicyError> {
    if fee < MINIMUM_FEE {
        Err(PolicyError::FeeTooSmall)
    } else {
        Ok(())
    }
}

/// `Currency::defaultDustThreshold` for a wallet height (the dust threshold
/// [`crate::TransferOptions::dust_threshold`] wants).
#[must_use]
pub fn dust_threshold(height: u64) -> u64 {
    default_dust_threshold(u32::try_from(height).unwrap_or(u32::MAX))
}

// ---------------------------------------------------------------------------------------------
// mixin

/// The mixins (ring size - 1) a transaction may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixinRange {
    /// Smallest allowed mixin.
    pub min: u64,
    /// Largest allowed mixin.
    pub max: u64,
    /// What C++ wallets use when the user gives none.
    pub default: u64,
}

impl MixinRange {
    /// `src/errors/validate_parameters.cpp` `validateMixin`.
    pub const fn check(&self, mixin: u64) -> Result<(), PolicyError> {
        if mixin < self.min {
            Err(PolicyError::MixinTooSmall {
                mixin,
                min: self.min,
            })
        } else if mixin > self.max {
            Err(PolicyError::MixinTooBig {
                mixin,
                max: self.max,
            })
        } else {
            Ok(())
        }
    }

    /// Whether `mixin` is allowed.
    #[must_use]
    pub const fn contains(&self, mixin: u64) -> bool {
        self.check(mixin).is_ok()
    }
}

/// `Mixins::getMixinAllowableRange(height)`'s default mixin (`DEFAULT_MIXIN_V*`), what C++
/// wallet-api uses for `/transactions/send/basic` and fusion.
#[must_use]
pub const fn default_mixin(height: u64) -> u64 {
    mixin_limits(height).default
}

/// The mixin range for a transaction of `version` built at `height`: the mixin limits of
/// `Mixins::getMixinAllowableRange`, and for version 2 also the network's fork-14 ring-size
/// rule (`RingSizeRule::Range`, ring size = mixin + 1). The default is clamped into the range.
#[must_use]
pub fn mixin_range(height: u64, version: TxVersion, rule: RingSizeRule) -> MixinRange {
    let limits = mixin_limits(height);
    let (min, max) = match (version, rule) {
        (TxVersion::V2, RingSizeRule::Range { min, max }) => {
            let lo = u64::from(min.max(1) - 1);
            let hi = u64::from(max.max(min.max(1)) - 1);
            (limits.min.clamp(lo, hi), limits.max.clamp(lo, hi))
        }
        _ => (limits.min, limits.max),
    };
    MixinRange {
        min,
        max,
        default: limits.default.clamp(min, max),
    }
}

/// Whether `mixin` satisfies the fork-14 ring-size rule for `version` (the mixin limits aside):
/// version 1 and [`RingSizeRule::MixinLimits`] add no rule.
#[must_use]
pub fn ring_rule_allows(mixin: u64, version: TxVersion, rule: RingSizeRule) -> bool {
    match (version, rule) {
        (TxVersion::V2, RingSizeRule::Range { min, max }) => {
            (u64::from(min)..=u64::from(max)).contains(&mixin.saturating_add(1))
        }
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// transaction version

/// The transaction versions the next block accepts, from the daemon's `/info` (`/getinfo`):
/// `tx_versions` when the daemon sends it (xkrd), otherwise `[1, 2]` when the top block's
/// `major_version` is 7 or higher and `[1]` before (C++ nodes, older xkrd; `docs/tx-v2.md`
/// §9).
#[must_use]
pub fn allowed_tx_versions(tx_versions: Option<&[u8]>, major_version: Option<u8>) -> Vec<u8> {
    match tx_versions {
        Some(list) => list.to_vec(),
        None if major_version.is_some_and(|m| m >= BLOCK_MAJOR_VERSION_7) => vec![1, 2],
        None => vec![1],
    }
}

/// The version to build: the highest of [`allowed_tx_versions`] (version 2 for anything above
/// 1, version 1 for an empty list).
#[must_use]
pub fn tx_version_for(tx_versions: Option<&[u8]>, major_version: Option<u8>) -> TxVersion {
    let highest = allowed_tx_versions(tx_versions, major_version)
        .into_iter()
        .max()
        .unwrap_or(1);
    if highest >= 2 {
        TxVersion::V2
    } else {
        TxVersion::V1
    }
}

// ---------------------------------------------------------------------------------------------
// unlock time

/// `src/utilities/utilities.cpp` `utilities::isInputUnlocked`: whether an output with
/// `unlock_time` may be spent by a transaction built now. `0` is always unlocked; below
/// `CRYPTONOTE_MAX_BLOCK_NUMBER` it is a block height (`height + 1 >= unlock_time`), otherwise
/// a unix time (`now + 90 >= unlock_time`). `height` is the daemon's block count, `now` the
/// wall clock in seconds.
#[must_use]
pub const fn is_unlocked(unlock_time: u64, height: u64, now: u64) -> bool {
    if unlock_time == 0 {
        return true;
    }
    if unlock_time >= CRYPTONOTE_MAX_BLOCK_NUMBER as u64 {
        return now.saturating_add(CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_SECONDS) >= unlock_time;
    }
    height.saturating_add(CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS) >= unlock_time
}

// ---------------------------------------------------------------------------------------------
// size

/// `src/utilities/utilities.cpp` `utilities::getMaxTxSize`: the largest transaction the
/// wallet sends at `height`, `min(100 000 + height * 102 400 / 350 400, 125 000) - 600` (the
/// block size limit without the median term, minus room for the coinbase).
#[must_use]
pub const fn max_tx_size(height: u64) -> u64 {
    let growth = height.saturating_mul(MAX_BLOCK_SIZE_GROWTH_SPEED_NUMERATOR)
        / MAX_BLOCK_SIZE_GROWTH_SPEED_DENOMINATOR;
    let x = (MAX_BLOCK_SIZE_INITIAL as u64).saturating_add(growth);
    let limit = if x < MAX_TX_SIZE_CAP {
        x
    } else {
        MAX_TX_SIZE_CAP
    };
    limit - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64
}

/// `src/wallet_backend/transfer.cpp` `isTransactionPayloadTooBig`: a serialized transaction of
/// `size` bytes must not exceed [`max_tx_size`].
pub const fn check_tx_size(size: u64, height: u64) -> Result<(), PolicyError> {
    let max = max_tx_size(height);
    if size > max {
        Err(PolicyError::TooLarge { size, max })
    } else {
        Ok(())
    }
}

/// An estimate of a transaction's serialized size in bytes (wallet-backend-js
/// `lib/Utilities.ts` `estimateTransactionSize`, plus 4 bytes per decoy for the relative ring
/// indexes, as C++ `Currency::getApproximateMaximumInputCount` counts them).
///
/// Every input has a ring of `mixin + 1`; version 1 signs it with 64 bytes per member, version
/// 2 with a CLSAG of 32 bytes per member plus 32, and version-2 outputs carry a view tag.
/// `extra_data_len` is the length of arbitrary extra nonce data (Hugin), 0 for none.
#[must_use]
pub fn estimate_tx_size(
    mixin: u64,
    inputs: u64,
    outputs: u64,
    has_payment_id: bool,
    extra_data_len: u64,
    version: TxVersion,
) -> u64 {
    const KEY_IMAGE_SIZE: u64 = 32;
    const OUTPUT_KEY_SIZE: u64 = 32;
    const AMOUNT_SIZE: u64 = 8 + 2; // varint
    const GLOBAL_INDEXES_VECTOR_SIZE_SIZE: u64 = 1; // varint
    const GLOBAL_INDEXES_INITIAL_VALUE_SIZE: u64 = 4; // varint
    const GLOBAL_INDEXES_DIFFERENCE_SIZE: u64 = 4; // varint
    const SIGNATURE_SIZE: u64 = 64;
    const CLSAG_SCALAR_SIZE: u64 = 32;
    const EXTRA_TAG_SIZE: u64 = 1;
    const INPUT_TAG_SIZE: u64 = 1;
    const OUTPUT_TAG_SIZE: u64 = 1;
    const PUBLIC_KEY_SIZE: u64 = 32;
    const TRANSACTION_VERSION_SIZE: u64 = 1;
    const TRANSACTION_UNLOCK_TIME_SIZE: u64 = 8 + 2; // varint
    const PAYMENT_ID_SIZE: u64 = 34;

    let v2 = matches!(version, TxVersion::V2);
    let extra_data = if extra_data_len > 0 {
        extra_data_len + 4
    } else {
        0
    };
    let payment_id = if has_payment_id { PAYMENT_ID_SIZE } else { 0 };
    let header = TRANSACTION_VERSION_SIZE
        + TRANSACTION_UNLOCK_TIME_SIZE
        + EXTRA_TAG_SIZE
        + extra_data
        + PUBLIC_KEY_SIZE
        + payment_id;
    let ring_size = mixin.saturating_add(1);
    let signature = if v2 {
        CLSAG_SCALAR_SIZE * ring_size + CLSAG_SCALAR_SIZE
    } else {
        SIGNATURE_SIZE * ring_size
    };
    let input = INPUT_TAG_SIZE
        + AMOUNT_SIZE
        + KEY_IMAGE_SIZE
        + GLOBAL_INDEXES_VECTOR_SIZE_SIZE
        + GLOBAL_INDEXES_INITIAL_VALUE_SIZE
        + GLOBAL_INDEXES_DIFFERENCE_SIZE * mixin
        + signature;
    let output = OUTPUT_TAG_SIZE + OUTPUT_KEY_SIZE + u64::from(v2) + AMOUNT_SIZE;
    header
        .saturating_add(input.saturating_mul(inputs))
        .saturating_add(output.saturating_mul(outputs))
}

/// `src/cryptonote_core/currency.cpp` `Currency::getApproximateMaximumInputCount`: about how
/// many version-1 inputs with `mixin` decoys fit in `transaction_size` bytes next to
/// `output_count` outputs (0 if not even the header and outputs fit).
#[must_use]
pub const fn approximate_max_input_count(
    transaction_size: u64,
    output_count: u64,
    mixin: u64,
) -> u64 {
    const KEY_IMAGE_SIZE: u64 = 32;
    const OUTPUT_KEY_SIZE: u64 = 32;
    const AMOUNT_SIZE: u64 = 8 + 2; // varint
    const GLOBAL_INDEXES_VECTOR_SIZE_SIZE: u64 = 1; // varint
    const GLOBAL_INDEXES_INITIAL_VALUE_SIZE: u64 = 4; // varint
    const GLOBAL_INDEXES_DIFFERENCE_SIZE: u64 = 4; // varint
    const SIGNATURE_SIZE: u64 = 64;
    const EXTRA_TAG_SIZE: u64 = 1;
    const INPUT_TAG_SIZE: u64 = 1;
    const OUTPUT_TAG_SIZE: u64 = 1;
    const PUBLIC_KEY_SIZE: u64 = 32;
    const TRANSACTION_VERSION_SIZE: u64 = 1;
    const TRANSACTION_UNLOCK_TIME_SIZE: u64 = 8;

    let outputs_size = output_count.saturating_mul(OUTPUT_TAG_SIZE + OUTPUT_KEY_SIZE + AMOUNT_SIZE);
    let header_size =
        TRANSACTION_VERSION_SIZE + TRANSACTION_UNLOCK_TIME_SIZE + EXTRA_TAG_SIZE + PUBLIC_KEY_SIZE;
    let input_size = (INPUT_TAG_SIZE
        + AMOUNT_SIZE
        + KEY_IMAGE_SIZE
        + SIGNATURE_SIZE
        + GLOBAL_INDEXES_VECTOR_SIZE_SIZE
        + GLOBAL_INDEXES_INITIAL_VALUE_SIZE)
        .saturating_add(mixin.saturating_mul(GLOBAL_INDEXES_DIFFERENCE_SIZE + SIGNATURE_SIZE));
    transaction_size
        .saturating_sub(header_size)
        .saturating_sub(outputs_size)
        / input_size
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Heights where the mixin rules change (`MIXIN_LIMITS_V*_HEIGHT`) and the block before.
    const HEIGHTS: [u64; 5] = [0, 440_000, 620_000, 800_000, 1_250_000];

    #[test]
    fn mixin_tables() {
        // (height, min, max, default): `getMixinAllowableRange`
        let want = [
            (0, 0, u64::MAX, 3),
            (440_000, 0, 100, 100),
            (620_000, 7, 7, 7),
            (800_000, 3, 3, 3),
            (1_250_000, 1, 5, 5),
        ];
        for (height, min, max, default) in want {
            let r = mixin_range(height, TxVersion::V1, RingSizeRule::MixinLimits);
            assert_eq!(r, MixinRange { min, max, default }, "height {height}");
            assert_eq!(default_mixin(height), default, "height {height}");
            // version 2 with the mixin-limits rule is the same
            assert_eq!(
                mixin_range(height, TxVersion::V2, RingSizeRule::MixinLimits),
                r
            );
        }
        // the block before each change still has the old rule
        assert_eq!(default_mixin(439_999), 3);
        assert_eq!(default_mixin(619_999), 100);
        assert_eq!(default_mixin(799_999), 7);
        assert_eq!(default_mixin(1_249_999), 3);
        assert_eq!(default_mixin(3_000_000), 5);
    }

    #[test]
    fn mixin_checks_and_messages() {
        let r = mixin_range(1_250_000, TxVersion::V1, RingSizeRule::MixinLimits);
        assert_eq!(r.check(1), Ok(()));
        assert_eq!(r.check(5), Ok(()));
        let e = r.check(0).unwrap_err();
        assert_eq!(e, PolicyError::MixinTooSmall { mixin: 0, min: 1 });
        assert_eq!(e.code(), 21);
        assert_eq!(
            e.to_string(),
            "The mixin value given (0) is lower than the minimum mixin allowed (1)"
        );
        let e = r.check(6).unwrap_err();
        assert_eq!(e.code(), 22);
        assert_eq!(
            e.to_string(),
            "The mixin value given (6) is greater than the maximum mixin allowed (5)"
        );
        assert!(!r.contains(6));
        assert!(mixin_range(0, TxVersion::V1, RingSizeRule::MixinLimits).contains(u64::MAX));
    }

    #[test]
    fn version_2_ring_rule_narrows_the_range() {
        let rule = RingSizeRule::Range { min: 4, max: 4 };
        let r = mixin_range(1_250_000, TxVersion::V2, rule);
        assert_eq!(
            r,
            MixinRange {
                min: 3,
                max: 3,
                default: 3
            }
        );
        // version 1 ignores the fork-14 rule
        assert_eq!(
            mixin_range(1_250_000, TxVersion::V1, rule),
            MixinRange {
                min: 1,
                max: 5,
                default: 5
            }
        );
        // a rule above the mixin limits pins both ends to the rule's lower end
        let high = RingSizeRule::Range { min: 16, max: 16 };
        assert_eq!(
            mixin_range(1_250_000, TxVersion::V2, high),
            MixinRange {
                min: 15,
                max: 15,
                default: 15
            }
        );
        assert!(ring_rule_allows(3, TxVersion::V2, rule));
        assert!(!ring_rule_allows(2, TxVersion::V2, rule));
        assert!(ring_rule_allows(2, TxVersion::V1, rule));
        assert!(ring_rule_allows(
            2,
            TxVersion::V2,
            RingSizeRule::MixinLimits
        ));
    }

    #[test]
    fn fee_rules() {
        assert_eq!(minimum_fee(), 10);
        assert_eq!(check_fee(10), Ok(()));
        let e = check_fee(9).unwrap_err();
        assert_eq!((e, e.code()), (PolicyError::FeeTooSmall, 17));
        assert_eq!(
            e.to_string(),
            "The fee given for this transaction is below the minimum allowed network fee."
        );
        for height in HEIGHTS {
            let want = if height >= 620_000 { 0 } else { 10 };
            assert_eq!(dust_threshold(height), want, "height {height}");
        }
    }

    #[test]
    fn transaction_versions() {
        assert_eq!(tx_version_for(Some(&[1]), Some(7)), TxVersion::V1);
        assert_eq!(tx_version_for(Some(&[1, 2]), None), TxVersion::V2);
        assert_eq!(tx_version_for(Some(&[2]), Some(6)), TxVersion::V2);
        assert_eq!(tx_version_for(Some(&[]), Some(7)), TxVersion::V1);
        assert_eq!(tx_version_for(None, None), TxVersion::V1);
        assert_eq!(tx_version_for(None, Some(6)), TxVersion::V1);
        assert_eq!(tx_version_for(None, Some(7)), TxVersion::V2);
        assert_eq!(allowed_tx_versions(None, Some(7)), vec![1, 2]);
        assert_eq!(allowed_tx_versions(None, Some(5)), vec![1]);
        assert_eq!(allowed_tx_versions(Some(&[2]), Some(5)), vec![2]);
    }

    #[test]
    fn unlock_times() {
        // zero is always unlocked
        assert!(is_unlocked(0, 0, 0));
        // block heights: height + 1 >= unlock_time
        assert!(is_unlocked(11, 10, 0));
        assert!(!is_unlocked(12, 10, 0));
        assert!(is_unlocked(499_999_999, 499_999_998, 0));
        // unix times: now + 90 >= unlock_time
        assert!(!is_unlocked(500_000_000, u64::MAX - 1, 0));
        assert!(is_unlocked(1_700_000_090, 0, 1_700_000_000));
        assert!(!is_unlocked(1_700_000_091, 0, 1_700_000_000));
        assert!(is_unlocked(u64::MAX, 0, u64::MAX));
    }

    #[test]
    fn size_limits() {
        let want = [
            (0, 99_400),
            (440_000, 124_400),
            (620_000, 124_400),
            (800_000, 124_400),
            (1_250_000, 124_400),
        ];
        for (height, max) in want {
            assert_eq!(max_tx_size(height), max, "height {height}");
        }
        // 85 546 * 102 400 / 350 400 = 24 999, 85 547 -> 25 000
        assert_eq!(max_tx_size(85_546), 124_399);
        assert_eq!(max_tx_size(85_547), 124_400);
        assert_eq!(max_tx_size(u64::MAX), 124_400);
        assert_eq!(check_tx_size(124_400, 1_250_000), Ok(()));
        let e = check_tx_size(130_000, 1_250_000).unwrap_err();
        assert_eq!(e.code(), 32);
        assert_eq!(
            e.to_string(),
            "Transaction is too large: (126.95 KB). Max allowed size is 121.48 KB. Decrease the \
             amount you are sending, or perform some fusion transactions."
        );
        assert_eq!(pretty_print_bytes(1000), "1000.00 B");
        assert_eq!(pretty_print_bytes(1024), "1.00 KB");
        assert_eq!(pretty_print_bytes(u64::MAX), "16777216.00 TB");
    }

    #[test]
    fn size_estimates() {
        // header 1 + 10 + 1 + 32; input 1 + 10 + 32 + 1 + 4 + 4 * 3 + 64 * 4; output 1 + 32 + 10
        assert_eq!(
            estimate_tx_size(3, 1, 2, false, 0, TxVersion::V1),
            44 + 316 + 86
        );
        // version 2: CLSAG 32 * 4 + 32, view tags; payment id and extra data
        assert_eq!(
            estimate_tx_size(3, 2, 3, true, 100, TxVersion::V2),
            (44 + 34 + 104) + 2 * (60 + 160) + 3 * 44
        );
        // `getApproximateMaximumInputCount(FUSION_TX_MAX_SIZE, 4, mixin)`: (30 000 - 42 - 172)
        // / (112 + 68 * mixin)
        assert_eq!(approximate_max_input_count(30_000, 4, 0), 265);
        assert_eq!(approximate_max_input_count(30_000, 4, 1), 165);
        assert_eq!(approximate_max_input_count(30_000, 4, 3), 94);
        assert_eq!(approximate_max_input_count(30_000, 4, 5), 65);
        assert_eq!(approximate_max_input_count(30_000, 4, 100), 4);
        assert_eq!(approximate_max_input_count(100, 4, 0), 0);
    }
}
