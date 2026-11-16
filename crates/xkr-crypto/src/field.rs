//! Minimal arithmetic in GF(2^255 - 19), only for porting `ge_fromfe_frombytes_vartime`
//! (`src/crypto/crypto-ops.c`), which `curve25519-dalek` does not expose.
//!
//! Elements are four little-endian `u64` limbs, **always fully reduced** to `[0, p)`, so
//! equality, parity (`fe_isnegative`) and zero tests (`fe_isnonzero`) are on canonical values
//! exactly like ref10's `fe_tobytes`-based predicates. Everything is variable time; it only
//! ever processes public data (hashes of public keys).

const P: [u64; 4] = [
    0xffff_ffff_ffff_ffed,
    0xffff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
    0x7fff_ffff_ffff_ffff,
];

/// A reduced field element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fe([u64; 4]);

/// `(a + b, carry)` over 4 limbs.
fn add_raw(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], bool) {
    let mut r = [0u64; 4];
    let mut carry = 0u64;
    for i in 0..4 {
        let s = u128::from(a[i]) + u128::from(b[i]) + u128::from(carry);
        r[i] = s as u64;
        carry = (s >> 64) as u64;
    }
    (r, carry != 0)
}

/// `(a - b, borrow)` over 4 limbs.
fn sub_raw(a: &[u64; 4], b: &[u64; 4]) -> ([u64; 4], bool) {
    let mut r = [0u64; 4];
    let mut borrow = false;
    for i in 0..4 {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(u64::from(borrow));
        r[i] = d2;
        borrow = b1 || b2;
    }
    (r, borrow)
}

/// `a >= p`.
fn geq_p(a: &[u64; 4]) -> bool {
    for i in (0..4).rev() {
        if a[i] != P[i] {
            return a[i] > P[i];
        }
    }
    true
}

/// Reduces any 256-bit value (< 2^256 < 3p) to `[0, p)`.
fn reduce_256(mut a: [u64; 4]) -> [u64; 4] {
    while geq_p(&a) {
        a = sub_raw(&a, &P).0;
    }
    a
}

/// Reduces a 512-bit product using `2^256 = 38 (mod p)`.
fn reduce_512(t: &[u64; 8]) -> [u64; 4] {
    let mut r = [0u64; 4];
    let mut carry: u128 = 0;
    for i in 0..4 {
        let cur = u128::from(t[i]) + u128::from(t[i + 4]) * 38 + carry;
        r[i] = cur as u64;
        carry = cur >> 64;
    }
    // value = r + carry * 2^256 = r + carry * 38 (mod p), carry < 39
    let mut c = carry * 38;
    for limb in &mut r {
        let cur = u128::from(*limb) + c;
        *limb = cur as u64;
        c = cur >> 64;
    }
    if c != 0 {
        // Wrapped past 2^256 once: r is now tiny, add the remaining 38.
        r[0] += 38;
    }
    reduce_256(r)
}

impl Fe {
    pub(crate) const ZERO: Self = Self([0, 0, 0, 0]);
    pub(crate) const ONE: Self = Self([1, 0, 0, 0]);

    /// From reduced little-endian limbs (constants).
    pub(crate) const fn from_limbs(limbs: [u64; 4]) -> Self {
        Self(limbs)
    }

    /// Interprets all 256 bits of `s` (the top bit is **not** masked, as in the inlined
    /// `fe_frombytes` of `ge_fromfe_frombytes_vartime`) and reduces mod p.
    pub(crate) fn from_bytes_wide(s: &[u8; 32]) -> Self {
        let mut limbs = [0u64; 4];
        for (limb, chunk) in limbs.iter_mut().zip(s.as_chunks::<8>().0) {
            *limb = u64::from_le_bytes(*chunk);
        }
        Self(reduce_256(limbs))
    }

    /// Canonical little-endian encoding (`fe_tobytes`).
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (chunk, limb) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.0) {
            *chunk = limb.to_le_bytes();
        }
        out
    }

    /// `fe_isnonzero`.
    pub(crate) fn is_nonzero(self) -> bool {
        self.0 != [0, 0, 0, 0]
    }

    /// `fe_isnegative`: lowest bit of the canonical encoding.
    pub(crate) fn is_negative(self) -> bool {
        self.0[0] & 1 == 1
    }

    /// `fe_add`.
    pub(crate) fn add(self, b: Self) -> Self {
        // Both < p < 2^255, so the sum fits in 256 bits.
        let (s, _) = add_raw(&self.0, &b.0);
        Self(reduce_256(s))
    }

    /// `fe_sub`.
    pub(crate) fn sub(self, b: Self) -> Self {
        // a + p - b, with a + p < 2^256 and a + p > b.
        let (t, _) = add_raw(&self.0, &P);
        let (d, _) = sub_raw(&t, &b.0);
        Self(reduce_256(d))
    }

    /// `fe_neg`.
    pub(crate) fn neg(self) -> Self {
        Self::ZERO.sub(self)
    }

    /// `fe_mul`.
    pub(crate) fn mul(self, b: Self) -> Self {
        let a = &self.0;
        let b = &b.0;
        let mut t = [0u64; 8];
        for i in 0..4 {
            let mut carry: u128 = 0;
            for j in 0..4 {
                let cur = u128::from(t[i + j]) + u128::from(a[i]) * u128::from(b[j]) + carry;
                t[i + j] = cur as u64;
                carry = cur >> 64;
            }
            t[i + 4] = carry as u64;
        }
        Self(reduce_512(&t))
    }

    /// `fe_sq`.
    pub(crate) fn square(self) -> Self {
        self.mul(self)
    }

    /// `n` successive squarings.
    fn square_n(self, n: usize) -> Self {
        let mut r = self;
        for _ in 0..n {
            r = r.square();
        }
        r
    }

    /// `fe_pow22523`: `z^((p-5)/8) = z^(2^252 - 3)`, ref10 addition chain.
    pub(crate) fn pow22523(self) -> Self {
        let z = self;
        let t0 = z.square(); // z^2
        let t1 = t0.square_n(2); // z^8
        let t1 = z.mul(t1); // z^9
        let t0 = t0.mul(t1); // z^11
        let t0 = t0.square(); // z^22
        let t0 = t1.mul(t0); // z^31 = z^(2^5 - 1)
        let t1 = t0.square_n(5);
        let t0 = t1.mul(t0); // z^(2^10 - 1)
        let t1 = t0.square_n(10);
        let t1 = t1.mul(t0); // z^(2^20 - 1)
        let t2 = t1.square_n(20);
        let t1 = t2.mul(t1); // z^(2^40 - 1)
        let t1 = t1.square_n(10);
        let t0 = t1.mul(t0); // z^(2^50 - 1)
        let t1 = t0.square_n(50);
        let t1 = t1.mul(t0); // z^(2^100 - 1)
        let t2 = t1.square_n(100);
        let t1 = t2.mul(t1); // z^(2^200 - 1)
        let t1 = t1.square_n(50);
        let t0 = t1.mul(t0); // z^(2^250 - 1)
        let t0 = t0.square_n(2); // z^(2^252 - 4)
        t0.mul(z) // z^(2^252 - 3)
    }

    /// `fe_invert`: `z^(p-2)`, ref10 addition chain. Maps 0 to 0.
    pub(crate) fn invert(self) -> Self {
        let z = self;
        let t0 = z.square(); // z^2
        let t1 = t0.square_n(2); // z^8
        let t1 = z.mul(t1); // z^9
        let t0 = t0.mul(t1); // z^11
        let t2 = t0.square(); // z^22
        let t1 = t1.mul(t2); // z^31 = z^(2^5 - 1)
        let t2 = t1.square_n(5);
        let t1 = t2.mul(t1); // z^(2^10 - 1)
        let t2 = t1.square_n(10);
        let t2 = t2.mul(t1); // z^(2^20 - 1)
        let t3 = t2.square_n(20);
        let t2 = t3.mul(t2); // z^(2^40 - 1)
        let t2 = t2.square_n(10);
        let t1 = t2.mul(t1); // z^(2^50 - 1)
        let t2 = t1.square_n(50);
        let t2 = t2.mul(t1); // z^(2^100 - 1)
        let t3 = t2.square_n(100);
        let t2 = t3.mul(t2); // z^(2^200 - 1)
        let t2 = t2.square_n(50);
        let t1 = t2.mul(t1); // z^(2^250 - 1)
        let t1 = t1.square_n(5); // z^(2^255 - 32)
        t1.mul(t0) // z^(2^255 - 21) = z^(p - 2)
    }

    /// Generic square-and-multiply, used by tests to cross-check the addition chains.
    #[cfg(test)]
    pub(crate) fn pow_vartime(self, exp: &[u64; 4]) -> Self {
        let mut r = Self::ONE;
        for i in (0..256).rev() {
            r = r.square();
            if (exp[i / 64] >> (i % 64)) & 1 == 1 {
                r = r.mul(self);
            }
        }
        r
    }
}

/// C++ `fe_divpowm1(r, u, v)`: `u * v^3 * (u * v^7)^((p-5)/8)`.
pub(crate) fn divpowm1(u: Fe, v: Fe) -> Fe {
    let v3 = v.square().mul(v); // v^3
    let uv7 = v3.square().mul(v).mul(u); // u v^7
    let t0 = uv7.pow22523();
    t0.mul(v3).mul(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seed: u64) -> Fe {
        let mut b = [0u8; 32];
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        for byte in &mut b {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *byte = x as u8;
        }
        Fe::from_bytes_wide(&b)
    }

    #[test]
    fn reduction_of_wide_inputs() {
        // 2^256 - 1 = 2p + 37
        assert_eq!(
            Fe::from_bytes_wide(&[0xff; 32]),
            Fe::from_limbs([37, 0, 0, 0])
        );
        // p itself reduces to 0
        let mut p = [0xffu8; 32];
        p[0] = 0xed;
        p[31] = 0x7f;
        assert_eq!(Fe::from_bytes_wide(&p), Fe::ZERO);
    }

    #[test]
    fn field_axioms_on_samples() {
        for s in 1..50 {
            let a = sample(s);
            let b = sample(s + 1000);
            let c = sample(s + 2000);
            assert_eq!(a.add(b).sub(b), a);
            assert_eq!(a.mul(b), b.mul(a));
            assert_eq!(a.mul(b.add(c)), a.mul(b).add(a.mul(c)));
            assert_eq!(a.add(a.neg()), Fe::ZERO);
            if a.is_nonzero() {
                assert_eq!(a.mul(a.invert()), Fe::ONE);
            }
        }
        // -1 * -1 = 1
        let m1 = Fe::ONE.neg();
        assert_eq!(m1.mul(m1), Fe::ONE);
        assert_eq!(Fe::ZERO.invert(), Fe::ZERO);
    }

    #[test]
    fn addition_chains_match_generic_pow() {
        const P_MINUS_2: [u64; 4] = [
            0xffff_ffff_ffff_ffeb,
            u64::MAX,
            u64::MAX,
            0x7fff_ffff_ffff_ffff,
        ];
        const P_MINUS_5_OVER_8: [u64; 4] = [
            0xffff_ffff_ffff_fffd,
            u64::MAX,
            u64::MAX,
            0x0fff_ffff_ffff_ffff,
        ];
        for s in 1..10 {
            let a = sample(s);
            assert_eq!(a.invert(), a.pow_vartime(&P_MINUS_2));
            assert_eq!(a.pow22523(), a.pow_vartime(&P_MINUS_5_OVER_8));
        }
    }
}
