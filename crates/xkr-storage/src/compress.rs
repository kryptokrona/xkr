//! zstd framing for stored blobs, with an optional trained dictionary.
//!
//! Every compressed value starts with a 5-byte header, so decompression never guesses:
//!
//! ```text
//! byte 0      tag: 0 = stored raw, 1 = zstd, 2 = zstd with dictionary
//! bytes 1..5  uncompressed length, u32 LE
//! [tag 2]     bytes 5..9 dictionary id, u32 LE
//! rest        payload
//! ```
//!
//! Blobs smaller than [`MIN_COMPRESS_LEN`] or that do not shrink are stored raw. Sealed chunks
//! (64 blocks of records) are compressed with [`Compressor::compress_chunk`] at
//! [`CHUNK_LEVEL`]. The dictionary (`META_BLOBS["zstd_dictionary"]`) is the one new chunks
//! are written with; a [`Compressor`] can decode frames of any number of extra dictionaries
//! ([`Compressor::add_decoder_dictionary`]), which is how a re-compression to a new dictionary
//! stays readable while it runs. Fresh mainnet databases start with the dictionary shipped in
//! `dicts/` ([`crate::builtin_dictionary`]) so two nodes write identical chunks.

use std::fmt;

use crate::{Error, Result, ops::chunk::ChunkCache};

/// Blobs shorter than this are stored raw (zstd frame overhead would eat the gain).
pub const MIN_COMPRESS_LEN: usize = 64;
/// Default zstd level for small blobs (alternative blocks): fast.
pub const DEFAULT_LEVEL: i32 = 3;
/// zstd level of sealed chunks (64 blocks, written once, read often).
pub const CHUNK_LEVEL: i32 = 9;

const TAG_RAW: u8 = 0;
const TAG_ZSTD: u8 = 1;
const TAG_ZSTD_DICT: u8 = 2;
const ZSTD_DICT_MAGIC: u32 = 0xEC30_A437;
/// The most a zstd block decompresses to (RFC 8878 `Block_Maximum_Size`).
const ZSTD_MAX_BLOCK_OUTPUT: usize = 128 * 1024;
/// Every zstd block starts with a 3-byte header, so a payload holds at most `len / 3` blocks.
const ZSTD_BLOCK_HEADER: usize = 3;

/// Reusable zstd contexts (one per writer/reader thread; not `Sync`), plus the last decoded
/// chunk of a read path (block-by-block reads of one chunk decompress it once).
pub struct Compressor {
    level: i32,
    dictionary: Option<Vec<u8>>,
    dict_id: Option<u32>,
    enc: zstd::bulk::Compressor<'static>,
    chunk_enc: Option<zstd::bulk::Compressor<'static>>,
    dec: zstd::bulk::Decompressor<'static>,
    dict_decs: Vec<(u32, zstd::bulk::Decompressor<'static>)>,
    pub(crate) chunk_cache: Option<Box<ChunkCache>>,
}

impl fmt::Debug for Compressor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compressor")
            .field("level", &self.level)
            .field("dict_id", &self.dict_id)
            .finish_non_exhaustive()
    }
}

impl Compressor {
    /// Plain zstd at `level`.
    pub fn new(level: i32) -> Result<Self> {
        Ok(Self {
            level,
            dictionary: None,
            dict_id: None,
            enc: zstd::bulk::Compressor::new(level)?,
            chunk_enc: None,
            dec: zstd::bulk::Decompressor::new()?,
            dict_decs: Vec::new(),
            chunk_cache: None,
        })
    }

    /// zstd with a trained dictionary at [`DEFAULT_LEVEL`].
    pub fn with_dictionary(dictionary: &[u8]) -> Result<Self> {
        Self::with_dictionary_level(dictionary, DEFAULT_LEVEL)
    }

    /// zstd with a trained dictionary at `level`.
    pub fn with_dictionary_level(dictionary: &[u8], level: i32) -> Result<Self> {
        let id = dictionary_id(dictionary);
        Ok(Self {
            level,
            dictionary: Some(dictionary.to_vec()),
            dict_id: Some(id),
            enc: zstd::bulk::Compressor::with_dictionary(level, dictionary)?,
            chunk_enc: None,
            dec: zstd::bulk::Decompressor::new()?,
            dict_decs: vec![(id, zstd::bulk::Decompressor::with_dictionary(dictionary)?)],
            chunk_cache: None,
        })
    }

    /// Also decode frames written with `dictionary` (writing keeps the current one).
    pub fn add_decoder_dictionary(&mut self, dictionary: &[u8]) -> Result<()> {
        let id = dictionary_id(dictionary);
        if !self.dict_decs.iter().any(|(i, _)| *i == id) {
            self.dict_decs
                .push((id, zstd::bulk::Decompressor::with_dictionary(dictionary)?));
        }
        Ok(())
    }

    /// Id of the dictionary used for writing, if any.
    #[must_use]
    pub const fn dictionary_id(&self) -> Option<u32> {
        self.dict_id
    }

    /// Drops the cached chunk (after maintenance that rewrites chunks).
    pub fn clear_cache(&mut self) {
        self.chunk_cache = None;
    }

    /// Compresses `data` into the framed format at the context's level.
    pub fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let id = self.dict_id;
        frame(data, id, |d| Ok(self.enc.compress(d)?))
    }

    /// Compresses a chunk body at [`CHUNK_LEVEL`] (same framing).
    pub fn compress_chunk(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if self.chunk_enc.is_none() {
            self.chunk_enc = Some(match &self.dictionary {
                Some(d) => zstd::bulk::Compressor::with_dictionary(CHUNK_LEVEL, d)?,
                None => zstd::bulk::Compressor::new(CHUNK_LEVEL)?,
            });
        }
        let id = self.dict_id;
        let enc = self.chunk_enc.as_mut().expect("just created");
        frame(data, id, |d| Ok(enc.compress(d)?))
    }

    /// Reverses [`Self::compress`] and [`Self::compress_chunk`].
    pub fn decompress(&mut self, framed: &[u8]) -> Result<Vec<u8>> {
        if framed.len() < 5 {
            return Err(Error::Corrupt("compressed blob shorter than its header"));
        }
        let len = u32::from_le_bytes(framed[1..5].try_into().expect("4 bytes")) as usize;
        match framed[0] {
            TAG_RAW => {
                let body = &framed[5..];
                if body.len() != len {
                    return Err(Error::Corrupt("raw blob length mismatch"));
                }
                Ok(body.to_vec())
            }
            TAG_ZSTD => {
                let payload = &framed[5..];
                check_declared_len(len, payload)?;
                let out = self.dec.decompress(payload, len)?;
                check_len(out, len)
            }
            TAG_ZSTD_DICT => {
                if framed.len() < 9 {
                    return Err(Error::Corrupt("dictionary blob shorter than its header"));
                }
                let id = u32::from_le_bytes(framed[5..9].try_into().expect("4 bytes"));
                let Some((_, dec)) = self.dict_decs.iter_mut().find(|(i, _)| *i == id) else {
                    return Err(Error::Corrupt(
                        "blob was written with another zstd dictionary",
                    ));
                };
                let payload = &framed[9..];
                check_declared_len(len, payload)?;
                let out = dec.decompress(payload, len)?;
                check_len(out, len)
            }
            _ => Err(Error::Corrupt("unknown compression tag")),
        }
    }
}

/// The dictionary id recorded in a framed blob (`None` for raw and plain zstd frames).
#[must_use]
pub fn framed_dictionary_id(framed: &[u8]) -> Option<u32> {
    (framed.len() >= 9 && framed[0] == TAG_ZSTD_DICT)
        .then(|| u32::from_le_bytes(framed[5..9].try_into().expect("4 bytes")))
}

/// Whether a framed blob is stored raw (it then depends on no dictionary).
#[must_use]
pub fn is_raw_frame(framed: &[u8]) -> bool {
    framed.first() == Some(&TAG_RAW)
}

fn frame(
    data: &[u8],
    dict_id: Option<u32>,
    compress: impl FnOnce(&[u8]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let len = u32::try_from(data.len()).map_err(|_| Error::Corrupt("blob larger than 4 GiB"))?;
    if data.len() >= MIN_COMPRESS_LEN {
        let packed = compress(data)?;
        let header = if dict_id.is_some() { 9 } else { 5 };
        if packed.len() + header < data.len() + 5 {
            let mut out = Vec::with_capacity(header + packed.len());
            if let Some(id) = dict_id {
                out.push(TAG_ZSTD_DICT);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&id.to_le_bytes());
            } else {
                out.push(TAG_ZSTD);
                out.extend_from_slice(&len.to_le_bytes());
            }
            out.extend_from_slice(&packed);
            return Ok(out);
        }
    }
    let mut out = Vec::with_capacity(5 + data.len());
    out.push(TAG_RAW);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// zstd pre-allocates the declared length, so a corrupt header (a damaged database
/// file, a downloaded snapshot) could make a 9-byte blob reserve 4 GiB. No zstd payload can
/// decompress to more than `(len / 3 + 1) * 128 KiB` (every block has a 3-byte header and yields
/// at most 128 KiB), so a larger declared length is refused before anything is allocated.
fn check_declared_len(len: usize, payload: &[u8]) -> Result<()> {
    let bound = (payload.len() / ZSTD_BLOCK_HEADER + 1).saturating_mul(ZSTD_MAX_BLOCK_OUTPUT);
    if len > bound {
        return Err(Error::Corrupt(
            "declared length larger than the zstd payload can hold",
        ));
    }
    Ok(())
}

fn check_len(out: Vec<u8>, len: usize) -> Result<Vec<u8>> {
    if out.len() == len {
        Ok(out)
    } else {
        Err(Error::Corrupt("decompressed length mismatch"))
    }
}

/// zstd dictionary id from a "formatted" dictionary (magic `0xEC30A437`, id at bytes 4..8),
/// or an FNV-1a hash of the bytes for a raw-content dictionary.
#[must_use]
pub fn dictionary_id(dictionary: &[u8]) -> u32 {
    if dictionary.len() >= 8
        && u32::from_le_bytes(dictionary[0..4].try_into().expect("4 bytes")) == ZSTD_DICT_MAGIC
    {
        return u32::from_le_bytes(dictionary[4..8].try_into().expect("4 bytes"));
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in dictionary {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Trains a zstd dictionary of at most `max_bytes` over `samples` (block records).
pub fn train_dictionary(samples: &[Vec<u8>], max_bytes: usize) -> Result<Vec<u8>> {
    if samples.len() < 8 {
        return Err(Error::Corrupt("too few samples to train a dictionary"));
    }
    Ok(zstd::dict::from_samples(samples, max_bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut c = Compressor::new(DEFAULT_LEVEL).unwrap();
        for data in [
            vec![],
            vec![1_u8; 10],
            vec![7_u8; 10_000],
            (0..=255_u8).collect::<Vec<_>>(),
        ] {
            let framed = c.compress(&data).unwrap();
            assert_eq!(c.decompress(&framed).unwrap(), data);
            let framed = c.compress_chunk(&data).unwrap();
            assert_eq!(c.decompress(&framed).unwrap(), data);
        }
        // Highly repetitive data really is compressed.
        assert!(c.compress(&[0_u8; 10_000]).unwrap().len() < 100);
    }

    #[test]
    fn dictionary_round_trip_and_mismatch() {
        // A raw-content dictionary is accepted by zstd.
        let dict: Vec<u8> = (0..4096_u32).map(|i| (i % 251) as u8).collect();
        let mut with = Compressor::with_dictionary(&dict).unwrap();
        let data: Vec<u8> = (0..2000_u32).map(|i| (i % 251) as u8).collect();
        let framed = with.compress(&data).unwrap();
        assert_eq!(with.decompress(&framed).unwrap(), data);
        let chunk = with.compress_chunk(&data).unwrap();
        assert_eq!(with.decompress(&chunk).unwrap(), data);

        let mut plain = Compressor::new(DEFAULT_LEVEL).unwrap();
        if framed[0] == TAG_ZSTD_DICT {
            assert_eq!(framed_dictionary_id(&framed), Some(dictionary_id(&dict)));
            assert!(plain.decompress(&framed).is_err());
            // A second decoder dictionary makes it readable.
            plain.add_decoder_dictionary(&dict).unwrap();
            assert_eq!(plain.decompress(&framed).unwrap(), data);
        }
        let plain_framed = plain.compress(&data).unwrap();
        assert_eq!(with.decompress(&plain_framed).unwrap(), data);
    }
}
