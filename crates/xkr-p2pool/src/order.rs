//! The C++ p2pool's hash order and small integer helpers.
//!
//! * [`hash_cmp`]: `hash::operator<` (`src/common.h`) compares the four little-endian 64-bit
//!   words from index 3 down to 0, i.e. the 32 bytes as a little-endian 256-bit integer (byte
//!   31 first, byte 0 last). It orders uncles, merge-mining extra ids and miner wallets. It is
//!   not the byte order of `Hash`'s own `Ord`.
//! * [`xorshift64star`] (`src/util.h`) and [`mul_hi`] (the `umul128` high part, the
//!   `floor(a * b / 2^64)` idiom for random indices).

use core::cmp::Ordering;

use xkr_types::Hash;

/// C++ `hash::operator<` as an [`Ordering`]: the bytes compared as a little-endian 256-bit
/// integer.
#[must_use]
pub fn hash_cmp(a: &Hash, b: &Hash) -> Ordering {
    a.0.iter().rev().cmp(b.0.iter().rev())
}

/// C++ `a < b` for hashes.
#[must_use]
pub fn hash_less(a: &Hash, b: &Hash) -> bool {
    hash_cmp(a, b) == Ordering::Less
}

/// A hash ordered like the C++ `hash` (for sorted containers, `std::map<hash, …>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HashOrd(pub Hash);

impl Ord for HashOrd {
    fn cmp(&self, other: &Self) -> Ordering {
        hash_cmp(&self.0, &other.0)
    }
}

impl PartialOrd for HashOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// C++ `xorshift64star` (`src/util.h`).
#[must_use]
pub const fn xorshift64star(mut x: u64) -> u64 {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// The high 64 bits of the full product `a * b` (C++ `umul128(a, b, &hi)`'s `hi`).
#[must_use]
pub const fn mul_hi(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) >> 64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal port of `hash::operator<`: words 3 down to 0, each a little-endian `u64`.
    fn less_by_words(a: &Hash, b: &Hash) -> bool {
        let w = |h: &Hash, i: usize| u64::from_le_bytes(h.0[i * 8..i * 8 + 8].try_into().unwrap());
        for i in (1..4).rev() {
            if w(a, i) < w(b, i) {
                return true;
            }
            if w(a, i) > w(b, i) {
                return false;
            }
        }
        w(a, 0) < w(b, 0)
    }

    #[test]
    fn hash_order_is_little_endian_256() {
        let mut x = 1u64;
        let mut next = || {
            x = xorshift64star(x);
            x
        };
        for _ in 0..20_000 {
            let mut a = [0u8; 32];
            let mut b = [0u8; 32];
            for (ai, bi) in a.iter_mut().zip(b.iter_mut()) {
                *ai = next() as u8;
                // Mostly equal bytes so the lower words decide too.
                *bi = if next().is_multiple_of(4) {
                    next() as u8
                } else {
                    *ai
                };
            }
            let (a, b) = (Hash(a), Hash(b));
            assert_eq!(hash_less(&a, &b), less_by_words(&a, &b));
            assert_eq!(hash_cmp(&a, &b) == Ordering::Equal, a == b);
        }
        let mut lo = [0u8; 32];
        lo[0] = 0xff;
        let mut hi = [0u8; 32];
        hi[31] = 0x01;
        // Byte order would say lo > hi; the 256-bit order says lo < hi.
        assert!(Hash(lo) > Hash(hi));
        assert!(hash_less(&Hash(lo), &Hash(hi)));
        assert!(HashOrd(Hash(lo)) < HashOrd(Hash(hi)));
    }

    #[test]
    fn xorshift_and_mul_hi() {
        // xorshift64star(1) step by step: 1 ^ 0 = 1; 1 ^ (1 << 25); then ^ (>> 27) = no change.
        let x = (1u64 | (1 << 25)).wrapping_mul(0x2545_F491_4F6C_DD1D);
        assert_eq!(xorshift64star(1), x);
        assert_eq!(xorshift64star(0), 0);
        assert_eq!(mul_hi(u64::MAX, u64::MAX), u64::MAX - 1);
        assert_eq!(mul_hi(1 << 63, 4), 2);
        assert_eq!(mul_hi(12345, 6789), 0);
    }
}
