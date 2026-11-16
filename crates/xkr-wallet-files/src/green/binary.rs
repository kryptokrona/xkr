//! The parts of the CryptoNote binary serializer that the container uses.
//!
//! Mirrors `src/serialization/binary_input_stream_serializer.cpp` and
//! `binary_output_stream_serializer.cpp`:
//! * every integer is a varint (`readVarint<T>` with the overflow and canonical-form checks;
//!   signed values are the varint of their two's-complement `u64`);
//! * `bool` is one byte, non-zero = true;
//! * a `std::string` is a varint length and the bytes;
//! * a POD (`PublicKey`, `Hash`, `chacha8_iv`) is its raw bytes (`serializePod`);
//! * a `std::vector<uint8_t>` (`BinaryArray`) goes through `serializeContainer`: a varint
//!   count, then every byte as a `uint8_t` varint (bytes from 0x80 take two bytes).

use xkr_serialization::varint::{VarintError, read_varint, write_varint};

/// A malformed or truncated binary stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{what} at byte {offset}: {reason}")]
pub struct BinaryError {
    /// The field being read.
    pub what: &'static str,
    /// Byte offset where reading failed.
    pub offset: usize,
    /// What was wrong.
    pub reason: &'static str,
}

/// Reads C++ `BinaryInputStreamSerializer` data.
#[derive(Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts at the beginning of `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current byte offset.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not read yet.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn err(&self, what: &'static str, reason: &'static str) -> BinaryError {
        BinaryError {
            what,
            offset: self.pos,
            reason,
        }
    }

    fn varint(&mut self, bits: u32, what: &'static str) -> Result<u64, BinaryError> {
        let start = self.pos;
        read_varint(self.data, &mut self.pos, bits).map_err(|e| BinaryError {
            what,
            offset: start,
            reason: match e {
                VarintError::Eof => "unexpected end of data",
                VarintError::Overflow => "varint overflow",
                VarintError::NonCanonical => "non-canonical varint",
            },
        })
    }

    /// C++ `operator()(uint8_t&)`.
    pub fn u8(&mut self, what: &'static str) -> Result<u8, BinaryError> {
        self.varint(8, what).map(|v| v as u8)
    }

    /// C++ `operator()(uint32_t&)`.
    pub fn u32(&mut self, what: &'static str) -> Result<u32, BinaryError> {
        self.varint(32, what).map(|v| v as u32)
    }

    /// C++ `operator()(uint64_t&)`.
    pub fn u64(&mut self, what: &'static str) -> Result<u64, BinaryError> {
        self.varint(64, what)
    }

    /// C++ `operator()(int64_t&)`: the varint of the `u64` bit pattern.
    pub fn i64(&mut self, what: &'static str) -> Result<i64, BinaryError> {
        self.varint(64, what).map(|v| v as i64)
    }

    /// C++ `operator()(bool&)`: one byte, non-zero is true.
    pub fn bool(&mut self, what: &'static str) -> Result<bool, BinaryError> {
        Ok(self.bytes(1, what)?[0] != 0)
    }

    /// C++ `binary(void*, size)`: `n` raw bytes.
    pub fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], BinaryError> {
        if self.remaining() < n {
            return Err(self.err(what, "unexpected end of data"));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// A POD of `N` bytes (`serializePod`).
    pub fn pod<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], BinaryError> {
        let b = self.bytes(N, what)?;
        Ok(b.try_into().expect("bytes returned N bytes"))
    }

    /// C++ `operator()(std::string&)`: varint length, then the bytes.
    pub fn string(&mut self, what: &'static str) -> Result<Vec<u8>, BinaryError> {
        let len = self.u64(what)?;
        let len = usize::try_from(len).map_err(|_| self.err(what, "length too large"))?;
        self.bytes(len, what).map(<[u8]>::to_vec)
    }

    /// C++ `serialize(std::vector<uint8_t>&)` through `serializeContainer`: varint count, then
    /// each byte as a `uint8_t` varint.
    pub fn byte_vector(&mut self, what: &'static str) -> Result<Vec<u8>, BinaryError> {
        let count = self.u64(what)?;
        // Every element takes at least one byte, so a count above the remaining length cannot
        // be satisfied; check before allocating.
        if count > self.remaining() as u64 {
            return Err(self.err(what, "count exceeds the data"));
        }
        let mut out = Vec::with_capacity(count as usize);
        for _ in 0..count {
            out.push(self.u8(what)?);
        }
        Ok(out)
    }
}

/// Writes C++ `BinaryOutputStreamSerializer` data.
#[derive(Debug, Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes written so far.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }

    /// Any unsigned integer as a varint.
    pub fn varint(&mut self, value: u64) {
        write_varint(&mut self.out, value);
    }

    /// C++ `operator()(int64_t&)`.
    pub fn i64(&mut self, value: i64) {
        self.varint(value as u64);
    }

    /// C++ `operator()(bool&)`.
    pub fn bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }

    /// Raw bytes (a POD).
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    /// C++ `operator()(std::string&)`.
    pub fn string(&mut self, bytes: &[u8]) {
        self.varint(bytes.len() as u64);
        self.bytes(bytes);
    }

    /// C++ `serialize(std::vector<uint8_t>&)`: varint count, each byte as a varint.
    pub fn byte_vector(&mut self, bytes: &[u8]) {
        self.varint(bytes.len() as u64);
        for b in bytes {
            self.varint(u64::from(*b));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_vector_uses_one_varint_per_byte() {
        let mut w = Writer::new();
        w.byte_vector(&[0x00, 0x7f, 0x80, 0xff]);
        assert_eq!(w.into_bytes(), vec![4, 0x00, 0x7f, 0x80, 0x01, 0xff, 0x01]);
        let data = [4u8, 0x00, 0x7f, 0x80, 0x01, 0xff, 0x01];
        let mut r = Reader::new(&data);
        assert_eq!(r.byte_vector("v").unwrap(), vec![0x00, 0x7f, 0x80, 0xff]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn u8_varint_rejects_values_above_255() {
        // 0x80 0x02 = 256
        let mut r = Reader::new(&[0x80, 0x02]);
        assert_eq!(r.u8("b").unwrap_err().reason, "varint overflow");
        // 0xff 0x01 = 255
        let mut r = Reader::new(&[0xff, 0x01]);
        assert_eq!(r.u8("b").unwrap(), 255);
    }

    #[test]
    fn signed_and_strings() {
        let mut w = Writer::new();
        w.i64(-1);
        w.string(b"abc");
        w.bool(true);
        w.bytes(&[9; 4]);
        let data = w.into_bytes();
        assert_eq!(data.len(), 10 + 4 + 1 + 4);
        let mut r = Reader::new(&data);
        assert_eq!(r.i64("i").unwrap(), -1);
        assert_eq!(r.string("s").unwrap(), b"abc");
        assert!(r.bool("b").unwrap());
        assert_eq!(r.pod::<4>("p").unwrap(), [9; 4]);
        assert!(r.bytes(1, "x").is_err());
    }

    #[test]
    fn truncated_and_oversized() {
        let mut r = Reader::new(&[5, b'a']);
        assert!(r.string("s").is_err());
        let mut r = Reader::new(&[3, 1]);
        assert_eq!(
            r.byte_vector("v").unwrap_err().reason,
            "count exceeds the data"
        );
        let mut r = Reader::new(&[0x80]);
        assert_eq!(r.u64("n").unwrap_err().reason, "unexpected end of data");
        let mut r = Reader::new(&[0x80, 0x00]);
        assert_eq!(r.u64("n").unwrap_err().reason, "non-canonical varint");
    }
}
