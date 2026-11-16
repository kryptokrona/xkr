//! CryptoNote base58 (`src/common/base58.cpp`).
//!
//! Not Bitcoin base58: the input is cut into 8-byte blocks, each encoded big-endian into a
//! fixed 11 characters (a shorter last block into 2..10 characters, see
//! [`ENCODED_BLOCK_SIZES`]), left-padded with `'1'`. Addresses are
//! `base58(varint(prefix) || data || keccak(varint(prefix) || data)[..4])`.

use xkr_crypto::cn_fast_hash;
use xkr_serialization::varint::{read_varint, write_varint};

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
/// Encoded length of a block of `i` bytes (`encoded_block_sizes`).
pub const ENCODED_BLOCK_SIZES: [usize; 9] = [0, 2, 3, 5, 6, 7, 9, 10, 11];
const FULL_BLOCK_SIZE: usize = 8;
const FULL_ENCODED_BLOCK_SIZE: usize = 11;
/// `addr_checksum_size`.
pub const ADDR_CHECKSUM_SIZE: usize = 4;

fn digit(c: u8) -> Option<u64> {
    ALPHABET.iter().position(|a| *a == c).map(|p| p as u64)
}

/// Decoded length of an encoded block of `n` characters (`decoded_block_sizes`).
fn decoded_block_size(n: usize) -> Option<usize> {
    ENCODED_BLOCK_SIZES.iter().position(|s| *s == n)
}

fn encode_block(block: &[u8], out: &mut [u8]) {
    let mut num = block.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    let mut i = out.len();
    while num > 0 && i > 0 {
        i -= 1;
        out[i] = ALPHABET[(num % 58) as usize];
        num /= 58;
    }
}

fn decode_block(block: &[u8], out: &mut [u8]) -> Option<()> {
    let res_size = decoded_block_size(block.len())?;
    if res_size == 0 || res_size != out.len() {
        return None;
    }
    let mut res: u64 = 0;
    let mut order: u64 = 1;
    for c in block.iter().rev() {
        let product = u128::from(order) * u128::from(digit(*c)?);
        let product = u64::try_from(product).ok()?;
        res = res.checked_add(product)?;
        // 58^10 < 2^64; the product after the last digit is never used.
        order = order.wrapping_mul(58);
    }
    if res_size < FULL_BLOCK_SIZE && (1u64 << (8 * res_size)) <= res {
        return None;
    }
    out.copy_from_slice(&res.to_be_bytes()[FULL_BLOCK_SIZE - res_size..]);
    Some(())
}

/// `tools::base58::encode`.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(
        data.len() / FULL_BLOCK_SIZE * FULL_ENCODED_BLOCK_SIZE + FULL_ENCODED_BLOCK_SIZE,
    );
    for chunk in data.chunks(FULL_BLOCK_SIZE) {
        let start = out.len();
        out.resize(start + ENCODED_BLOCK_SIZES[chunk.len()], ALPHABET[0]);
        encode_block(chunk, &mut out[start..]);
    }
    // Only alphabet characters were written.
    out.into_iter().map(char::from).collect()
}

/// `tools::base58::decode`; `None` for an invalid length, character or overflowing block.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let enc = text.as_bytes();
    let full = enc.len() / FULL_ENCODED_BLOCK_SIZE;
    let last = enc.len() % FULL_ENCODED_BLOCK_SIZE;
    let last_decoded = decoded_block_size(last)?;
    let mut out = vec![0u8; full * FULL_BLOCK_SIZE + last_decoded];
    for i in 0..full {
        decode_block(
            &enc[i * FULL_ENCODED_BLOCK_SIZE..(i + 1) * FULL_ENCODED_BLOCK_SIZE],
            &mut out[i * FULL_BLOCK_SIZE..(i + 1) * FULL_BLOCK_SIZE],
        )?;
    }
    if last > 0 {
        decode_block(
            &enc[full * FULL_ENCODED_BLOCK_SIZE..],
            &mut out[full * FULL_BLOCK_SIZE..],
        )?;
    }
    Some(out)
}

/// `tools::base58::encode_addr(tag, data)`.
#[must_use]
pub fn encode_addr(tag: u64, data: &[u8]) -> String {
    let mut buf = Vec::with_capacity(data.len() + 16);
    write_varint(&mut buf, tag);
    buf.extend_from_slice(data);
    let checksum = cn_fast_hash(&buf);
    buf.extend_from_slice(&checksum.0[..ADDR_CHECKSUM_SIZE]);
    encode(&buf)
}

/// `tools::base58::decode_addr`: `(tag, data)` if the checksum matches.
#[must_use]
pub fn decode_addr(text: &str) -> Option<(u64, Vec<u8>)> {
    let raw = decode(text)?;
    if raw.len() <= ADDR_CHECKSUM_SIZE {
        return None;
    }
    let (body, checksum) = raw.split_at(raw.len() - ADDR_CHECKSUM_SIZE);
    if cn_fast_hash(body).0[..ADDR_CHECKSUM_SIZE] != *checksum {
        return None;
    }
    let mut pos = 0usize;
    let tag = read_varint(body, &mut pos, 64).ok()?;
    Some((tag, body[pos..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_round_trips() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(200))
                .collect();
            let enc = encode(&data);
            assert_eq!(enc.len(), len / 8 * 11 + ENCODED_BLOCK_SIZES[len % 8]);
            assert_eq!(decode(&enc).as_deref(), Some(data.as_slice()), "len {len}");
        }
        assert_eq!(encode(&[0u8; 8]), "11111111111");
        assert_eq!(decode("11111111111"), Some(vec![0u8; 8]));
        // invalid lengths and characters
        assert_eq!(decode("1"), None);
        assert_eq!(decode("1111"), None);
        assert_eq!(decode("0O"), None);
        // overflow of a 1-byte block: "5R" = 4 * 58 + 24 = 256 does not fit
        assert_eq!(decode("5R"), None);
    }

    #[test]
    fn address_checksum() {
        let enc = encode_addr(2_239_254, &[7u8; 64]);
        assert!(enc.starts_with("SEKR"));
        assert_eq!(enc.len(), 99);
        let (tag, data) = decode_addr(&enc).unwrap();
        assert_eq!(tag, 2_239_254);
        assert_eq!(data, vec![7u8; 64]);
        let mut broken = enc.into_bytes();
        let last = broken.len() - 1;
        broken[last] = if broken[last] == b'z' { b'y' } else { b'z' };
        assert!(decode_addr(std::str::from_utf8(&broken).unwrap()).is_none());
    }
}
