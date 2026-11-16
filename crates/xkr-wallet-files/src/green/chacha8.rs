//! `ChaCha` with 8 rounds, a 256-bit key, a 64-bit nonce and a 64-bit block counter (D. J.
//! Bernstein's original layout), as `src/crypto/chacha8.cpp` `crypto::chacha8`
//! ("chacha-merged.c version 20080118"), and the container key of
//! `src/crypto/chacha8.h` `generate_chacha8_key`.
//!
//! State words (all little-endian): 0..4 the constant "expand 32-byte k", 4..12 the key,
//! 12..14 the block counter starting at 0 (word 12 low, carried into word 13), 14..16 the IV.

use zeroize::{Zeroize as _, Zeroizing};

/// C++ `CHACHA8_KEY_SIZE`.
pub const KEY_SIZE: usize = 32;
/// C++ `CHACHA8_IV_SIZE`.
pub const IV_SIZE: usize = 8;

const SIGMA: &[u8; 16] = b"expand 32-byte k";

/// XORs `data` in place with the `ChaCha8` key stream for `key` and `iv` (encryption and
/// decryption are the same operation). C++ `crypto::chacha8`.
pub fn chacha8_xor(data: &mut [u8], key: &[u8; KEY_SIZE], iv: &[u8; IV_SIZE]) {
    chacha_xor(data, key, *iv, 8, 0);
}

/// [`chacha8_xor`] into a new buffer.
#[must_use]
pub fn chacha8(data: &[u8], key: &[u8; KEY_SIZE], iv: &[u8; IV_SIZE]) -> Vec<u8> {
    let mut out = data.to_vec();
    chacha8_xor(&mut out, key, iv);
    out
}

/// C++ `crypto::generate_chacha8_key`: the first 32 bytes of `cn_slow_hash_v0(password)`
/// (the whole hash, as it is 32 bytes).
#[must_use]
pub fn generate_chacha8_key(password: &str) -> Zeroizing<[u8; KEY_SIZE]> {
    let mut hash = xkr_crypto::cn_slow_hash_v0(password.as_bytes());
    let key = Zeroizing::new(hash.0);
    hash.0.zeroize();
    key
}

/// The `ChaCha` stream cipher with `rounds` rounds (8 for the wallet; 20 only for the tests
/// against published `ChaCha20` vectors of the same layout), starting at block `counter`
/// (0 for the wallet).
fn chacha_xor(
    data: &mut [u8],
    key: &[u8; KEY_SIZE],
    iv: [u8; IV_SIZE],
    rounds: usize,
    counter: u64,
) {
    let mut state = [0u32; 16];
    for (i, word) in state[..4].iter_mut().enumerate() {
        *word = le32(&SIGMA[4 * i..]);
    }
    for (i, word) in state[4..12].iter_mut().enumerate() {
        *word = le32(&key[4 * i..]);
    }
    state[12] = counter as u32;
    state[13] = (counter >> 32) as u32;
    state[14] = le32(&iv[..4]);
    state[15] = le32(&iv[4..]);

    let mut block = [0u8; 64];
    for chunk in data.chunks_mut(64) {
        key_block(&state, rounds, &mut block);
        for (d, k) in chunk.iter_mut().zip(block.iter()) {
            *d ^= *k;
        }
        // C++: j12 = PLUSONE(j12); if (!j12) j13 = PLUSONE(j13);
        state[12] = state[12].wrapping_add(1);
        if state[12] == 0 {
            state[13] = state[13].wrapping_add(1);
        }
    }
    block.zeroize();
    state.zeroize();
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[inline]
fn quarter_round(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

/// One 64-byte key-stream block: `rounds / 2` double rounds (column then diagonal), plus the
/// input state, little-endian.
fn key_block(state: &[u32; 16], rounds: usize, out: &mut [u8; 64]) {
    let mut x = *state;
    for _ in 0..rounds / 2 {
        quarter_round(&mut x, 0, 4, 8, 12);
        quarter_round(&mut x, 1, 5, 9, 13);
        quarter_round(&mut x, 2, 6, 10, 14);
        quarter_round(&mut x, 3, 7, 11, 15);
        quarter_round(&mut x, 0, 5, 10, 15);
        quarter_round(&mut x, 1, 6, 11, 12);
        quarter_round(&mut x, 2, 7, 8, 13);
        quarter_round(&mut x, 3, 4, 9, 14);
    }
    for (i, word) in x.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.wrapping_add(state[i]).to_le_bytes());
    }
    x.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// `ChaCha8` known answers. Source: `external/cryptopp/TestVectors/chacha.txt` in the C++
    /// tree (Crypto++'s copy of draft-strombergson-chacha-test-vectors, `Name: ChaCha8`,
    /// 32-byte key cases TC1-TC3, 64 zero bytes of plaintext).
    ///
    /// Why these vectors apply to `crypto::chacha8`: reading `src/crypto/chacha8.cpp`, the
    /// state is `sigma` ("expand 32-byte k") in words 0-3, the 32-byte key in words 4-11, a
    /// counter starting at 0 in words 12-13 and the 8-byte IV in words 14-15, all little
    /// endian; the loop `for (i = 8; i > 0; i -= 2)` runs four column+diagonal double rounds;
    /// the input state is added back and the result `XORed` with the data. That is exactly the
    /// `eSTREAM`/DJB `ChaCha8` that the draft's vectors are for (64-bit nonce, 64-bit counter,
    /// 32-byte key), so the C++ function produces these bytes for these inputs.
    #[test]
    fn chacha8_known_answers() {
        let cases = [
            // TC1: all-zero key and IV
            (
                [0u8; 32],
                [0u8; 8],
                "3e00ef2f895f40d67f5bb8e81f09a5a12c840ec3ce9a7f3b181be188ef711a1e\
                 984ce172b9216f419f445367456d5619314a42a3da86b001387bfdb80e0cfe42",
            ),
            // TC2: key byte 0 = 0x01
            (
                {
                    let mut k = [0u8; 32];
                    k[0] = 1;
                    k
                },
                [0u8; 8],
                "cf5ee9a0494aa9613e05d5ed725b804b12f4a465ee635acc3a311de8740489ea\
                 289d04f43c7518db56eb4433e498a1238cd8464d3763ddbb9222ee3bd8fae3c8",
            ),
            // TC3: IV byte 0 = 0x01
            (
                [0u8; 32],
                [1, 0, 0, 0, 0, 0, 0, 0],
                "2b8f4bb3798306ca5130d47c4f8d4ed13aa0edccc1be6942090faeeca0d7599b\
                 7ff0fe616bb25aa0153ad6fdc88b954903c22426d478b97b22b8f9b1db00cf06",
            ),
        ];
        for (key, iv, expected) in cases {
            assert_eq!(chacha8(&[0u8; 64], &key, &iv), h(expected));
        }
    }

    /// The same core with 20 rounds against two consecutive `ChaCha20` blocks (zero key, zero
    /// nonce, DJB layout): the first block is `Name: ChaCha20` TC1 of the same Crypto++ file,
    /// both blocks are `test_chacha_true_values_a` of the `chacha20` crate's `tests/rng.rs`
    /// (draft-nir-cfrg-chacha20-poly1305-04 vectors 1 and 2). This checks the counter step
    /// between blocks and a partial last block.
    #[test]
    fn chacha20_two_blocks_and_partial_tail() {
        let words: [u32; 32] = [
            0xade0b876, 0x903df1a0, 0xe56a5d40, 0x28bd8653, 0xb819d2bd, 0x1aed8da0, 0xccef36a8,
            0xc70d778b, 0x7c5941da, 0x8d485751, 0x3fe02477, 0x374ad8b8, 0xf4b8436a, 0x1ca11815,
            0x69b687c3, 0x8665eeb2, 0xbee7079f, 0x7a385155, 0x7c97ba98, 0x0d082d73, 0xa0290fcb,
            0x6965e348, 0x3e53c612, 0xed7aee32, 0x7621b729, 0x434ee69c, 0xb03371d5, 0xd539d874,
            0x281fed31, 0x45fb0a51, 0x1f0ae1ac, 0x6f4d794b,
        ];
        let expected: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(
            hex::encode(&expected[..64]),
            "76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
             da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586"
        );
        let mut buf = [0u8; 128];
        chacha_xor(&mut buf, &[0u8; 32], [0u8; 8], 20, 0);
        assert_eq!(buf.to_vec(), expected);
        let mut tail = [0u8; 100];
        chacha_xor(&mut tail, &[0u8; 32], [0u8; 8], 20, 0);
        assert_eq!(tail.to_vec(), expected[..100].to_vec());
    }

    #[test]
    fn xor_is_its_own_inverse_and_empty_is_noop() {
        let key = [9u8; 32];
        let iv = [4u8; 8];
        let data: Vec<u8> = (0..200u8).collect();
        let enc = chacha8(&data, &key, &iv);
        assert_ne!(enc, data);
        assert_eq!(chacha8(&enc, &key, &iv), data);
        assert_eq!(chacha8(&[], &key, &iv), Vec::<u8>::new());
    }

    #[test]
    fn counter_carries_into_the_high_word() {
        // Two blocks from counter 2^32 - 1: the second must be block 2^32, i.e. word 12 wraps
        // to 0 and word 13 becomes 1, as in C++ (`if (!j12) j13 = PLUSONE(j13)`).
        let key = [1u8; 32];
        let iv = [2u8; 8];
        let mut across = [0u8; 128];
        chacha_xor(&mut across, &key, iv, 8, u64::from(u32::MAX));
        let mut high = [0u8; 64];
        chacha_xor(&mut high, &key, iv, 8, 1u64 << 32);
        assert_eq!(&across[64..], &high[..]);
        let mut first = [0u8; 64];
        chacha_xor(&mut first, &key, iv, 8, 0);
        assert_ne!(first, high);
    }

    #[test]
    fn password_key_is_slow_hash() {
        // CryptoNight v0 of "de omnibus dubitandum": first line of Monero
        // `tests/hash/tests-slow.txt`.
        let key = generate_chacha8_key("de omnibus dubitandum");
        assert_eq!(
            hex::encode(*key),
            "2f8e3df40bd11f9ac90c743ca8e32bb391da4fb98612aa3b6cdc639ee00b31f5"
        );
        assert_eq!(
            *generate_chacha8_key(""),
            xkr_crypto::cn_slow_hash_v0(b"").0
        );
    }
}
