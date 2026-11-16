//! Stratum share targets (hugin-utils `parseTarget` / `meetsTarget`, cryptonote-nodejs-pool
//! `getTargetHex`).
//!
//! A pool sends its share difficulty as a compact target: 4 bytes (8 hex characters,
//! little-endian `raw`, difficulty `0xFFFFFFFF / raw`) or 8 bytes (a little-endian `u64`). A
//! share meets the target when the last 8 bytes of its hash, read little-endian, are not above
//! the 64-bit target.

use crate::blob::is_hex;

/// hugin-utils `parseTarget`: a 4-byte compact pool target `raw` (little-endian) becomes
/// `u64::MAX / (0xFFFFFFFF / raw)`; an 8-byte target is a little-endian `u64`. `None` for a zero
/// or otherwise unusable target.
#[must_use]
pub fn parse_target(target_hex: &str) -> Option<u64> {
    let bytes = hex::decode(target_hex).ok()?;
    match bytes.len() {
        4 => {
            let raw = u64::from(u32::from_le_bytes(bytes[..4].try_into().ok()?));
            if raw == 0 {
                return None;
            }
            let denom = 0xFFFF_FFFFu64 / raw;
            (denom != 0).then(|| u64::MAX / denom)
        }
        8 => Some(u64::from_le_bytes(bytes[..8].try_into().ok()?)),
        _ => None,
    }
}

/// The last 8 bytes of `hash`, little-endian: the value compared with a 64-bit target.
#[must_use]
pub fn hash_tail(hash: &[u8; 32]) -> u64 {
    let mut tail = [0u8; 8];
    tail.copy_from_slice(&hash[24..32]);
    u64::from_le_bytes(tail)
}

/// Whether `hash` meets the 64-bit `target` ([`parse_target`]).
#[must_use]
pub fn meets_target_u64(hash: &[u8; 32], target: u64) -> bool {
    hash_tail(hash) <= target
}

/// hugin-utils `meetsTarget`: the last 8 bytes of the hash (little-endian) `<= target`. Like
/// hugin-utils, an unusable target counts as met; callers that must refuse such jobs check
/// [`parse_target`] first.
#[must_use]
pub fn meets_target(hash: &[u8; 32], target_hex: &str) -> bool {
    parse_target(target_hex).is_none_or(|target| meets_target_u64(hash, target))
}

/// The pool difficulty a 4-byte compact target stands for (`0xFFFFFFFF / raw`), for logs.
#[must_use]
pub fn target_difficulty(target_hex: &str) -> Option<u64> {
    parse_target(target_hex).map(|t| u64::MAX / t.max(1))
}

/// hugin-node `share_difficulty`: `floor(0xFFFFFFFF / raw)` for a 4-byte compact target (8 hex
/// characters, `raw` little-endian); `None` for any other length, non-hex or a zero target.
#[must_use]
pub fn share_difficulty(target_hex: &str) -> Option<u64> {
    if target_hex.len() != 8 || !is_hex(target_hex) {
        return None;
    }
    let bytes: [u8; 4] = hex::decode(target_hex).ok()?.try_into().ok()?;
    let raw = u64::from(u32::from_le_bytes(bytes));
    (raw != 0).then(|| 0xFFFF_FFFFu64 / raw)
}

/// The 4-byte compact target (8 hex characters) for a share difficulty, as stratum pools send it.
#[must_use]
pub fn target_for_difficulty(difficulty: u64) -> String {
    let raw = (0xFFFF_FFFFu64 / difficulty.max(1)).max(1) as u32;
    hex::encode(raw.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_and_long_targets() {
        assert_eq!(parse_target("ffffffff"), Some(u64::MAX));
        assert_eq!(parse_target("00000000"), None);
        assert_eq!(parse_target("0102"), None);
        assert_eq!(parse_target("0100000000000000"), Some(1));
        assert_eq!(target_for_difficulty(1), "ffffffff");
        assert_eq!(target_for_difficulty(100), "285c8f02");
        assert_eq!(share_difficulty("285c8f02"), Some(100));
        assert_eq!(target_difficulty(&target_for_difficulty(5000)), Some(5000));
        let mut hash = [0u8; 32];
        hash[24..32].copy_from_slice(&5u64.to_le_bytes());
        assert!(meets_target_u64(&hash, 5));
        assert!(!meets_target_u64(&hash, 4));
        assert!(meets_target(&hash, "0500000000000000"));
        assert!(!meets_target(&hash, "0400000000000000"));
        assert!(meets_target(&hash, "zz"));
    }

    /// The same targets as hugin-p2p `difficultyToTarget` (`max(1, floor(0xFFFFFFFF / d))`, 4
    /// bytes little-endian) and the same difficulty back as its `targetToDifficulty`
    /// (`u64::MAX / parseTarget`, here [`target_difficulty`]) and as hugin-node
    /// `share_difficulty`, so the apps and the relay agree on every app target.
    #[test]
    fn difficulty_targets_match_hugin_p2p() {
        let vectors: [(u64, &str); 9] = [
            (1, "ffffffff"),
            (3, "55555555"),
            (7, "24499224"),
            (100, "285c8f02"),
            (200, "14ae4701"),
            (1000, "37894100"),
            (5000, "711b0d00"),
            (0xFFFF_FFFF, "01000000"),
            // beyond 32 bits the target stays at its least value
            (10_000_000_000, "01000000"),
        ];
        for (difficulty, target) in vectors {
            assert_eq!(target_for_difficulty(difficulty), target, "{difficulty}");
            let back = difficulty.min(0xFFFF_FFFF);
            assert_eq!(target_difficulty(target), Some(back), "{target}");
            assert_eq!(share_difficulty(target), Some(back), "{target}");
        }
        // difficulty 0 counts as 1, like `Math.max(1, ...)`
        assert_eq!(target_for_difficulty(0), "ffffffff");
        // hugin-p2p `targetToDifficulty` gives 0 for a zero target; here there is none
        assert_eq!(target_difficulty("00000000"), None);
    }
}
