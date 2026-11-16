//! Bounds-checked cursor used by all decoders (C++ `MemoryInputStream` +
//! `BinaryInputStreamSerializer`).

use xkr_types::Hash;

use crate::error::DecodeError;
use crate::varint::{VarintError, read_varint};

#[derive(Debug)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Fails with `TrailingBytes` unless everything was consumed (`fromBinaryArray`'s
    /// `endOfStream` check).
    pub(crate) fn finish(&self, what: &'static str) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            trailing => Err(DecodeError::TrailingBytes { what, trailing }),
        }
    }

    /// Raw bytes (`serializer.binary(ptr, n)`).
    pub(crate) fn bytes(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let offset = self.pos;
        let end = offset.checked_add(n).filter(|e| *e <= self.data.len());
        let Some(end) = end else {
            return Err(DecodeError::UnexpectedEof { offset, what });
        };
        self.pos = end;
        Ok(&self.data[offset..end])
    }

    pub(crate) fn u8_raw(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.bytes(1, what)?[0])
    }

    pub(crate) fn array32(&mut self, what: &'static str) -> Result<[u8; 32], DecodeError> {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.bytes(32, what)?);
        Ok(out)
    }

    pub(crate) fn hash(&mut self, what: &'static str) -> Result<Hash, DecodeError> {
        Ok(Hash(self.array32(what)?))
    }

    pub(crate) fn array64(&mut self, what: &'static str) -> Result<[u8; 64], DecodeError> {
        let mut out = [0u8; 64];
        out.copy_from_slice(self.bytes(64, what)?);
        Ok(out)
    }

    /// 4 raw little-endian bytes (`serializer.binary(&nonce, sizeof(nonce))` on x86).
    pub(crate) fn u32_le(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        let mut w = [0u8; 4];
        w.copy_from_slice(self.bytes(4, what)?);
        Ok(u32::from_le_bytes(w))
    }

    fn varint(&mut self, bits: u32, what: &'static str) -> Result<u64, DecodeError> {
        let offset = self.pos;
        let mut pos = self.pos;
        let r = read_varint(self.data, &mut pos, bits);
        match r {
            Ok(v) => {
                self.pos = pos;
                Ok(v)
            }
            Err(VarintError::Eof) => Err(DecodeError::UnexpectedEof { offset, what }),
            Err(VarintError::Overflow) => Err(DecodeError::VarintOverflow { offset, what }),
            Err(VarintError::NonCanonical) => Err(DecodeError::VarintNonCanonical { offset, what }),
        }
    }

    pub(crate) fn varint_u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.varint(8, what)? as u8)
    }

    pub(crate) fn varint_u32(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        Ok(self.varint(32, what)? as u32)
    }

    pub(crate) fn varint_u64(&mut self, what: &'static str) -> Result<u64, DecodeError> {
        self.varint(64, what)
    }

    /// A `u64` element count (`beginArray`) where every element needs at least `min_elem_size`
    /// bytes; rejects counts that cannot possibly fit in the remaining input.
    pub(crate) fn count(
        &mut self,
        min_elem_size: usize,
        what: &'static str,
    ) -> Result<usize, DecodeError> {
        let offset = self.pos;
        let len = self.varint_u64(what)?;
        let remaining = self.remaining();
        let fits = usize::try_from(len)
            .ok()
            .and_then(|n| n.checked_mul(min_elem_size.max(1)))
            .is_some_and(|need| need <= remaining);
        if !fits {
            return Err(DecodeError::LengthTooLarge {
                offset,
                what,
                len,
                remaining,
            });
        }
        // `fits` implies the conversion succeeded.
        Ok(usize::try_from(len).unwrap_or(0))
    }
}
