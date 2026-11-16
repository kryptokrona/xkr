//! epee helpers for TurtleCoin's "POD as binary blob" fields.
//!
//! The C++ node serialises some containers with `serializeAsBinary(container, name, s)`
//! (`src/serialization/serialization_overloads.h`): the elements are `memcpy`'d into one
//! `std::string` and written as a single epee *string* (type 10). That is used for every
//! `std::vector<crypto::Hash>` / `std::list<PeerlistEntry>` / `std::list<connection_entry>`
//! in the P2P and protocol messages. Single hashes and the uuid go through
//! `ISerializer::binary(ptr, size, name)`, which is also a plain string.
//!
//! Empty containers are *omitted* by the C++ writer (`binary()` with size 0 writes
//! nothing) and read back as empty; [`PodList`] does the same.
//!
//! Orphan rules stop us from implementing `xkr_epee::EpeeValue` for `xkr_types::Hash` or
//! `Vec<Hash>` directly, so these are transparent newtypes used via the
//! `field: Type as Newtype` form of `epee_object!`.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use ref_cast::RefCast;
use xkr_epee::{EpeeValue, EpeeValueLimits, Error, Marker, Result, write_bytes};
use xkr_types::Hash;

/// A fixed-size little-endian record that can live inside a [`PodList`] blob.
pub trait FixedBlob: Sized {
    /// Encoded size in bytes (C++ `sizeof(T)` including padding).
    const SIZE: usize;
    /// Decodes one record; `bytes.len() == Self::SIZE` is guaranteed by the caller.
    fn read_blob(bytes: &[u8]) -> Self;
    /// Appends exactly `Self::SIZE` bytes.
    fn write_blob(&self, out: &mut BytesMut);
}

impl FixedBlob for Hash {
    const SIZE: usize = 32;

    fn read_blob(bytes: &[u8]) -> Self {
        let mut h = [0_u8; 32];
        h.copy_from_slice(bytes);
        Hash(h)
    }

    fn write_blob(&self, out: &mut BytesMut) {
        out.put_slice(&self.0);
    }
}

/// `std::vector<T>` / `std::list<T>` serialised with `serializeAsBinary`.
#[derive(Debug, Clone, PartialEq, Eq, Default, RefCast)]
#[repr(transparent)]
pub struct PodList<T>(pub Vec<T>);

impl<T> From<Vec<T>> for PodList<T> {
    fn from(value: Vec<T>) -> Self {
        Self(value)
    }
}

impl<T> From<PodList<T>> for Vec<T> {
    fn from(value: PodList<T>) -> Self {
        value.0
    }
}

impl<'a, T> From<&'a Vec<T>> for &'a PodList<T> {
    fn from(value: &'a Vec<T>) -> Self {
        PodList::ref_cast(value)
    }
}

impl<T: FixedBlob> EpeeValue for PodList<T> {
    const MARKER: Marker = <Bytes as EpeeValue>::MARKER;

    fn read<B: Buf>(r: &mut B, marker: &Marker, limits: EpeeValueLimits) -> Result<Self> {
        let bytes = <Bytes as EpeeValue>::read(r, marker, EpeeValueLimits::default())?;
        if bytes.len() % T::SIZE != 0 {
            // C++: `throw std::runtime_error("Invalid blob size given!")`
            return Err(Error::Format("Invalid blob size given!"));
        }
        let count = bytes.len() / T::SIZE;
        if count > limits.max_sequence_len {
            return Err(Error::Format("Sequence exceeded maximum length"));
        }
        Ok(Self(
            bytes.chunks_exact(T::SIZE).map(T::read_blob).collect(),
        ))
    }

    fn should_write(&self) -> bool {
        !self.0.is_empty()
    }

    fn epee_default_value() -> Option<Self> {
        Some(Self(Vec::new()))
    }

    fn write<B: BufMut>(self, w: &mut B) -> Result<()> {
        let mut buf = BytesMut::with_capacity(self.0.len() * T::SIZE);
        for item in &self.0 {
            item.write_blob(&mut buf);
        }
        write_bytes(buf, w)
    }
}

/// A single `crypto::Hash` field (`serializePod` -> 32-byte epee string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, RefCast)]
#[repr(transparent)]
pub struct HashValue(pub Hash);

impl From<Hash> for HashValue {
    fn from(value: Hash) -> Self {
        Self(value)
    }
}

impl From<HashValue> for Hash {
    fn from(value: HashValue) -> Self {
        value.0
    }
}

impl<'a> From<&'a Hash> for &'a HashValue {
    fn from(value: &'a Hash) -> Self {
        HashValue::ref_cast(value)
    }
}

impl EpeeValue for HashValue {
    const MARKER: Marker = <[u8; 32] as EpeeValue>::MARKER;

    fn read<B: Buf>(r: &mut B, marker: &Marker, limits: EpeeValueLimits) -> Result<Self> {
        Ok(Self(Hash(<[u8; 32] as EpeeValue>::read(
            r, marker, limits,
        )?)))
    }

    fn write<B: BufMut>(self, w: &mut B) -> Result<()> {
        <[u8; 32] as EpeeValue>::write(self.0.0, w)
    }
}
