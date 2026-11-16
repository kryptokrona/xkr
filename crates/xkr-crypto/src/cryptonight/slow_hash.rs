// Vendored from Cuprate <https://github.com/Cuprate/cuprate>, crate `cuprate-cryptonight`
// (`cryptonight/src/slow_hash.rs`), rev a7b28239c0b8aac1752fbe74c913002c9ce29958.
// MIT License, Copyright (c) 2023-2024 Cuprate Contributors; see ./LICENSE-MIT.
// Local modifications are listed in crates/xkr-crypto/VENDORED.md. In short: scratchpad size,
// address mask and iteration count are parameters (`CnParams`) instead of the fixed Monero 2 MiB
// / 2^20 constants, CryptoNight-R was removed, and Keccak comes from `crate::keccak`.

use std::mem::swap;

use digest::Digest as _;
use groestl::Groestl256;
use jh::Jh256;
use skein::{Skein512, consts::U32};

use crate::cryptonight::{
    blake256::{Blake256, Digest as _},
    cnaes::{self, AES_BLOCK_SIZE, CN_AES_KEY_SIZE},
    hash_v2 as v2,
    util::{subarray, subarray_copy},
};

pub(super) const AES_KEY_SIZE: usize = CN_AES_KEY_SIZE;
pub(super) const INIT_BLOCKS: usize = 8;
const INIT_SIZE_BYTE: usize = INIT_BLOCKS * AES_BLOCK_SIZE;

pub(super) const KECCAK1600_BYTE_SIZE: usize = 200;

/// CryptoNight tweak variant (the C++ `variant` argument of `cn_slow_hash`).
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Variant {
    /// Original CryptoNight.
    V0,
    /// Variant 1 (Monero v7 tweak); needs at least 43 bytes of input.
    V1,
    /// Variant 2 (Monero v8: shuffle + integer math).
    V2,
}

/// Memory and iteration parameters of C++ `cn_slow_hash(data, length, hash, light, variant,
/// prehashed, page_size, scratchpad, iterations)` (`src/crypto/slow-hash.c`, x86-64 path,
/// which is what every mainnet node runs):
///
/// * `TOTALBLOCKS = page_size / 16`; the random-access address mask is
///   `TOTALBLOCKS / (light ? 2 : 1) - 1` blocks;
/// * `init_rounds = scratchpad / 128`: that many 128-byte lines are filled in step 2 and read
///   back in step 4;
/// * the main loop runs `iterations / 2` times (two scratchpad accesses per round).
///
/// For CN-Turtle-Lite this means a 256 KiB scratchpad of which only the first 128 KiB is
/// addressed by the main loop. Fields are private so only the reviewed presets below exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CnParams {
    page_size: u32,
    scratchpad: u32,
    iterations: u32,
    light: bool,
}

impl CnParams {
    /// `CN_PAGE_SIZE`, `CN_SCRATCHPAD`, `CN_ITERATIONS`, light = 0 (`hash.h`).
    pub const CN: Self = Self {
        page_size: 2_097_152,
        scratchpad: 2_097_152,
        iterations: 1_048_576,
        light: false,
    };
    /// `CN_LITE_*`, light = 1.
    pub const CN_LITE: Self = Self {
        page_size: 2_097_152,
        scratchpad: 1_048_576,
        iterations: 524_288,
        light: true,
    };
    /// `CN_DARK_*`, light = 0.
    pub const CN_DARK: Self = Self {
        page_size: 524_288,
        scratchpad: 524_288,
        iterations: 262_144,
        light: false,
    };
    /// `CN_DARK_*`, light = 1.
    pub const CN_DARK_LITE: Self = Self {
        page_size: 524_288,
        scratchpad: 524_288,
        iterations: 262_144,
        light: true,
    };
    /// `CN_TURTLE_*`, light = 0.
    pub const CN_TURTLE: Self = Self {
        page_size: 262_144,
        scratchpad: 262_144,
        iterations: 131_072,
        light: false,
    };
    /// `CN_TURTLE_*`, light = 1.
    pub const CN_TURTLE_LITE: Self = Self {
        page_size: 262_144,
        scratchpad: 262_144,
        iterations: 131_072,
        light: true,
    };

    /// Address mask in 16-byte blocks (`TOTALBLOCKS / lightFlag - 1`).
    pub(super) const fn mask(&self) -> usize {
        let total_blocks = self.page_size as usize / AES_BLOCK_SIZE;
        let light_flag = if self.light { 2 } else { 1 };
        total_blocks / light_flag - 1
    }

    /// Number of 16-byte blocks initialised in step 2 and folded back in step 4.
    pub(super) const fn init_blocks(&self) -> usize {
        (self.scratchpad as usize / INIT_SIZE_BYTE) * INIT_BLOCKS
    }

    /// Main-loop rounds (`iterations / 2`).
    pub(super) const fn rounds(&self) -> u32 {
        self.iterations / 2
    }

    /// Length of the scratchpad in 16-byte blocks: every address the mask can produce is in
    /// bounds even if `mask + 1` exceeds the initialised region (never the case for the
    /// presets).
    pub(super) fn scratchpad_blocks(&self) -> usize {
        self.init_blocks().max(self.mask() + 1)
    }
}

/// Returned for variant 1 when the input is shorter than 43 bytes.
#[derive(thiserror::Error, Debug, Copy, Clone, Eq, PartialEq)]
#[error("CryptoNight variant 1 needs at least 43 bytes of data")]
pub struct DataCanNotBeHashed;

fn get_keccak_word(state: &[u8; KECCAK1600_BYTE_SIZE], index: usize) -> u64 {
    u64::from_le_bytes(subarray_copy(state, index * 8))
}

pub(super) fn get_k(state: &[u8; KECCAK1600_BYTE_SIZE]) -> [u128; 4] {
    [
        u128::from_le_bytes(subarray_copy(state, 0)),
        u128::from_le_bytes(subarray_copy(state, 16)),
        u128::from_le_bytes(subarray_copy(state, 32)),
        u128::from_le_bytes(subarray_copy(state, 48)),
    ]
}

pub(super) fn get_init(state: &[u8; KECCAK1600_BYTE_SIZE]) -> [u128; INIT_BLOCKS] {
    let mut init = [0_u128; INIT_BLOCKS];
    for (i, block) in init.iter_mut().enumerate() {
        *block = u128::from_le_bytes(subarray_copy(state, 64 + i * AES_BLOCK_SIZE));
    }
    init
}

pub(super) fn set_init(state: &mut [u8; KECCAK1600_BYTE_SIZE], init: &[u128; INIT_BLOCKS]) {
    for (i, block) in init.iter().enumerate() {
        state[64 + i * AES_BLOCK_SIZE..64 + (i + 1) * AES_BLOCK_SIZE]
            .copy_from_slice(&block.to_le_bytes());
    }
}

/// C++ `hash_permutation` (`hash.c`): Keccak-f\[1600\] over the 200-byte state.
pub(super) fn hash_permutation(b: &mut [u8; KECCAK1600_BYTE_SIZE]) {
    let mut state = [0_u64; 25];
    for (i, state_i) in state.iter_mut().enumerate() {
        *state_i = u64::from_le_bytes(subarray_copy(b, i * 8));
    }
    crate::keccak::keccakf(&mut state);
    for (i, chunk) in state.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&chunk.to_le_bytes());
    }
}

/// C++ `state_index(x, lightFlag)`, in blocks instead of bytes.
#[inline]
#[expect(clippy::cast_possible_truncation)]
pub(super) const fn e2i(a: u128, mask: usize) -> usize {
    // truncates upper 64 bits before dividing
    let value = (a as u64) / (AES_BLOCK_SIZE as u64);
    (value as usize) & mask
}

#[expect(clippy::cast_possible_truncation)]
pub(super) fn mul(a: u64, b: u64) -> u128 {
    let product = u128::from(a).wrapping_mul(u128::from(b));
    let hi = (product >> 64) as u64;
    let lo = product as u64;

    // swap hi and low, so this isn't just a multiply
    (u128::from(lo) << 64) | u128::from(hi)
}

#[expect(clippy::cast_possible_truncation)]
pub(super) fn sum_half_blocks(a: u128, b: u128) -> u128 {
    let a_low = a as u64;
    let b_low = b as u64;
    let sum_low = a_low.wrapping_add(b_low);

    let a_high = (a >> 64) as u64;
    let b_high = (b >> 64) as u64;
    let sum_high = a_high.wrapping_add(b_high);

    (u128::from(sum_high) << 64) | u128::from(sum_low)
}

/// C++ `VARIANT1_INIT64` (`NONCE_POINTER` = data + 35). Caller checked `data.len() >= 43`.
pub(super) fn variant1_init(
    state: &[u8; KECCAK1600_BYTE_SIZE],
    data: &[u8],
    variant: Variant,
) -> u64 {
    const NONCE_PTR_INDEX: usize = 35;

    if variant != Variant::V1 {
        return 0;
    }

    let mut tweak1_2 = u64::from_le_bytes(subarray_copy(state, 192));
    tweak1_2 ^= u64::from_le_bytes(subarray_copy(data, NONCE_PTR_INDEX));
    tweak1_2
}

/// C++ `VARIANT1_1`.
pub(super) fn variant1_1(p: &mut u128, variant: Variant) {
    const MASK_BYTE11: u128 = !(0xFF << (11 * 8)); // all bits except the 11th byte are ones
    const TABLE: u32 = 0x75310_u32;

    #[expect(clippy::cast_possible_truncation)]
    if variant == Variant::V1 {
        let old_byte11 = (*p >> (11 * 8)) as u8;
        let index = (((old_byte11 >> 3) & 6) | (old_byte11 & 1)) << 1;
        let new_byte11 = old_byte11 ^ ((TABLE >> index) & 0x30) as u8;
        *p = (*p & MASK_BYTE11) | (u128::from(new_byte11) << (11 * 8));
    }
}

/// C++ `VARIANT1_2`.
pub(super) fn variant1_2(c2: &mut u128, tweak1_2: u64, variant: Variant) {
    if variant == Variant::V1 {
        *c2 ^= u128::from(tweak1_2) << 64;
    }
}

/// C++ `VARIANT2_INIT64`.
pub(super) fn variant_2_init(
    b: &mut [u128; 2],
    state: &[u8; KECCAK1600_BYTE_SIZE],
    variant: Variant,
) -> (u64, u64) {
    if variant != Variant::V2 {
        return (0, 0);
    }

    b[1] = u128::from_le_bytes(subarray_copy(state, 64))
        ^ u128::from_le_bytes(subarray_copy(state, 80));
    let division_result = get_keccak_word(state, 12);
    let sqrt_result = get_keccak_word(state, 13);

    (division_result, sqrt_result)
}

/// C++ `VARIANT2_2`.
pub(super) fn variant_2_2(long_state: &mut [u128], j: usize, d: &mut u128, variant: Variant) {
    if variant == Variant::V2 {
        let chunk1_start = j ^ 0x1;
        let chunk2_start = j ^ 0x2;
        long_state[chunk1_start] ^= *d;
        *d ^= long_state[chunk2_start];
    }
}

/// C++ `extra_hashes[state.hs.b[0] & 3]`: BLAKE-256, Groestl-256, JH-256, Skein-512-256.
pub(super) fn extra_hashes(input: &[u8; KECCAK1600_BYTE_SIZE]) -> [u8; 32] {
    match input[0] & 0x3 {
        0 => Blake256::digest(input),
        1 => Groestl256::digest(input).into(),
        2 => Jh256::digest(input).into(),
        _ => Skein512::<U32>::digest(input).into(),
    }
}

/// C++ `cn_slow_hash` (x86-64 path, `prehashed = 0`) for the given variant and parameters.
#[expect(clippy::cast_possible_truncation)]
pub(crate) fn cn_slow_hash(
    data: &[u8],
    variant: Variant,
    params: &CnParams,
) -> Result<[u8; 32], DataCanNotBeHashed> {
    if variant == Variant::V1 && data.len() < 43 {
        return Err(DataCanNotBeHashed);
    }

    let mask = params.mask();
    let init_blocks = params.init_blocks();
    let rounds = params.iterations / 2;

    // Step 1: Keccak1600 initialises the state.
    let mut state = crate::keccak::keccak1600(data);
    let aes_expanded_key = cnaes::key_extend(subarray(&state, 0));
    let mut text = get_init(&state);

    let tweak1_2 = variant1_init(&state, data, variant);
    let mut b = [0_u128; 2];
    let (mut division_result, mut sqrt_result) = variant_2_init(&mut b, &state, variant);

    // Step 2: fill the scratchpad. Allocated so that every address the mask can produce is in
    // bounds even if `mask + 1` exceeds the initialised region (never the case for the presets).
    let mut long_state = vec![0_u128; init_blocks.max(mask + 1)];
    for (i, slot) in long_state.iter_mut().take(init_blocks).enumerate() {
        let block = &mut text[i % INIT_BLOCKS];
        *block = cnaes::aesb_pseudo_round(*block, &aes_expanded_key);
        *slot = *block;
    }

    let k = get_k(&state);
    let mut a = k[0] ^ k[2];
    b[0] = k[1] ^ k[3];

    // Step 3: `iterations / 2` rounds of two random scratchpad accesses each.
    for _ in 0..rounds {
        // Iteration 1
        let mut j = e2i(a, mask);
        let mut c1 = long_state[j];
        cnaes::aesb_single_round(&mut c1, a);
        v2::variant2_shuffle_add(&mut c1, a, &b, &mut long_state, j, variant);

        long_state[j] = c1 ^ b[0];
        variant1_1(&mut long_state[j], variant);

        // Iteration 2
        j = e2i(c1, mask);
        let mut c2 = long_state[j];

        let mut a1 = a;
        v2::variant2_integer_math(&mut c2, c1, &mut division_result, &mut sqrt_result, variant);
        let mut d = mul(c1 as u64, c2 as u64);
        variant_2_2(&mut long_state, j, &mut d, variant);
        v2::variant2_shuffle_add(&mut c1, a, &b, &mut long_state, j, variant);
        a1 = sum_half_blocks(a1, d);
        swap(&mut a1, &mut c2);
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
    let aes_expanded_key = cnaes::key_extend(subarray(&state, AES_KEY_SIZE));
    for line in long_state[..init_blocks].as_chunks::<INIT_BLOCKS>().0 {
        for (block, ls) in text.iter_mut().zip(line) {
            *block ^= *ls;
            *block = cnaes::aesb_pseudo_round(*block, &aes_expanded_key);
        }
    }
    set_init(&mut state, &text);

    // Step 5: permute and finalise with one of four hashes.
    hash_permutation(&mut state);

    Ok(extra_hashes(&state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cryptonight::util::hex_to_array;

    #[test]
    fn test_keccak1600() {
        let input: [u8; 44] = hex_to_array(
            "5468697320697320612074657374205468697320697320612074657374205468697320697320612074657374",
        );
        let output = crate::keccak::keccak1600(&input);
        let output_hex = "af6fe96f8cb409bdd2a61fb837e346f1a28007b0f078a8d68bc1224b6fcfcc3c39f1244db8c0af06e94173db4a54038a2f7a6a9c729928b5ec79668a30cbf5f266110665e23e891ea4ee2337fb304b35bf8d9c2e4c3524e52e62db67b0b170487a68a34f8026a81b35dc835c60b356d2c411ad227b6c67e30e9b57ba34b3cf27fccecae972850cf3889bb3ff8347b55a5710d58086973d12d75a3340a39430b65ee2f4be27c21e7b39f47341dd036fe13bf43bb2c55bce498a3adcbf07397ea66062b66d56cd8136";
        assert_eq!(hex::encode(output), output_hex);
    }

    #[test]
    fn test_mul() {
        let test = |a_hex: &str, b_hex: &str, expected_hex: &str| {
            let a = u64::from_le_bytes(hex_to_array(a_hex));
            let b = u64::from_le_bytes(hex_to_array(b_hex));
            let res = mul(a, b);
            assert_eq!(hex::encode(res.to_le_bytes()), expected_hex);
        };
        test(
            "0100000000000000",
            "0100000000000000",
            "00000000000000000100000000000000",
        );
        test(
            "ffffffffffffffff",
            "0200000000000000",
            "0100000000000000feffffffffffffff",
        );
        test(
            "34504affdab54e6d",
            "b352de34917bcc4f",
            "2d82d3509a9912225cbcbe6b16321e17",
        );
        test(
            "26ce23ce804055ed",
            "d8e42f12da72202a",
            "1f531a54b7110e2710c8c956b3f98f90",
        );
    }

    #[test]
    fn test_hash_permutations() {
        const EXPECTED: &str = "31e2fb6eb8e2e376d42a53bc88166378f2a23cf9be54645ff69e8ade3aa4b7ad35040d0e3ad0ee0d8562d53a51acdf14f44de5c097c48a29f63676346194b3af13c3c45af214335a14329491081068a32ea29b3a6856e0efa737dff49d3b5dbf3f7847f058bb41d36347c19d5cd5bdb354ac64a86156c8194e19b0f62d109a8112024a7734730a2bb221c137d3034204e1e57d9cec9689bc199de684f38aeed4624b84c39675a4755ce9b69fde9d36cabd12f1aef4a5b2bb6c6126900799f2109e9b6b55d7bb3ff5";

        let mut state_bytes: [u8; KECCAK1600_BYTE_SIZE] = hex_to_array(
            "af6fe96f8cb409bdd2a61fb837e346f1a28007b0f078a8d68bc1224b6fcfcc3c39f1244db8c0af06e94173db4a54038a2f7a6a9c729928b5ec79668a30cbf5f2622fea9d7982e587e6612c4e6a1d28fdbaba4af1aea99e63322a632d514f35b4fc5cf231e9a6328efb5eb22ad2cfabe571ee8b6ef7dbc64f63185d54a771bdccd207b75e10547b4928f5dcb309192d88bf313d8bc53c8fe71da7ea93355d266c5cc8d39a1273e44b074d143849a3b302edad73c2e61f936c502f6bbabb972b616062b66d56cd8136",
        );
        hash_permutation(&mut state_bytes);
        assert_eq!(hex::encode(state_bytes), EXPECTED);
    }

    #[test]
    fn test_extra_hashes() {
        const EXPECTED: [&str; 4] = [
            "c4d944c2b1c00a8ee627726b35d4cd7fe018de090bc637553cc782e25f974cba",
            "73905cfed57520c60eb468defc58a925170cecc6b4a9f2f6e56d34d674d64111",
            "71a4f8ae96c48df7ace370854824a60a2f247fbf903c7b936f6f99d164c2f6b1",
            "040e79b9daa0fc6219234a06b3889f86f8b02b78dcc25a9874ca95630cf6b5e6",
        ];

        let mut input = [0_u8; KECCAK1600_BYTE_SIZE];
        for (i, val) in input.iter_mut().enumerate() {
            *val = u8::try_from(i & 0xFF).unwrap();
        }

        for (i, expected) in EXPECTED.iter().enumerate() {
            input[0] = u8::try_from(i).unwrap();
            let output = extra_hashes(&input);
            assert_eq!(hex::encode(output), *expected, "hash {i}");
        }
    }

    /// Monero `tests/hash/tests-slow*.txt` vectors (v0, v1, v2) as used upstream by Cuprate.
    #[test]
    fn test_cn_slow_hash_monero_vectors() {
        let test = |input_hex: &str, v0: &str, v1: &str, v2: &str| {
            let input = hex::decode(input_hex).unwrap();
            assert_eq!(
                hex::encode(cn_slow_hash(&input, Variant::V0, &CnParams::CN).unwrap()),
                v0
            );
            assert_eq!(
                hex::encode(cn_slow_hash(&input, Variant::V1, &CnParams::CN).unwrap()),
                v1
            );
            assert_eq!(
                hex::encode(cn_slow_hash(&input, Variant::V2, &CnParams::CN).unwrap()),
                v2
            );
        };
        test(
            "a83cd815319596c6e4fbf2ff9399ce99eb092f58b75c351a7be64a65a118cee031c06a8542b758a15b8a7e",
            "19dff098bf4f330c466480c6bf59cf89c5b4a9692a8941e7625435275e7b12e5",
            "89def5411f4c877e75a0ae9a49c73e63071acedf69e730d9d56425c01ecf0c9a",
            "4c058a38985b6b064d85562f0bd2ebfbaa2915de6ec90307b660b5525ef7d7c3",
        );
        test(
            "0020a84a1806a9cfd957bfd243de587891d6e732322289a9de3db5b15aceacbf1d5b94d04f143bd652bc2d5c6980c2db58007d03b4e5cf94829b06b13ccd61e9edb2ea910876e2eaff62304658a07144236ac3492edfbdee9077f62622d6b9b56173b0eb822391857a33c17e4fed4a54f42f2ea7d85e9246e6879a98c9576240477060ef4f4c88056d08acbd36825b314cfba95082aa66ab6bc89b9831ece3abcc8e7764eb",
            "24b03eefa6fa730cfceb1fe3043dacdff9242f69a3aff86b9d75e08d5ce6903a",
            "b624fcd7345589b2531f29a9d47ab53f4f90caaf3e76f20aa3573d73421e55a2",
            "6426f2a9e3274926b41965f389ec40625f32d1ae6decfff8966d5af208789bf1",
        );
    }

    #[test]
    fn test_variant1_needs_43_bytes() {
        assert_eq!(
            cn_slow_hash(&[0u8; 42], Variant::V1, &CnParams::CN_LITE),
            Err(DataCanNotBeHashed)
        );
        assert!(cn_slow_hash(&[0u8; 42], Variant::V2, &CnParams::CN_TURTLE_LITE).is_ok());
    }

    /// Every variant from the C++ `crypto_test` (`src/crypto_test/main.cpp`), same input.
    #[test]
    fn test_kryptokrona_crypto_test_vectors() {
        const INPUT: &str = "0100fb8e8ac805899323371bb790db19218afd8db8e3755d8b90f39b3d5506a9abce4fa912244500000000ee8146d49fa93ee724deb57d12cbc6c6f3b924d946127c7a97418f9348828f0f02";
        let input = hex::decode(INPUT).unwrap();
        let cases: [(CnParams, Variant, &str); 18] = [
            (
                CnParams::CN,
                Variant::V0,
                "1b606a3f4a07d6489a1bcd07697bd16696b61c8ae982f61a90160f4e52828a7f",
            ),
            (
                CnParams::CN,
                Variant::V1,
                "c9fae8425d8688dc236bcdbc42fdb42d376c6ec190501aa84b04a4b4cf1ee122",
            ),
            (
                CnParams::CN,
                Variant::V2,
                "871fcd6823f6a879bb3f33951c8e8e891d4043880b02dfa1bb3be498b50e7578",
            ),
            (
                CnParams::CN_LITE,
                Variant::V0,
                "28a22bad3f93d1408fca472eb5ad1cbe75f21d053c8ce5b3af105a57713e21dd",
            ),
            (
                CnParams::CN_LITE,
                Variant::V1,
                "87c4e570653eb4c2b42b7a0d546559452dfab573b82ec52f152b7ff98e79446f",
            ),
            (
                CnParams::CN_LITE,
                Variant::V2,
                "b7e78fab22eb19cb8c9c3afe034fb53390321511bab6ab4915cd538a630c3c62",
            ),
            (
                CnParams::CN_DARK,
                Variant::V0,
                "bea42eadd78614f875e55bb972aa5ec54a5edf2dd7068220fda26bf4b1080fb8",
            ),
            (
                CnParams::CN_DARK,
                Variant::V1,
                "d18cb32bd5b465e5a7ba4763d60f88b5792f24e513306f1052954294b737e871",
            ),
            (
                CnParams::CN_DARK,
                Variant::V2,
                "a18a14d94efea108757a42633a1b4d4dc11838084c3c4347850d39ab5211a91f",
            ),
            (
                CnParams::CN_DARK_LITE,
                Variant::V0,
                "faa7884d9c08126eb164814aeba6547b5d6064277a09fb6b414f5dbc9d01eb2b",
            ),
            (
                CnParams::CN_DARK_LITE,
                Variant::V1,
                "c75c010780fffd9d5e99838eb093b37c0dd015101c9d298217866daa2993d277",
            ),
            (
                CnParams::CN_DARK_LITE,
                Variant::V2,
                "fdceb794c1055977a955f31c576a8be528a0356ee1b0a1f9b7f09e20185cda28",
            ),
            (
                CnParams::CN_TURTLE,
                Variant::V0,
                "546c3f1badd7c1232c7a3b88cdb013f7f611b7bd3d1d2463540fccbd12997982",
            ),
            (
                CnParams::CN_TURTLE,
                Variant::V1,
                "29e7831780a0ab930e0fe3b965f30e8a44d9b3f9ad2241d67cfbfea3ed62a64e",
            ),
            (
                CnParams::CN_TURTLE,
                Variant::V2,
                "fc67dfccb5fc90d7855ae903361eabd76f1e40a22a72ad3ef2d6ad27b5a60ce5",
            ),
            (
                CnParams::CN_TURTLE_LITE,
                Variant::V0,
                "5e1891a15d5d85c09baf4a3bbe33675cfa3f77229c8ad66c01779e590528d6d3",
            ),
            (
                CnParams::CN_TURTLE_LITE,
                Variant::V1,
                "ae7f864a7a2f2b07dcef253581e60a014972b9655a152341cb989164761c180a",
            ),
            (
                CnParams::CN_TURTLE_LITE,
                Variant::V2,
                "b2172ec9466e1aee70ec8572a14c233ee354582bcb93f869d429744de5726a26",
            ),
        ];
        for (params, variant, expected) in cases {
            let got = hex::encode(cn_slow_hash(&input, variant, &params).unwrap());
            assert_eq!(got, expected, "{params:?} {variant:?}");
        }
    }
}
