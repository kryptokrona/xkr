//! Keccak (original submission padding `0x01`, **not** SHA-3's `0x06`), as used by CryptoNote.
//!
//! Mirrors `src/crypto/keccak.c` (`keccakf`, `keccak`, `keccak1600`) and `src/crypto/hash.c`
//! (`cn_fast_hash`, `hash_permutation`).

use xkr_types::Hash;

const KECCAKF_RNDC: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

const KECCAKF_ROTC: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];

const KECCAKF_PILN: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

/// Rate in bytes for a 256-bit output (`HASH_DATA_AREA` in `keccak.c`).
pub const HASH_DATA_AREA: usize = 136;

/// Keccak-f\[1600\] with 24 rounds. C++ `keccakf(st, 24)`.
pub fn keccakf(st: &mut [u64; 25]) {
    for rndc in KECCAKF_RNDC {
        // Theta
        let mut bc = [0u64; 5];
        for (i, b) in bc.iter_mut().enumerate() {
            *b = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        }
        for i in 0..5 {
            let t = bc[(i + 4) % 5] ^ bc[(i + 1) % 5].rotate_left(1);
            for j in (0..25).step_by(5) {
                st[j + i] ^= t;
            }
        }

        // Rho Pi
        let mut t = st[1];
        for (piln, rotc) in KECCAKF_PILN.iter().zip(KECCAKF_ROTC) {
            let tmp = st[*piln];
            st[*piln] = t.rotate_left(rotc);
            t = tmp;
        }

        // Chi
        for j in (0..25).step_by(5) {
            let mut row = [0u64; 5];
            row.copy_from_slice(&st[j..j + 5]);
            for i in 0..5 {
                st[j + i] ^= (!row[(i + 1) % 5]) & row[(i + 2) % 5];
            }
        }

        // Iota
        st[0] ^= rndc;
    }
}

fn absorb_block(st: &mut [u64; 25], block: &[u8]) {
    for (lane, chunk) in st.iter_mut().zip(block.as_chunks::<8>().0) {
        *lane ^= u64::from_le_bytes(*chunk);
    }
}

/// C++ `keccak1600`: absorbs `data` at rate 136 with `0x01 ... 0x80` padding and returns the
/// full 200-byte state (CryptoNight's `hash_process`).
#[must_use]
pub fn keccak1600(data: &[u8]) -> [u8; 200] {
    let mut st = [0u64; 25];
    let (blocks, rem) = data.as_chunks::<HASH_DATA_AREA>();
    for block in blocks {
        absorb_block(&mut st, block);
        keccakf(&mut st);
    }
    let mut temp = [0u8; HASH_DATA_AREA];
    temp[..rem.len()].copy_from_slice(rem);
    temp[rem.len()] = 1;
    temp[HASH_DATA_AREA - 1] |= 0x80;
    absorb_block(&mut st, &temp);
    keccakf(&mut st);

    let mut out = [0u8; 200];
    for (dst, lane) in out.as_chunks_mut::<8>().0.iter_mut().zip(st.iter()) {
        *dst = lane.to_le_bytes();
    }
    out
}

/// C++ `cn_fast_hash`: Keccak-256 (original padding) = first 32 bytes of [`keccak1600`].
#[must_use]
pub fn cn_fast_hash(data: &[u8]) -> Hash {
    let state = keccak1600(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&state[..32]);
    Hash(out)
}

/// [`cn_fast_hash`] over data fed in pieces: `update(a); update(b); finalize()` equals
/// `cn_fast_hash(a || b)`, without holding the whole input in memory.
#[derive(Debug, Clone)]
pub struct FastHasher {
    st: [u64; 25],
    buf: [u8; HASH_DATA_AREA],
    len: usize,
}

impl Default for FastHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl FastHasher {
    /// An empty hasher.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            st: [0; 25],
            buf: [0; HASH_DATA_AREA],
            len: 0,
        }
    }

    /// Absorbs `data`.
    pub fn update(&mut self, mut data: &[u8]) {
        if self.len > 0 {
            let n = (HASH_DATA_AREA - self.len).min(data.len());
            self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
            self.len += n;
            data = &data[n..];
            if self.len < HASH_DATA_AREA {
                return;
            }
            absorb_block(&mut self.st, &self.buf);
            keccakf(&mut self.st);
            self.len = 0;
        }
        let (blocks, rem) = data.as_chunks::<HASH_DATA_AREA>();
        for block in blocks {
            absorb_block(&mut self.st, block);
            keccakf(&mut self.st);
        }
        self.buf[..rem.len()].copy_from_slice(rem);
        self.len = rem.len();
    }

    /// The hash of everything absorbed.
    #[must_use]
    pub fn finalize(mut self) -> Hash {
        let mut temp = [0u8; HASH_DATA_AREA];
        temp[..self.len].copy_from_slice(&self.buf[..self.len]);
        temp[self.len] = 1;
        temp[HASH_DATA_AREA - 1] |= 0x80;
        absorb_block(&mut self.st, &temp);
        keccakf(&mut self.st);
        let mut out = [0u8; 32];
        for (dst, lane) in out.as_chunks_mut::<8>().0.iter_mut().zip(self.st.iter()) {
            *dst = lane.to_le_bytes();
        }
        Hash(out)
    }
}

/// `cn_fast_hash(a || b)` without allocating; the tree-hash node function.
#[must_use]
pub fn cn_fast_hash_pair(a: &Hash, b: &Hash) -> Hash {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&a.0);
    buf[32..].copy_from_slice(&b.0);
    cn_fast_hash(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_keccak256() {
        assert_eq!(
            cn_fast_hash(b"").to_string(),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn crypto_test_vector() {
        // src/crypto_test/main.cpp: CN_FAST_HASH of INPUT_DATA.
        let input = hex::decode("0100fb8e8ac805899323371bb790db19218afd8db8e3755d8b90f39b3d5506a9abce4fa912244500000000ee8146d49fa93ee724deb57d12cbc6c6f3b924d946127c7a97418f9348828f0f02").unwrap();
        assert_eq!(
            cn_fast_hash(&input).to_string(),
            "b542df5b6e7f5f05275c98e7345884e2ac726aeeb07e03e44e0389eb86cd05f0"
        );
    }

    #[test]
    fn fast_hasher_matches_cn_fast_hash_for_any_split() {
        let data: Vec<u8> = (0..700usize).map(|i| (i * 13 + 5) as u8).collect();
        for len in [0usize, 1, 135, 136, 137, 272, 300, 700] {
            let whole = cn_fast_hash(&data[..len]);
            for cut in [0usize, 1, 7, 135, 136, 137, 200] {
                let cut = cut.min(len);
                let mut h = FastHasher::new();
                h.update(&data[..cut]);
                h.update(&[]);
                for piece in data[cut..len].chunks(61) {
                    h.update(piece);
                }
                assert_eq!(h.finalize(), whole, "len {len}, cut {cut}");
            }
        }
    }

    #[test]
    fn matches_tiny_keccak_on_all_lengths_around_the_rate() {
        use tiny_keccak::{Hasher, Keccak};
        for len in [0usize, 1, 31, 32, 135, 136, 137, 271, 272, 273, 1000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let mut k = Keccak::v256();
            k.update(&data);
            let mut expected = [0u8; 32];
            k.finalize(&mut expected);
            assert_eq!(cn_fast_hash(&data).0, expected, "len {len}");
        }
    }
}
