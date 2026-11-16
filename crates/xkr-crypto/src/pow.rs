//! Proof-of-work helpers: the CryptoNight entry points named like the C++ `hash.h` inline
//! functions and `check_hash` from `src/cryptonote_core/check_difficulty.cpp`.

use xkr_types::Hash;

use crate::cryptonight::{CnParams, DataCanNotBeHashed, Variant, cn_slow_hash};

/// C++ `cn_slow_hash_v0`: CryptoNight, 2 MiB, 2^20 iterations (blocks of major version 1-3).
#[must_use]
pub fn cn_slow_hash_v0(data: &[u8]) -> Hash {
    // Variant 0 never fails.
    Hash(cn_slow_hash(data, Variant::V0, &CnParams::CN).unwrap_or_default())
}

/// C++ `cn_lite_slow_hash_v1`: CryptoNight-Lite variant 1, 1 MiB, 2^19 iterations
/// (blocks of major version 4). Variant 1 needs at least 43 bytes of input.
pub fn cn_lite_slow_hash_v1(data: &[u8]) -> Result<Hash, DataCanNotBeHashed> {
    cn_slow_hash(data, Variant::V1, &CnParams::CN_LITE).map(Hash)
}

/// C++ `cn_turtle_lite_slow_hash_v2`: CryptoNight-Turtle-Lite variant 2, 256 KiB scratchpad
/// (128 KiB addressed), 2^17 iterations (blocks of major version >= 5).
#[must_use]
pub fn cn_turtle_lite_slow_hash_v2(data: &[u8]) -> Hash {
    // Variant 2 never fails.
    Hash(cn_slow_hash(data, Variant::V2, &CnParams::CN_TURTLE_LITE).unwrap_or_default())
}

#[inline]
fn mul(a: u64, b: u64) -> (u64, u64) {
    let r = u128::from(a) * u128::from(b);
    (r as u64, (r >> 64) as u64)
}

#[inline]
fn cadd(a: u64, b: u64) -> bool {
    a.wrapping_add(b) < a
}

#[inline]
fn cadc(a: u64, b: u64, c: bool) -> bool {
    a.wrapping_add(b) < a || (c && a.wrapping_add(b) == u64::MAX)
}

/// C++ `check_hash(hash, difficulty)` (`check_difficulty.cpp`), ported literally: true iff
/// `hash * difficulty < 2^256` with `hash` read as a little-endian 256-bit integer.
#[must_use]
pub fn check_hash(hash: &Hash, difficulty: u64) -> bool {
    let word = |i: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&hash.0[i * 8..i * 8 + 8]);
        u64::from_le_bytes(w)
    };
    // First check the highest word, this will most likely fail for a random hash.
    let (top, high) = mul(word(3), difficulty);
    if high != 0 {
        return false;
    }
    let (_low, cur) = mul(word(0), difficulty);
    let (low, high) = mul(word(1), difficulty);
    let mut carry = cadd(cur, low);
    let cur = high;
    let (low, high) = mul(word(2), difficulty);
    carry = cadc(cur, low, carry);
    carry = cadc(high, top, carry);
    !carry
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference: exact 256-bit product < 2^256.
    fn check_hash_ref(hash: &Hash, difficulty: u64) -> bool {
        let mut carry: u128 = 0;
        for i in 0..4 {
            let mut w = [0u8; 8];
            w.copy_from_slice(&hash.0[i * 8..i * 8 + 8]);
            let v = u128::from(u64::from_le_bytes(w)) * u128::from(difficulty) + carry;
            carry = v >> 64;
        }
        carry == 0
    }

    #[test]
    fn check_hash_edges() {
        assert!(check_hash(&Hash([0xff; 32]), 1));
        assert!(!check_hash(&Hash([0xff; 32]), 2));
        assert!(check_hash(&Hash::ZERO, u64::MAX));
        let mut h = [0u8; 32];
        h[31] = 0x80; // 2^255
        assert!(!check_hash(&Hash(h), 2));
        assert!(check_hash(&Hash(h), 1));
    }

    #[test]
    fn check_hash_matches_reference() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        for _ in 0..2000 {
            let mut h = [0u8; 32];
            for b in &mut h {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *b = x as u8;
            }
            // bias the top word towards small values so both outcomes occur
            h[31] >>= (x % 8) as u32;
            h[30] >>= (x % 5) as u32;
            let d = x >> (x % 60);
            let hash = Hash(h);
            assert_eq!(check_hash(&hash, d), check_hash_ref(&hash, d), "{hash} {d}");
        }
    }
}
