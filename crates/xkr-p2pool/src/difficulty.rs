//! Sidechain difficulty: the unsigned 128-bit `difficulty_type` of the C++ p2pool
//! (`src/common.h` `difficulty_type`, `src/util.cpp` `difficulty_type::operator/=` and
//! `difficulty_type::check_pow`).
//!
//! The arithmetic matches C++ exactly, including where it wraps: `+`, `-` and `* u64` are taken
//! modulo `2^128`, divisions are floor divisions.

use core::fmt;

use xkr_types::Hash;

/// A sidechain difficulty or cumulative difficulty (C++ `difficulty_type {lo, hi}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Difficulty(pub u128);

impl Difficulty {
    /// Zero (C++ `difficulty_type()`, "empty").
    pub const ZERO: Self = Self(0);
    /// `diff_max` (`src/common.h`).
    pub const MAX: Self = Self(u128::MAX);

    /// C++ `difficulty_type(lo, hi)`.
    #[must_use]
    pub const fn new(lo: u64, hi: u64) -> Self {
        Self(((hi as u128) << 64) | lo as u128)
    }

    /// C++ `difficulty_type(lo)`.
    #[must_use]
    pub const fn from_u64(lo: u64) -> Self {
        Self(lo as u128)
    }

    /// The low 64 bits (`lo`).
    #[must_use]
    pub const fn lo(self) -> u64 {
        self.0 as u64
    }

    /// The high 64 bits (`hi`).
    #[must_use]
    pub const fn hi(self) -> u64 {
        (self.0 >> 64) as u64
    }

    /// C++ `empty()`.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// C++ `operator+=`: modulo `2^128`.
    #[must_use]
    pub const fn wrapping_add(self, other: Self) -> Self {
        Self(self.0.wrapping_add(other.0))
    }

    /// C++ `operator-=`: modulo `2^128`.
    #[must_use]
    pub const fn wrapping_sub(self, other: Self) -> Self {
        Self(self.0.wrapping_sub(other.0))
    }

    /// C++ `operator*=(uint64_t)`: `lo * b` as a full 128-bit product, `hi * b` wrapping, i.e.
    /// the product modulo `2^128`.
    #[must_use]
    pub const fn wrapping_mul_u64(self, b: u64) -> Self {
        Self(self.0.wrapping_mul(b as u128))
    }

    /// C++ `operator/=(uint64_t)`: exact 128/64 floor division. `None` for a zero divisor.
    #[must_use]
    pub const fn checked_div_u64(self, b: u64) -> Option<Self> {
        if b == 0 {
            None
        } else {
            Some(Self(self.0 / b as u128))
        }
    }

    /// C++ `operator/=(difficulty_type)` (`src/util.cpp`), step for step: 0 and 1 answered
    /// directly, a 64-bit divisor divided exactly, otherwise a quotient estimated from the
    /// normalised top 64 bits of the divisor and corrected once. The result is the floor of the
    /// quotient. `None` for a zero divisor.
    #[must_use]
    pub fn checked_div(self, b: Self) -> Option<Self> {
        if b.is_zero() {
            return None;
        }
        let (a, d) = (self.0, b.0);
        if a < d {
            return Some(Self::ZERO);
        }
        if a - d < d {
            return Some(Self(1));
        }
        if b.hi() == 0 {
            return self.checked_div_u64(b.lo());
        }
        // `bsr(b.hi) + 1`, in 1..=63 here: a divisor of 2^127 or more answered 1 above.
        let shift = 64 - b.hi().leading_zeros();
        // `shiftleft128(b.lo, b.hi, 64 - shift)`: the top 64 bits of the divisor.
        let divisor = ((d << (64 - shift)) >> 64) as u64;
        let (hi, lo) = (self.hi(), self.lo());
        let t = if hi < divisor {
            let q = ((u128::from(hi) << 64) | u128::from(lo)) / u128::from(divisor);
            (q >> shift) as u64
        } else {
            let q = ((u128::from(hi - divisor) << 64) | u128::from(lo)) / u128::from(divisor);
            // `shiftright128(q, 1, shift)`
            (((1u128 << 64) | u128::from(q as u64)) >> shift) as u64
        };
        // `product = b * t`; one too many when it exceeds 128 bits or the dividend.
        let low = u128::from(b.lo()) * u128::from(t);
        let high = u128::from(b.hi()) * u128::from(t);
        let (product_hi, carried) = ((low >> 64) as u64).overflowing_add(high as u64);
        let product = (u128::from(product_hi) << 64) | u128::from(low as u64);
        let t = if (high >> 64) != 0 || carried || a < product {
            t.wrapping_sub(1)
        } else {
            t
        };
        Some(Self(u128::from(t)))
    }

    /// C++ `difficulty_type::check_pow`: `pow_hash`, read as a little-endian 256-bit integer,
    /// times this difficulty is below `2^256`.
    #[must_use]
    pub fn check_pow(self, pow_hash: &Hash) -> bool {
        let h: [u64; 4] = core::array::from_fn(|i| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&pow_hash.0[i * 8..i * 8 + 8]);
            u64::from_le_bytes(w)
        });
        let d = [self.lo(), self.hi()];
        let mut r = [0u64; 6];
        for (i, hw) in h.iter().enumerate() {
            let mut carry = 0u128;
            for (j, dw) in d.iter().enumerate() {
                let cur = u128::from(r[i + j]) + u128::from(*hw) * u128::from(*dw) + carry;
                r[i + j] = cur as u64;
                carry = cur >> 64;
            }
            let mut k = i + 2;
            while carry != 0 && k < r.len() {
                let cur = u128::from(r[k]) + carry;
                r[k] = cur as u64;
                carry = cur >> 64;
                k += 1;
            }
        }
        r[4] == 0 && r[5] == 0
    }

    /// C++ `difficulty_type::target()`: the 64-bit stratum target `ceil(2^64 / lo)`; 1 when
    /// `hi != 0`, `u64::MAX` when `lo <= 1`.
    #[must_use]
    pub const fn target(self) -> u64 {
        if self.hi() != 0 {
            return 1;
        }
        let lo = self.lo();
        if lo <= 1 {
            return u64::MAX;
        }
        let n = 1u128 << 64;
        let q = (n / lo as u128) as u64;
        if n.is_multiple_of(lo as u128) {
            q
        } else {
            q + 1
        }
    }

    /// C++ `to_double()`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn to_f64(self) -> f64 {
        self.hi() as f64 * 18_446_744_073_709_551_616.0 + self.lo() as f64
    }
}

/// Plain decimal, no separators (C++ `operator<<(std::ostream&, const difficulty_type&)`).
impl fmt::Display for Difficulty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for Difficulty {
    fn from(v: u64) -> Self {
        Self::from_u64(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::xorshift64star;

    /// Literal port of `difficulty_type::check_pow` (`src/util.cpp`) as the reference.
    fn check_pow_reference(d: Difficulty, pow_hash: &Hash) -> bool {
        let a: [u64; 4] = core::array::from_fn(|i| {
            u64::from_le_bytes(pow_hash.0[i * 8..i * 8 + 8].try_into().unwrap())
        });
        let umul = |x: u64, y: u64| {
            let r = u128::from(x) * u128::from(y);
            (r as u64, (r >> 64) as u64)
        };
        let mut result = [0u64; 6];
        let mut product = [0u64; 6];
        if d.hi() == 0 {
            for i in (0..4).rev() {
                (product[0], product[1]) = umul(a[i], d.lo());
                let mut carry = 0u64;
                for (l, k) in (i..5).enumerate() {
                    let mut t = result[k].wrapping_add(product[l]);
                    let next_carry = u64::from(t < result[k]);
                    t = t.wrapping_add(carry);
                    carry = next_carry | u64::from(t < result[k]);
                    result[k] = t;
                }
                if result[4] != 0 {
                    return false;
                }
            }
        } else {
            let b = [d.lo(), d.hi()];
            for i in (0..4).rev() {
                for j in (0..2).rev() {
                    (product[0], product[1]) = umul(a[i], b[j]);
                    let mut carry = 0u64;
                    for (l, k) in (i + j..6).enumerate() {
                        let mut t = result[k].wrapping_add(product[l]);
                        let next_carry = u64::from(t < result[k]);
                        t = t.wrapping_add(carry);
                        carry = next_carry | u64::from(t < result[k]);
                        result[k] = t;
                    }
                    if result[4] != 0 || result[5] != 0 {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// 256-bit reference: `hash * d < 2^256` with schoolbook limbs and no early exit.
    fn check_pow_u256(d: Difficulty, pow_hash: &Hash) -> bool {
        let mut limbs = [0u128; 8];
        for (i, hb) in pow_hash.0.iter().enumerate() {
            for j in 0..16usize {
                let byte_d = (d.0 >> (8 * j)) as u8;
                limbs[(i + j) / 8] += (u128::from(*hb) * u128::from(byte_d)) << (8 * ((i + j) % 8));
            }
        }
        // Normalise base 2^64 carries.
        let mut carry = 0u128;
        let mut out = [0u64; 8];
        for (k, l) in limbs.iter().enumerate() {
            let cur = l + carry;
            out[k] = cur as u64;
            carry = cur >> 64;
        }
        carry == 0 && out[4..].iter().all(|w| *w == 0)
    }

    struct Rng(u64);
    impl Rng {
        fn step(&mut self) -> u64 {
            self.0 = xorshift64star(self.0);
            self.0
        }
        /// Values of every size: mostly small, sometimes full width.
        fn u128(&mut self) -> u128 {
            let bits = self.step() % 129;
            let v = (u128::from(self.step()) << 64) | u128::from(self.step());
            if bits == 0 { 0 } else { v >> (128 - bits) }
        }
        fn hash(&mut self) -> Hash {
            let mut h = [0u8; 32];
            let zero_top = (self.step() % 33) as usize;
            for (i, b) in h.iter_mut().enumerate() {
                *b = if i >= 32 - zero_top {
                    0
                } else {
                    self.step() as u8
                };
            }
            Hash(h)
        }
    }

    #[test]
    fn wrapping_rules() {
        let max = Difficulty::MAX;
        assert_eq!(max.wrapping_add(Difficulty(1)), Difficulty::ZERO);
        assert_eq!(Difficulty::ZERO.wrapping_sub(Difficulty(1)), max);
        assert_eq!(Difficulty::new(5, 7).lo(), 5);
        assert_eq!(Difficulty::new(5, 7).hi(), 7);
        // `hi * b` wraps, `lo * b` carries into `hi`.
        let d = Difficulty::new(u64::MAX, u64::MAX >> 1);
        assert_eq!(d.wrapping_mul_u64(4), Difficulty(d.0.wrapping_mul(4)));
        assert_eq!(
            Difficulty::new(u64::MAX, 0).wrapping_mul_u64(u64::MAX),
            Difficulty(u128::from(u64::MAX) * u128::from(u64::MAX))
        );
        assert_eq!(
            Difficulty::new(0, 1).wrapping_mul_u64(1 << 63),
            Difficulty::new(0, 1 << 63)
        );
        assert_eq!(
            Difficulty::new(0, 2).wrapping_mul_u64(1 << 63),
            Difficulty::ZERO
        );
        assert_eq!(Difficulty(150_802_240).to_string(), "150802240");
        assert_eq!(Difficulty::MAX.to_string(), u128::MAX.to_string());
    }

    #[test]
    fn division_matches_u128_floor_division() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..200_000 {
            let a = Difficulty(rng.u128());
            let b = Difficulty(rng.u128());
            if b.is_zero() {
                assert_eq!(a.checked_div(b), None);
                continue;
            }
            assert_eq!(a.checked_div(b), Some(Difficulty(a.0 / b.0)), "{a} / {b}");
            if b.hi() == 0 {
                assert_eq!(a.checked_div_u64(b.lo()), Some(Difficulty(a.0 / b.0)));
            }
        }
        // Edges: the estimate branch with the dividend's top word above the divisor's.
        for (a, b) in [
            (u128::MAX, (1u128 << 64) + 1),
            (u128::MAX, (1u128 << 126) + 12345),
            (u128::MAX - 1, u128::MAX >> 1),
            ((1u128 << 127) + 5, (1u128 << 64) | u128::from(u64::MAX)),
            (u128::MAX, 3),
        ] {
            assert_eq!(
                Difficulty(a).checked_div(Difficulty(b)),
                Some(Difficulty(a / b)),
                "{a} / {b}"
            );
        }
        assert_eq!(Difficulty(5).checked_div_u64(0), None);
    }

    #[test]
    fn check_pow_matches_the_references() {
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for _ in 0..50_000 {
            let d = Difficulty(rng.u128());
            let h = rng.hash();
            let got = d.check_pow(&h);
            assert_eq!(got, check_pow_reference(d, &h), "{d} {h}");
            assert_eq!(got, check_pow_u256(d, &h), "{d} {h}");
            if d.hi() == 0 {
                assert_eq!(got, xkr_crypto::check_hash(&h, d.lo()), "{d} {h}");
            }
        }
        // Boundaries: 2^256 - 1 meets difficulty 1 only.
        let ones = Hash([0xff; 32]);
        assert!(Difficulty(1).check_pow(&ones));
        assert!(!Difficulty(2).check_pow(&ones));
        assert!(Difficulty::MAX.check_pow(&Hash::ZERO));
        let mut top = [0u8; 32];
        top[15] = 1; // 2^120
        let d = Difficulty(1u128 << 120);
        assert_eq!(d.check_pow(&Hash(top)), check_pow_u256(d, &Hash(top)));
    }

    #[test]
    fn stratum_target() {
        assert_eq!(Difficulty::new(0, 1).target(), 1);
        assert_eq!(Difficulty(0).target(), u64::MAX);
        assert_eq!(Difficulty(1).target(), u64::MAX);
        assert_eq!(Difficulty(2).target(), 1 << 63);
        assert_eq!(Difficulty(3).target(), 6_148_914_691_236_517_206);
        assert_eq!(Difficulty(1000).target(), 18_446_744_073_709_552);
        assert_eq!(Difficulty(1 << 32).target(), 1 << 32);
    }
}
