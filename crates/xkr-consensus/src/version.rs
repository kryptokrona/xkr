//! Block major version schedule and proof-of-work algorithm per version.

use crate::params::NetworkParams;

/// C++ `UpgradeManager::getBlockMajorVersion(height)` with the fixed upgrade heights of
/// `Core` (`addMajorBlockVersion(V2..V6, UPGRADE_HEIGHT_V2..V6)`): the highest version whose
/// upgrade height is **strictly below** `height`, else 1. Delegates to the fork table
/// ([`crate::forks::ForkSchedule::block_major_version_for`]).
///
/// Mainnet (heights 1, 2, 3, 4): blocks 0-1 are v1, 2 is v2, 3 is v3, 4 is v4, >= 5 is v5.
/// Testnet (all 0): block 0 is v1, everything after is v5. With fork 13 at `F`: blocks `> F`
/// are v6.
#[must_use]
pub fn block_major_version_for_height(params: &NetworkParams, height: u32) -> u8 {
    params.forks().block_major_version_for(u64::from(height))
}

/// C++ `Currency::upgradeHeight(majorVersion)`: `None` for versions without a configured height
/// (C++ returns `UNDEF_HEIGHT`).
#[must_use]
pub fn upgrade_height(params: &NetworkParams, major_version: u8) -> Option<u32> {
    params.forks().upgrade_height(major_version)
}

/// Proof-of-work function of a block major version (C++ `CachedBlock::getBlockLongHash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowVariant {
    /// `cn_slow_hash_v0`: CryptoNight, 2 MiB, 2^20 iterations (major 1-3).
    CryptoNightV0,
    /// `cn_lite_slow_hash_v1`: CryptoNight-Lite variant 1, 1 MiB (major 4).
    CryptoNightLiteV1,
    /// `cn_turtle_lite_slow_hash_v2`: CryptoNight-Turtle-Lite variant 2, 256 KiB (major >= 5,
    /// including the fork-13 version 6).
    CryptoNightTurtleLiteV2,
}

/// PoW variant for a major version; `None` for 0 (C++ throws "Unknown block major version").
#[must_use]
pub fn pow_variant_for(major_version: u8) -> Option<PowVariant> {
    match major_version {
        0 => None,
        1..=3 => Some(PowVariant::CryptoNightV0),
        4 => Some(PowVariant::CryptoNightLiteV1),
        5.. => Some(PowVariant::CryptoNightTurtleLiteV2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{MAINNET, TESTNET};

    #[test]
    fn mainnet_schedule() {
        let v: Vec<u8> = (0..8)
            .map(|h| block_major_version_for_height(&MAINNET, h))
            .collect();
        assert_eq!(v, vec![1, 1, 2, 3, 4, 5, 5, 5]);
        assert_eq!(block_major_version_for_height(&MAINNET, 2_617_000), 5);
    }

    #[test]
    fn testnet_schedule() {
        assert_eq!(block_major_version_for_height(&TESTNET, 0), 1);
        assert_eq!(block_major_version_for_height(&TESTNET, 1), 5);
    }

    #[test]
    fn testnet_fork13_schedule() {
        let p = TESTNET.with_fork13_height(Some(10));
        assert_eq!(block_major_version_for_height(&p, 10), 5);
        assert_eq!(block_major_version_for_height(&p, 11), 6);
        assert_eq!(upgrade_height(&p, 6), Some(10));
        assert_eq!(upgrade_height(&TESTNET, 6), None);
        assert_eq!(
            pow_variant_for(6),
            Some(PowVariant::CryptoNightTurtleLiteV2)
        );
    }

    #[test]
    fn pow_variants() {
        assert_eq!(pow_variant_for(3), Some(PowVariant::CryptoNightV0));
        assert_eq!(pow_variant_for(4), Some(PowVariant::CryptoNightLiteV1));
        assert_eq!(
            pow_variant_for(5),
            Some(PowVariant::CryptoNightTurtleLiteV2)
        );
        assert_eq!(pow_variant_for(0), None);
    }
}
