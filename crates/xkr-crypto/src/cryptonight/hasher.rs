//! A reusable CryptoNight hasher for miners and pools: C++ `cn_slow_hash`
//! (`src/crypto/slow-hash.c`, x86-64 path) with the scratchpad kept between hashes and the AES
//! rounds on the CPU's AES instructions.
//!
//! [`super::cn_slow_hash`] allocates its scratchpad (up to 2 MiB) on every call and runs table
//! AES in software, which is fine for block verification. [`CnHasher`] owns the scratchpad, so
//! hashing allocates nothing, and with [`AesImpl::Hardware`] it runs the ten-round "pseudo
//! rounds" of steps 2 and 4 (eight blocks at a time) and the single rounds of the main loop
//! through the `aes` crate's `hazmat` round functions: AES-NI on x86 / x86-64 and the `ARMv8`
//! AES instructions, detected at run time, with the crate's own software rounds where the CPU has
//! none. [`AesImpl::Software`] uses the same table rounds as [`super::cn_slow_hash`].
//!
//! Both give the same hash as [`super::cn_slow_hash`] for every variant and parameter preset
//! (`docs/test-vectors/cryptonight.txt`, `tests/cn_hasher.rs`).

use std::fmt;

use aes::Block;
use aes::hazmat::{Block8, cipher_round, cipher_round_par};

use super::cnaes;
use super::hash_v2 as v2;
use super::slow_hash::{
    AES_KEY_SIZE, CnParams, DataCanNotBeHashed, INIT_BLOCKS, Variant, e2i, extra_hashes, get_init,
    get_k, hash_permutation, mul, set_init, sum_half_blocks, variant_2_2, variant_2_init,
    variant1_1, variant1_2, variant1_init,
};
use super::util::subarray;

/// The AES round implementation a [`CnHasher`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AesImpl {
    /// The CPU's AES instructions (AES-NI, `ARMv8` AES) through the `aes` crate's `hazmat`
    /// rounds, detected at run time; the crate's software rounds where the CPU has none.
    Hardware,
    /// Table-based software rounds (`slow-hash.c` `aesb_single_round` / `aesb_pseudo_round`),
    /// the same code as [`super::cn_slow_hash`].
    Software,
}

impl AesImpl {
    /// [`AesImpl::Hardware`] when the CPU has AES instructions ([`hardware_aes_available`]),
    /// otherwise [`AesImpl::Software`] (the table rounds are faster than bitsliced AES on one
    /// block at a time).
    #[must_use]
    pub fn detect() -> Self {
        if hardware_aes_available() {
            Self::Hardware
        } else {
            Self::Software
        }
    }

    /// `hardware` or `software`, for logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
        }
    }
}

/// Whether the CPU has AES instructions the `aes` crate uses (x86 / x86-64 `aes`, `AArch64`
/// `aes`).
#[must_use]
pub fn hardware_aes_available() -> bool {
    cpu_has_aes()
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn cpu_has_aes() -> bool {
    std::arch::is_x86_feature_detected!("aes")
}

#[cfg(target_arch = "aarch64")]
fn cpu_has_aes() -> bool {
    std::arch::is_aarch64_feature_detected!("aes")
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn cpu_has_aes() -> bool {
    false
}

/// The AES operations of CryptoNight: the key schedule of the 32-byte keys, ten rounds over
/// the eight blocks of a 128-byte line (`aesb_pseudo_round` on each), and one round
/// (`aesb_single_round`, the same as x86 `AESENC`).
trait Rounds {
    /// The expanded round keys in the form the rounds take.
    type Keys;
    fn expand(key: &[u8; AES_KEY_SIZE]) -> Self::Keys;
    fn pseudo_round8(text: &mut [u128; INIT_BLOCKS], keys: &Self::Keys);
    fn single_round(block: &mut u128, key: u128);
}

/// Table rounds (`cnaes`).
#[derive(Clone, Copy)]
struct Soft;

impl Rounds for Soft {
    type Keys = [u128; 10];

    fn expand(key: &[u8; AES_KEY_SIZE]) -> Self::Keys {
        cnaes::key_extend(key)
    }

    #[inline]
    fn pseudo_round8(text: &mut [u128; INIT_BLOCKS], keys: &Self::Keys) {
        for block in text.iter_mut() {
            *block = cnaes::aesb_pseudo_round(*block, keys);
        }
    }

    #[inline]
    fn single_round(block: &mut u128, key: u128) {
        cnaes::aesb_single_round(block, key);
    }
}

/// `aes::hazmat` rounds. A `u128` block is the 16 bytes of the scratchpad read little-endian,
/// so the bytes handed to the AES rounds are its `to_le_bytes`.
#[derive(Clone, Copy)]
struct Hard;

#[inline]
fn to_block(x: u128) -> Block {
    Block::from(x.to_le_bytes())
}

#[inline]
fn from_block(b: Block) -> u128 {
    u128::from_le_bytes(b.into())
}

impl Rounds for Hard {
    /// Each round key repeated for the eight blocks of a line.
    type Keys = [Block8; 10];

    fn expand(key: &[u8; AES_KEY_SIZE]) -> Self::Keys {
        cnaes::key_extend(key).map(|round_key| Block8::from([to_block(round_key); 8]))
    }

    #[inline]
    fn pseudo_round8(text: &mut [u128; INIT_BLOCKS], keys: &Self::Keys) {
        let mut blocks = Block8::from((*text).map(to_block));
        for round_keys in keys {
            cipher_round_par(&mut blocks, round_keys);
        }
        for (t, b) in text.iter_mut().zip(blocks.iter()) {
            *t = from_block(*b);
        }
    }

    #[inline]
    fn single_round(block: &mut u128, key: u128) {
        let mut b = to_block(*block);
        cipher_round(&mut b, &to_block(key));
        *block = from_block(b);
    }
}

/// C++ `cn_slow_hash` (x86-64 path, `prehashed = 0`) into a caller-owned scratchpad of
/// `CnParams::scratchpad_blocks` blocks; the loop of [`super::cn_slow_hash`] with the AES
/// rounds of `A`. The caller checked the variant 1 input length.
fn hash_into<A: Rounds>(
    _rounds: A,
    data: &[u8],
    variant: Variant,
    params: &CnParams,
    long_state: &mut [u128],
) -> [u8; 32] {
    let mask = params.mask();
    let init_blocks = params.init_blocks();
    let rounds = params.rounds();

    // Step 1: Keccak1600 initialises the state.
    let mut state = crate::keccak::keccak1600(data);
    let keys = A::expand(subarray(&state, 0));
    let mut text = get_init(&state);

    let tweak1_2 = variant1_init(&state, data, variant);
    let mut b = [0_u128; 2];
    let (mut division_result, mut sqrt_result) = variant_2_init(&mut b, &state, variant);

    // Step 2: fill the scratchpad line by line (block `i` of a line is `text[i]` after one
    // more pseudo round). Blocks past the initialised region (none for the presets) are
    // zero, as in a fresh allocation.
    let (filled, rest) = long_state.split_at_mut(init_blocks);
    for line in filled.as_chunks_mut::<INIT_BLOCKS>().0 {
        A::pseudo_round8(&mut text, &keys);
        *line = text;
    }
    rest.fill(0);

    let k = get_k(&state);
    let mut a = k[0] ^ k[2];
    b[0] = k[1] ^ k[3];

    // Step 3: `iterations / 2` rounds of two random scratchpad accesses each.
    for _ in 0..rounds {
        // Iteration 1
        let mut j = e2i(a, mask);
        let mut c1 = long_state[j];
        A::single_round(&mut c1, a);
        v2::variant2_shuffle_add(&mut c1, a, &b, long_state, j, variant);

        long_state[j] = c1 ^ b[0];
        variant1_1(&mut long_state[j], variant);

        // Iteration 2
        j = e2i(c1, mask);
        let mut c2 = long_state[j];

        let mut a1 = a;
        v2::variant2_integer_math(&mut c2, c1, &mut division_result, &mut sqrt_result, variant);
        let mut d = mul(c1 as u64, c2 as u64);
        variant_2_2(long_state, j, &mut d, variant);
        v2::variant2_shuffle_add(&mut c1, a, &b, long_state, j, variant);
        a1 = sum_half_blocks(a1, d);
        std::mem::swap(&mut a1, &mut c2);
        a1 ^= c2;
        variant1_2(&mut c2, tweak1_2, variant);
        long_state[j] = c2;

        if variant == Variant::V2 {
            b[1] = b[0];
        }
        b[0] = c1;
        a = a1;
    }

    // Step 4: fold the scratchpad back into the state.
    let mut text = get_init(&state);
    let keys = A::expand(subarray(&state, AES_KEY_SIZE));
    for line in long_state[..init_blocks].as_chunks::<INIT_BLOCKS>().0 {
        for (block, ls) in text.iter_mut().zip(line) {
            *block ^= *ls;
        }
        A::pseudo_round8(&mut text, &keys);
    }
    set_init(&mut state, &text);

    // Step 5: permute and finalise with one of four hashes.
    hash_permutation(&mut state);
    extra_hashes(&state)
}

/// CryptoNight for one variant and parameter preset with its own scratchpad: hashing
/// allocates nothing. One per hashing thread.
pub struct CnHasher {
    variant: Variant,
    params: CnParams,
    aes: AesImpl,
    long_state: Vec<u128>,
}

impl fmt::Debug for CnHasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CnHasher")
            .field("variant", &self.variant)
            .field("params", &self.params)
            .field("aes", &self.aes)
            .field("scratchpad_bytes", &(self.long_state.len() * 16))
            .finish()
    }
}

impl CnHasher {
    /// A hasher with [`AesImpl::detect`].
    #[must_use]
    pub fn new(variant: Variant, params: CnParams) -> Self {
        Self::with_aes(variant, params, AesImpl::detect())
    }

    /// A hasher with the given AES implementation ([`AesImpl::Software`] runs the same code as
    /// [`super::cn_slow_hash`] on any CPU).
    #[must_use]
    pub fn with_aes(variant: Variant, params: CnParams, aes: AesImpl) -> Self {
        Self {
            variant,
            params,
            aes,
            long_state: vec![0_u128; params.scratchpad_blocks()],
        }
    }

    /// C++ `cn_slow_hash_v0` (CryptoNight, blocks of major version 1-3).
    #[must_use]
    pub fn cn_v0() -> Self {
        Self::new(Variant::V0, CnParams::CN)
    }

    /// C++ `cn_lite_slow_hash_v1` (CryptoNight-Lite variant 1, major version 4).
    #[must_use]
    pub fn cn_lite_v1() -> Self {
        Self::new(Variant::V1, CnParams::CN_LITE)
    }

    /// C++ `cn_turtle_lite_slow_hash_v2` (CryptoNight-Turtle-Lite variant 2, major version
    /// 5 and later).
    #[must_use]
    pub fn cn_turtle_lite_v2() -> Self {
        Self::new(Variant::V2, CnParams::CN_TURTLE_LITE)
    }

    /// The hash of `data`; the same as [`super::cn_slow_hash`]`(data, variant, params)`.
    /// Variant 1 needs at least 43 bytes of input.
    pub fn hash(&mut self, data: &[u8]) -> Result<[u8; 32], DataCanNotBeHashed> {
        if self.variant == Variant::V1 && data.len() < 43 {
            return Err(DataCanNotBeHashed);
        }
        Ok(match self.aes {
            AesImpl::Hardware => {
                hash_into(Hard, data, self.variant, &self.params, &mut self.long_state)
            }
            AesImpl::Software => {
                hash_into(Soft, data, self.variant, &self.params, &mut self.long_state)
            }
        })
    }

    /// The variant.
    #[must_use]
    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The parameter preset.
    #[must_use]
    pub fn params(&self) -> CnParams {
        self.params
    }

    /// The AES implementation.
    #[must_use]
    pub fn aes(&self) -> AesImpl {
        self.aes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One AES round through `aes::hazmat` equals the table round, for both byte orders that
    /// matter (scratchpad block and key read little-endian).
    #[test]
    fn hazmat_round_equals_table_round() {
        let mut x = 0x0123_4567_89ab_cdef_u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200 {
            let block = (u128::from(next()) << 64) | u128::from(next());
            let key = (u128::from(next()) << 64) | u128::from(next());
            let mut soft = block;
            let mut hard = block;
            Soft::single_round(&mut soft, key);
            Hard::single_round(&mut hard, key);
            assert_eq!(soft, hard);
        }
        let key = [7u8; AES_KEY_SIZE];
        let mut soft = [0x1111_u128, 2, 3, 4, 5, 6, 7, u128::MAX];
        let mut hard = soft;
        Soft::pseudo_round8(&mut soft, &Soft::expand(&key));
        Hard::pseudo_round8(&mut hard, &Hard::expand(&key));
        assert_eq!(soft, hard);
    }

    #[test]
    fn variant1_needs_43_bytes() {
        let mut h = CnHasher::with_aes(Variant::V1, CnParams::CN_TURTLE_LITE, AesImpl::Software);
        assert_eq!(h.hash(&[0u8; 42]), Err(DataCanNotBeHashed));
        assert!(h.hash(&[0u8; 43]).is_ok());
    }
}
