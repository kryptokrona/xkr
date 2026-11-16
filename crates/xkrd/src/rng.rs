//! Random numbers for the node.
//!
//! * [`SystemRng`] / [`os_u64`]: every value comes from the operating system's random source
//!   (`xkr_crypto::os_rng`). Used for the P2P peer id, the choice of addresses to dial and the
//!   sample of keys a stripe lookup dials.
//! * [`Rng`]: xorshift64*, seeded from the operating system; cheap, for choices such as the
//!   stripe of a new pruned database. (The built-in miner's start nonces come from
//!   `xkr-miner`, which draws them from `xkr_crypto::os_rng`.)

/// 64 bits from the operating system's random source.
///
/// `xkr_crypto::ops::random_scalar` reduces 512 random bits modulo the group order
/// (about 2^252); the low 8 bytes of the result are uniform to within 2^-180.
#[must_use]
pub fn os_u64() -> u64 {
    let bytes = xkr_crypto::ops::random_scalar(&mut xkr_crypto::os_rng()).to_bytes();
    let mut low = [0u8; 8];
    low.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(low)
}

/// A source of random `u64`s with uniform indices and shuffles on top.
pub trait RandomSource {
    /// Next 64 random bits.
    fn next_u64(&mut self) -> u64;

    /// Uniform index below `n` (0 for `n == 0`).
    fn below(&mut self, n: usize) -> usize {
        let n = n.max(1) as u64;
        // Values at or above `limit` would make the low residues more likely.
        let limit = u64::MAX - u64::MAX % n;
        loop {
            let x = self.next_u64();
            if x < limit {
                return (x % n) as usize;
            }
        }
    }

    /// Shuffles `items` in place (Fisher-Yates).
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// Draws every value from the operating system ([`os_u64`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRng;

impl RandomSource for SystemRng {
    fn next_u64(&mut self) -> u64 {
        os_u64()
    }
}

/// xorshift64*, seeded from the operating system. One output reveals its state, so it is only
/// used where the next value need not be hidden.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A freshly seeded generator.
    #[must_use]
    pub fn new() -> Self {
        Self(os_u64() | 1)
    }

    /// Uniform index below `n` (0 for `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        RandomSource::below(self, n)
    }
}

impl RandomSource for Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// A shuffle is a permutation, and the first places are not always the same keys.
    fn shuffles_vary(rng: &mut impl RandomSource) {
        let mut firsts = HashSet::new();
        for _ in 0..64 {
            let mut v: Vec<u32> = (0..16).collect();
            rng.shuffle(&mut v);
            firsts.insert(v[0]);
            v.sort_unstable();
            assert_eq!(v, (0..16).collect::<Vec<_>>());
        }
        assert!(firsts.len() > 1, "the first key never changes");
        let mut empty: [u8; 0] = [];
        rng.shuffle(&mut empty);
        let mut one = [7];
        rng.shuffle(&mut one);
        assert_eq!(one, [7]);
    }

    #[test]
    fn shuffle_permutes_and_varies() {
        shuffles_vary(&mut Rng::new());
        shuffles_vary(&mut SystemRng);
    }

    /// Indices stay below the bound, every index shows up, and `below(0)` is 0.
    #[test]
    fn below_covers_the_range() {
        let mut rng = SystemRng;
        let mut seen = HashSet::new();
        for _ in 0..400 {
            let i = rng.below(5);
            assert!(i < 5);
            seen.insert(i);
        }
        assert_eq!(seen.len(), 5);
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.below(1), 0);
        assert!(Rng::new().below(3) < 3);
    }

    /// Operating-system values differ between calls, and two fresh generators start apart.
    #[test]
    fn os_values_differ() {
        let values: HashSet<u64> = (0..32).map(|_| os_u64()).collect();
        assert_eq!(values.len(), 32);
        assert_ne!(
            RandomSource::next_u64(&mut Rng::new()),
            RandomSource::next_u64(&mut Rng::new())
        );
    }
}
