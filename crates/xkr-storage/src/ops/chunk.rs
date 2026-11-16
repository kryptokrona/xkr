//! Sealed chunks: `CHUNK_BLOCKS` consecutive block records compressed together, and the
//! matching signature stream, each split into [`PART_LEN`] values so every part fills one page.

use redb::{ReadableTable, Table};

use crate::{
    Error, Result,
    codec::{BlockRecord, decode_chunk},
    compress::{Compressor, DEFAULT_LEVEL},
    schema::{PART_LEN, meta, part_key},
};

/// The last decoded chunk of a read path, kept in the [`Compressor`].
#[derive(Debug)]
pub(crate) struct ChunkCache {
    pub chunk: u64,
    /// Hash of the chunk's last block: a chunk that was unsealed and sealed again with other
    /// blocks (a reorg deeper than the seal lag) never matches a stale cache.
    pub tag: [u8; 32],
    pub records: Vec<BlockRecord>,
    /// Offset of each block's signatures in the chunk's signature stream.
    pub sig_base: Vec<u64>,
}

impl ChunkCache {
    pub fn new(chunk: u64, tag: [u8; 32], records: Vec<BlockRecord>) -> Self {
        let mut sig_base = Vec::with_capacity(records.len());
        let mut at = 0;
        for r in &records {
            sig_base.push(at);
            at += r.sig_bytes();
        }
        Self {
            chunk,
            tag,
            records,
            sig_base,
        }
    }
}

/// Most parts of one chunk: the part index takes the low 16 bits of the key ([`part_key`]).
const MAX_PARTS: u64 = 1 << 16;

/// Writes `data` as the parts of `chunk`.
pub(crate) fn write_parts(
    table: &mut Table<'_, u64, &'static [u8]>,
    chunk: u64,
    data: &[u8],
) -> Result<()> {
    for (i, part) in data.chunks(PART_LEN).enumerate() {
        if i as u64 >= MAX_PARTS {
            return Err(Error::Corrupt("chunk has more than 65536 parts"));
        }
        table.insert(part_key(chunk, i as u64), part)?;
    }
    Ok(())
}

/// Deletes every part of `chunk`; returns how many there were.
pub(crate) fn delete_parts(table: &mut Table<'_, u64, &'static [u8]>, chunk: u64) -> Result<u64> {
    let mut n = 0;
    table.retain_in(part_key(chunk, 0)..part_key(chunk + 1, 0), |_, _| {
        n += 1;
        false
    })?;
    Ok(n)
}

/// All parts of `chunk` concatenated; `None` if it has none.
pub(crate) fn read_parts<T: ReadableTable<u64, &'static [u8]>>(
    table: &T,
    chunk: u64,
) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut any = false;
    for row in table.range(part_key(chunk, 0)..part_key(chunk + 1, 0))? {
        let (_, v) = row?;
        out.extend_from_slice(v.value());
        any = true;
    }
    Ok(any.then_some(out))
}

/// Bytes `off..off + len` of a chunk's concatenated parts, reading only the parts needed.
/// `None` if a needed part is missing (pruned signatures).
pub(crate) fn read_part_range<T: ReadableTable<u64, &'static [u8]>>(
    table: &T,
    chunk: u64,
    off: u64,
    len: u64,
) -> Result<Option<Vec<u8>>> {
    if len == 0 {
        return Ok(Some(Vec::new()));
    }
    let part = PART_LEN as u64;
    let first = off / part;
    let last = off
        .checked_add(len - 1)
        .ok_or(Error::Corrupt("signature range beyond its chunk"))?
        / part;
    // A chunk has at most `MAX_PARTS` parts ([`write_parts`]); a part index above that would be
    // a key of the next chunk.
    if last >= MAX_PARTS {
        return Err(Error::Corrupt("signature range beyond its chunk"));
    }
    // The buffer grows with the parts actually read, not with the length the record states.
    let mut buf = Vec::new();
    for p in first..=last {
        match table.get(part_key(chunk, p))? {
            Some(v) => buf.extend_from_slice(v.value()),
            None => return Ok(None),
        }
    }
    let start = (off - first * part) as usize;
    let end = start + len as usize;
    if end > buf.len() {
        return Err(Error::Corrupt("signature range beyond its chunk"));
    }
    buf.truncate(end);
    buf.drain(..start);
    Ok(Some(buf))
}

/// Decompresses and decodes a sealed chunk body.
pub(crate) fn decode_body(framed: &[u8], comp: &mut Compressor) -> Result<Vec<BlockRecord>> {
    let body = comp.decompress(framed)?;
    decode_chunk(&body)?
        .into_iter()
        .map(BlockRecord::decode)
        .collect()
}

/// A [`Compressor`] with the database's dictionaries: the current one for writing, plus the
/// previous one (during a re-compression) for reading.
pub(crate) fn compressor_from_blobs<T: ReadableTable<&'static str, &'static [u8]>>(
    blobs: &T,
) -> Result<Compressor> {
    let current = blobs
        .get(meta::ZSTD_DICTIONARY)?
        .map(|g| g.value().to_vec());
    let prev = blobs
        .get(meta::ZSTD_DICTIONARY_PREV)?
        .map(|g| g.value().to_vec());
    let mut comp = match current {
        Some(d) => Compressor::with_dictionary(&d)?,
        None => Compressor::new(DEFAULT_LEVEL)?,
    };
    if let Some(p) = prev {
        comp.add_decoder_dictionary(&p)?;
    }
    Ok(comp)
}

#[cfg(test)]
mod tests {
    use redb::{Database, ReadableDatabase as _, backends::InMemoryBackend};

    use super::*;
    use crate::schema::SIG_PARTS;

    fn db_with_parts(chunk: u64, data: &[u8]) -> Database {
        let db = Database::builder()
            .create_with_backend(InMemoryBackend::new())
            .expect("in-memory database");
        let txn = db.begin_write().expect("write");
        {
            let mut t = txn.open_table(SIG_PARTS).expect("table");
            write_parts(&mut t, chunk, data).expect("parts");
        }
        txn.commit().expect("commit");
        db
    }

    #[test]
    fn part_ranges_read_across_parts() {
        let data: Vec<u8> = (0..3 * PART_LEN + 17).map(|i| (i % 251) as u8).collect();
        let db = db_with_parts(5, &data);
        let txn = db.begin_read().expect("read");
        let t = txn.open_table(SIG_PARTS).expect("table");
        let off = PART_LEN as u64 - 3;
        let got = read_part_range(&t, 5, off, 2 * PART_LEN as u64).expect("range");
        assert_eq!(
            got.as_deref(),
            Some(&data[off as usize..off as usize + 2 * PART_LEN])
        );
        assert_eq!(
            read_part_range(&t, 5, 7, 0).expect("empty"),
            Some(Vec::new())
        );
        // Another chunk has no parts.
        assert_eq!(read_part_range(&t, 6, 0, 10).expect("missing"), None);
    }

    #[test]
    fn part_ranges_stay_inside_their_chunk() {
        // Part indices of chunk 0 end at 0xffff; the keys after them are chunk 1's.
        let db = db_with_parts(1, &[7; 64]);
        let txn = db.begin_read().expect("read");
        let t = txn.open_table(SIG_PARTS).expect("table");
        let past_last_part = MAX_PARTS * PART_LEN as u64;
        assert!(matches!(
            read_part_range(&t, 0, past_last_part, 10),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            read_part_range(&t, 0, 0, u64::MAX),
            Err(Error::Corrupt(_))
        ));
        assert!(matches!(
            read_part_range(&t, 0, u64::MAX, 2),
            Err(Error::Corrupt(_))
        ));
        // A range longer than the stored parts.
        let db = db_with_parts(0, &[1; 100]);
        let txn = db.begin_read().expect("read");
        let t = txn.open_table(SIG_PARTS).expect("table");
        assert!(matches!(
            read_part_range(&t, 0, 50, 51),
            Err(Error::Corrupt(_))
        ));
        assert_eq!(
            read_part_range(&t, 0, 50, 50).expect("tail"),
            Some(vec![1; 50])
        );
    }
}
