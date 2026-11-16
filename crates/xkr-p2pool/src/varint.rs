//! Varints exactly as the C++ p2pool reads and writes them (`readVarint` / `writeVarint`,
//! `src/util.h`).
//!
//! Reading accepts the canonical LEB128 form only: a zero byte after the first one, bits beyond
//! the target type and more bytes than the type has room for are refused.

/// Appends `value` as LEB128 (C++ `writeVarint`).
pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Bytes of the LEB128 form of `value`.
#[must_use]
pub const fn varint_len(mut value: u64) -> usize {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// C++ `readVarint<T>` for an unsigned type of `bits` bits (32 or 64) from `data[*pos..]`.
/// Advances `pos` past the varint; `None` (and `pos` unspecified) when it is malformed or
/// truncated.
#[must_use]
pub fn read_varint(data: &[u8], pos: &mut usize, bits: u32) -> Option<u64> {
    let mut result = 0u64;
    let mut k = 0u32;
    while let Some(&byte) = data.get(*pos) {
        if k >= bits {
            return None;
        }
        *pos += 1;
        if k > 0 && byte == 0 {
            return None;
        }
        let payload = u64::from(byte & 0x7f);
        // Bits shifted out of the 64-bit result.
        if k > 0 && (u128::from(payload) << k) >> 64 != 0 {
            return None;
        }
        result |= payload << k;
        k += 7;
        if byte & 0x80 == 0 {
            if bits < 64 && result >> bits != 0 {
                return None;
            }
            return Some(result);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8], bits: u32) -> Option<(u64, usize)> {
        let mut pos = 0;
        read_varint(bytes, &mut pos, bits).map(|v| (v, pos))
    }

    #[test]
    fn round_trip_and_lengths() {
        for v in [
            0u64,
            1,
            0x7f,
            0x80,
            300,
            16_383,
            16_384,
            28_524,
            150_802_240,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut out = Vec::new();
            write_varint(&mut out, v);
            assert_eq!(out.len(), varint_len(v));
            assert_eq!(out, xkr_serialization::varint::varint_bytes(v));
            assert_eq!(read(&out, 64), Some((v, out.len())));
        }
        assert_eq!(read(&[0xec, 0xde, 0x01, 0xff], 64), Some((28_524, 3)));
    }

    #[test]
    fn refuses_what_cpp_refuses() {
        // Zero continuation byte (non-canonical 0 / 1).
        assert_eq!(read(&[0x80, 0x00], 64), None);
        assert_eq!(read(&[0x81, 0x00], 64), None);
        // Truncated.
        assert_eq!(read(&[0x80], 64), None);
        assert_eq!(read(&[], 64), None);
        // 2^64 needs a tenth byte with payload 2: refused.
        assert_eq!(
            read(
                &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02],
                64
            ),
            None
        );
        // u64::MAX is the largest accepted.
        assert_eq!(
            read(
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
                64
            ),
            Some((u64::MAX, 10))
        );
        // Eleven bytes.
        assert_eq!(
            read(
                &[
                    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x81, 0x01
                ],
                64
            ),
            None
        );
        // 32-bit target: 2^32 does not fit, six bytes are too many.
        assert_eq!(read(&[0x80, 0x80, 0x80, 0x80, 0x10], 32), None);
        assert_eq!(
            read(&[0xff, 0xff, 0xff, 0xff, 0x0f], 32),
            Some((u64::from(u32::MAX), 5))
        );
        assert_eq!(read(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01], 32), None);
    }
}
