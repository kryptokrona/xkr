//! CryptoNote varints: 7 bits per byte, little-endian groups, high bit = continuation.
//!
//! [`write_varint`] is C++ `writeVarint` (`common/stream_tools.cpp`) / `tools::write_varint`
//! (`common/varint.h`). [`read_varint`] is C++ `readVarint(IInputStream&, T&)` for a `T` of
//! `bits` bits: it rejects a group that would overflow `T` and a non-canonical encoding whose
//! last group is zero.

/// Why a varint could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarintError {
    /// Input ended inside the varint.
    Eof,
    /// Value does not fit the target width.
    Overflow,
    /// Last group is zero but not the first (`0x80 0x00` etc.).
    NonCanonical,
}

/// Appends `value` in varint form.
pub fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Varint encoding of `value` as a new vector (C++ `tools::get_varint_data`).
#[must_use]
pub fn varint_bytes(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    write_varint(&mut out, value);
    out
}

/// Reads a varint of a `bits`-bit unsigned type (8, 16, 32 or 64) at `*pos`, advancing `*pos`.
/// On error `*pos` is left wherever reading stopped.
pub fn read_varint(data: &[u8], pos: &mut usize, bits: u32) -> Result<u64, VarintError> {
    debug_assert!(matches!(bits, 8 | 16 | 32 | 64));
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let piece = *data.get(*pos).ok_or(VarintError::Eof)?;
        *pos += 1;
        // C++: if (shift >= sizeof(T) * 8 - 7 && piece >= 1 << (sizeof(T) * 8 - shift)) overflow
        if shift + 7 >= bits && u32::from(piece) >= (1u32 << (bits - shift)) {
            return Err(VarintError::Overflow);
        }
        value |= u64::from(piece & 0x7f) << shift;
        if piece & 0x80 == 0 {
            if piece == 0 && shift != 0 {
                return Err(VarintError::NonCanonical);
            }
            return Ok(value);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(v: u64) {
        let b = varint_bytes(v);
        let mut pos = 0;
        assert_eq!(read_varint(&b, &mut pos, 64), Ok(v));
        assert_eq!(pos, b.len());
    }

    #[test]
    fn roundtrip() {
        for v in [
            0,
            1,
            0x7f,
            0x80,
            300,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            rt(v);
        }
        assert_eq!(varint_bytes(300), vec![0xac, 0x02]);
        assert_eq!(varint_bytes(u64::MAX).len(), 10);
    }

    #[test]
    fn width_limits_match_cpp() {
        let mut p = 0;
        assert_eq!(read_varint(&[0xff, 0x01], &mut p, 8), Ok(255));
        p = 0;
        assert_eq!(
            read_varint(&[0x80, 0x02], &mut p, 8),
            Err(VarintError::Overflow)
        );
        p = 0;
        assert_eq!(read_varint(&[0xff, 0xff, 0x03], &mut p, 16), Ok(65_535));
        p = 0;
        assert_eq!(
            read_varint(&[0xff, 0xff, 0x04], &mut p, 16),
            Err(VarintError::Overflow)
        );
        p = 0;
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x0f], &mut p, 32),
            Ok(u64::from(u32::MAX))
        );
        p = 0;
        assert_eq!(
            read_varint(&[0xff, 0xff, 0xff, 0xff, 0x10], &mut p, 32),
            Err(VarintError::Overflow)
        );
        let mut max = vec![0xff; 9];
        max.push(0x01);
        p = 0;
        assert_eq!(read_varint(&max, &mut p, 64), Ok(u64::MAX));
        max[9] = 0x02;
        p = 0;
        assert_eq!(read_varint(&max, &mut p, 64), Err(VarintError::Overflow));
    }

    #[test]
    fn non_canonical_and_eof() {
        let mut p = 0;
        assert_eq!(
            read_varint(&[0x80, 0x00], &mut p, 64),
            Err(VarintError::NonCanonical)
        );
        p = 0;
        assert_eq!(read_varint(&[0x00], &mut p, 64), Ok(0));
        p = 0;
        assert_eq!(read_varint(&[0x80], &mut p, 64), Err(VarintError::Eof));
        p = 0;
        assert_eq!(read_varint(&[], &mut p, 64), Err(VarintError::Eof));
    }
}
