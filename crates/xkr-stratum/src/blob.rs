//! Job blob helpers (hugin-utils `getNonceOffsetFromBuffer`, `extractPrevIdFromBlob`,
//! `insertNonce`).
//!
//! A stratum job blob is the proof-of-work hashing blob of a block template: the three header
//! varints (major version, minor version, timestamp), the 32-byte `prev_id`, the 4-byte nonce and
//! the rest (C++ `CachedBlock::getBlockLongHash` input, `src/cryptonote_core/cached_block.cpp`).
//! These helpers find the fields without decoding the whole blob, as pools and miners do.

/// hugin-utils' fallback nonce offset (major, minor, 5-byte timestamp, `prev_id`).
pub const DEFAULT_NONCE_OFFSET: usize = 39;

/// `/^[0-9a-f]+$/i` (non-empty).
#[must_use]
pub fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Length of the varint at `off` (hugin-utils `readVarint`: at most 10 bytes).
fn varint_len(buf: &[u8], off: usize) -> Option<usize> {
    let mut shift = 0u32;
    let mut n = 0usize;
    loop {
        let byte = *buf.get(off + n)?;
        n += 1;
        if byte & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Offset of the first byte after `major | minor | timestamp` (varints), if parseable.
#[must_use]
pub fn header_len(blob: &[u8]) -> Option<usize> {
    let mut off = 0;
    for _ in 0..3 {
        off += varint_len(blob, off)?;
    }
    Some(off)
}

/// hugin-utils `getNonceOffsetFromBuffer`: after the three header varints and the 32-byte
/// `prev_id`, or [`DEFAULT_NONCE_OFFSET`] if the header does not parse.
#[must_use]
pub fn nonce_offset(blob: &[u8]) -> usize {
    header_len(blob).map_or(DEFAULT_NONCE_OFFSET, |o| o + 32)
}

/// hugin-utils `extractPrevIdFromBlob`.
#[must_use]
pub fn extract_prev_id(blob: &[u8]) -> Option<[u8; 32]> {
    let off = header_len(blob)?;
    blob.get(off..off + 32)?.try_into().ok()
}

/// hugin-utils `insertNonce`; `None` where JavaScript would throw (nonce past the end).
#[must_use]
pub fn insert_nonce(blob: &[u8], nonce: &[u8; 4]) -> Option<Vec<u8>> {
    let off = nonce_offset(blob);
    if off + 4 > blob.len() {
        return None;
    }
    let mut out = blob.to_vec();
    out[off..off + 4].copy_from_slice(nonce);
    Some(out)
}

/// The block major version a job blob starts with (its first varint), which selects the
/// proof-of-work algorithm (C++ `CachedBlock::getBlockLongHash`). `None` for an empty blob or
/// a version that does not fit in a byte.
#[must_use]
pub fn major_version(blob: &[u8]) -> Option<u8> {
    let first = *blob.first()?;
    (first & 0x80 == 0).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_of_a_blob() {
        // major 7, minor 0, timestamp 0x80 0x01 (two bytes), prev_id, nonce
        let mut blob = vec![7, 0, 0x80, 0x01];
        blob.extend_from_slice(&[9; 32]);
        blob.extend_from_slice(&[0; 4]);
        blob.push(0x42);
        assert_eq!(header_len(&blob), Some(4));
        assert_eq!(nonce_offset(&blob), 36);
        assert_eq!(extract_prev_id(&blob), Some([9; 32]));
        let patched = insert_nonce(&blob, &[1, 2, 3, 4]).unwrap();
        assert_eq!(&patched[36..40], &[1, 2, 3, 4]);
        assert_eq!(major_version(&blob), Some(7));
        assert_eq!(major_version(&[0x81, 1]), None);
        assert_eq!(major_version(&[]), None);
        assert_eq!(nonce_offset(&[0x80; 20]), DEFAULT_NONCE_OFFSET);
        assert_eq!(insert_nonce(&blob[..30], &[0; 4]), None);
        assert!(is_hex("00aF") && !is_hex("") && !is_hex("0g"));
    }
}
