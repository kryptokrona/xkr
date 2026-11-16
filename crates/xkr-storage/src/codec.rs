//! Byte codecs of the v2 layout: varints, key-stripped transaction blobs, block records and
//! chunk bodies (`docs/storage.md` "Records and chunks").
//!
//! **Key stripping.** Every output's 32-byte one-time key is stored once, in the `outputs`
//! table (ring members need it there). The serialized miner transaction (inside the block blob)
//! and every transaction prefix are stored with those keys cut out; the record keeps the byte
//! offset of the first output and the outputs' global indexes, and reading puts the keys back
//! from `outputs[(amount, global_index)]`. The outputs section of a CryptoNote prefix is
//! `varint amount | 0x02 | key[32]` per output, so the walk needs nothing else. Stripping is
//! verified while it is done; a blob whose outputs cannot be located byte-exactly is stored
//! unstripped (`outputs_at = 0`).

use crate::{Error, Result};

/// Variant tag of a key output (`TransactionOutputToKey`).
pub const TAG_KEY_OUTPUT: u8 = 0x02;

const RECORD_VERSION: u8 = 1;
const CHUNK_VERSION: u8 = 1;

/// Appends `v` as a LEB128 varint (the CryptoNote varint).
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Encoded length of `v`.
#[must_use]
pub const fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

/// Reads a varint at `b[pos..]`: `(value, length)`. `None` if truncated or longer than 64 bits.
#[must_use]
pub fn varint_at(b: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..10 {
        let byte = *b.get(pos + i)?;
        let bits = u64::from(byte & 0x7f);
        if i == 9 && bits > 1 {
            return None;
        }
        v |= bits << (7 * i);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// A cursor over encoded bytes.
struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn varint(&mut self) -> Result<u64> {
        let (v, n) = varint_at(self.0, 0).ok_or(Error::Corrupt("bad varint in record"))?;
        self.0 = &self.0[n..];
        Ok(v)
    }

    fn usize(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| Error::Corrupt("length overflow in record"))
    }

    fn u32(&mut self) -> Result<u32> {
        u32::try_from(self.varint()?).map_err(|_| Error::Corrupt("u32 overflow in record"))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(Error::Corrupt("truncated record"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u32_list(&mut self) -> Result<Vec<u32>> {
        let n = self.usize()?;
        if n > self.0.len() {
            return Err(Error::Corrupt("list longer than its record"));
        }
        (0..n).map(|_| self.u32()).collect()
    }
}

/// Cuts the output keys out of a serialized transaction (prefix) or block blob.
///
/// `outputs` are the transaction's outputs `(amount, key)` in order. Returns
/// `(offset of the first output, stripped bytes)`, or `None` if the outputs section cannot be
/// located byte-exactly (the caller then stores the blob as it is).
#[must_use]
pub fn strip_output_keys(blob: &[u8], outputs: &[(u64, [u8; 32])]) -> Option<(usize, Vec<u8>)> {
    let (amount0, key0) = outputs.first()?;
    let back = 1 + varint_len(*amount0);
    let mut from = 0;
    while let Some(found) = find(&blob[from..], key0) {
        let q = from + found;
        if q >= back
            && let Some(stripped) = try_strip(blob, q - back, outputs)
        {
            return Some((q - back, stripped));
        }
        from = q + 1;
    }
    None
}

fn find(hay: &[u8], needle: &[u8; 32]) -> Option<usize> {
    if hay.len() < 32 {
        return None;
    }
    let first = needle[0];
    (0..=hay.len() - 32).find(|&i| hay[i] == first && &hay[i..i + 32] == needle)
}

fn try_strip(blob: &[u8], off: usize, outputs: &[(u64, [u8; 32])]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(blob.len().saturating_sub(32 * outputs.len()));
    out.extend_from_slice(blob.get(..off)?);
    let mut p = off;
    for (amount, key) in outputs {
        let start = p;
        let (v, n) = varint_at(blob, p)?;
        if v != *amount {
            return None;
        }
        p += n;
        if *blob.get(p)? != TAG_KEY_OUTPUT {
            return None;
        }
        p += 1;
        if blob.get(p..p + 32)? != key {
            return None;
        }
        out.extend_from_slice(&blob[start..p]);
        p += 32;
    }
    out.extend_from_slice(&blob[p..]);
    Some(out)
}

/// Reverses [`strip_output_keys`]: `key(i, amount)` returns the key of output `i`.
pub fn unstrip_output_keys(
    stripped: &[u8],
    off: usize,
    count: usize,
    mut key: impl FnMut(usize, u64) -> Result<[u8; 32]>,
) -> Result<Vec<u8>> {
    // `count` comes from the database; never let it size an allocation on its own (every
    // output needs at least 2 stripped bytes).
    let mut out = Vec::with_capacity(stripped.len() + 32 * count.min(stripped.len() / 2));
    out.extend_from_slice(
        stripped
            .get(..off)
            .ok_or(Error::Corrupt("outputs offset beyond blob"))?,
    );
    let mut p = off;
    for i in 0..count {
        let (amount, n) =
            varint_at(stripped, p).ok_or(Error::Corrupt("bad output amount in stripped blob"))?;
        out.extend_from_slice(&stripped[p..p + n]);
        p += n;
        if stripped.get(p) != Some(&TAG_KEY_OUTPUT) {
            return Err(Error::Corrupt("bad output tag in stripped blob"));
        }
        out.push(TAG_KEY_OUTPUT);
        p += 1;
        out.extend_from_slice(&key(i, amount)?);
    }
    out.extend_from_slice(&stripped[p..]);
    Ok(out)
}

/// Amounts of the `count` outputs of a stripped blob (without the keys).
pub fn stripped_amounts(stripped: &[u8], off: usize, count: usize) -> Result<Vec<u64>> {
    let mut amounts = Vec::with_capacity(count.min(stripped.len() / 2));
    let mut p = off;
    for _ in 0..count {
        let (amount, n) =
            varint_at(stripped, p).ok_or(Error::Corrupt("bad output amount in stripped blob"))?;
        p += n + 1;
        amounts.push(amount);
    }
    Ok(amounts)
}

/// One transaction of a [`BlockRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TxRecord {
    /// The prefix, keys stripped when `outputs_at != 0`.
    pub prefix: Vec<u8>,
    /// 0 = stored as is; else 1 + offset of the first output in `prefix`.
    pub outputs_at: u32,
    /// Global output indexes, one per output.
    pub global_indexes: Vec<u32>,
    /// Length of the signatures (`blob[prefix_len..]`), stored in the signature stream.
    pub sig_len: u32,
}

/// Everything stored about one main-chain block besides its index rows: the block blob (miner
/// output keys stripped), global indexes, and the transaction prefixes (keys stripped).
/// Signatures live in a separate stream (`HOT_SIGS` / `SIG_PARTS`) so pruning can drop them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockRecord {
    /// Block blob (header, miner tx, tx hashes), miner output keys stripped if `miner_outputs_at != 0`.
    pub blob: Vec<u8>,
    /// 0 = stored as is; else 1 + offset of the miner tx's first output in `blob`.
    pub miner_outputs_at: u32,
    /// Global indexes of the miner tx outputs.
    pub miner_global_indexes: Vec<u32>,
    /// Transactions in `tx_hashes` order.
    pub txs: Vec<TxRecord>,
}

impl BlockRecord {
    /// Serialized form (uncompressed).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.blob.len() + self.txs.iter().map(|t| t.prefix.len() + 16).sum::<usize>() + 32,
        );
        out.push(RECORD_VERSION);
        put_varint(&mut out, self.blob.len() as u64);
        out.extend_from_slice(&self.blob);
        put_varint(&mut out, u64::from(self.miner_outputs_at));
        put_list(&mut out, &self.miner_global_indexes);
        put_varint(&mut out, self.txs.len() as u64);
        for t in &self.txs {
            put_varint(&mut out, t.prefix.len() as u64);
            out.extend_from_slice(&t.prefix);
            put_varint(&mut out, u64::from(t.outputs_at));
            put_list(&mut out, &t.global_indexes);
            put_varint(&mut out, u64::from(t.sig_len));
        }
        out
    }

    /// Reverses [`Self::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Input(bytes);
        if r.u8()? != RECORD_VERSION {
            return Err(Error::Corrupt("unknown block record version"));
        }
        let n = r.usize()?;
        let blob = r.bytes(n)?.to_vec();
        let miner_outputs_at = r.u32()?;
        let miner_global_indexes = r.u32_list()?;
        let n_txs = r.usize()?;
        if n_txs > r.0.len() {
            return Err(Error::Corrupt("more transactions than record bytes"));
        }
        let mut txs = Vec::with_capacity(n_txs);
        for _ in 0..n_txs {
            let n = r.usize()?;
            let prefix = r.bytes(n)?.to_vec();
            let outputs_at = r.u32()?;
            let global_indexes = r.u32_list()?;
            let sig_len = r.u32()?;
            txs.push(TxRecord {
                prefix,
                outputs_at,
                global_indexes,
                sig_len,
            });
        }
        if !r.0.is_empty() {
            return Err(Error::Corrupt("trailing bytes after block record"));
        }
        Ok(Self {
            blob,
            miner_outputs_at,
            miner_global_indexes,
            txs,
        })
    }

    /// Total signature bytes of the block.
    #[must_use]
    pub fn sig_bytes(&self) -> u64 {
        self.txs.iter().map(|t| u64::from(t.sig_len)).sum()
    }

    /// Byte offset of transaction `i`'s signatures within the block's signature bytes.
    #[must_use]
    pub fn sig_offset(&self, i: usize) -> u64 {
        self.txs[..i].iter().map(|t| u64::from(t.sig_len)).sum()
    }

    /// Hash of transaction `tx_index` (1-based) from the tail of the block blob, where
    /// CryptoNote serializes `tx_hashes` (stripping never touches it).
    #[must_use]
    pub fn tx_hash(&self, tx_index: u32) -> Option<[u8; 32]> {
        let n = self.txs.len();
        let i = (tx_index as usize).checked_sub(1)?;
        if i >= n {
            return None;
        }
        let end = self.blob.len().checked_sub(32 * (n - 1 - i))?;
        let start = end.checked_sub(32)?;
        self.blob.get(start..end)?.try_into().ok()
    }
}

fn put_list(out: &mut Vec<u8>, v: &[u32]) {
    put_varint(out, v.len() as u64);
    for x in v {
        put_varint(out, u64::from(*x));
    }
}

/// Serialized chunk body: `version | varint n | varint len[n] | records`.
#[must_use]
pub fn encode_chunk(records: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = records.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total + 4 * records.len() + 8);
    out.push(CHUNK_VERSION);
    put_varint(&mut out, records.len() as u64);
    for r in records {
        put_varint(&mut out, r.len() as u64);
    }
    for r in records {
        out.extend_from_slice(r);
    }
    out
}

/// Splits a chunk body into its (encoded) records.
pub fn decode_chunk(body: &[u8]) -> Result<Vec<&[u8]>> {
    let mut r = Input(body);
    if r.u8()? != CHUNK_VERSION {
        return Err(Error::Corrupt("unknown chunk version"));
    }
    let n = r.usize()?;
    if n > r.0.len() {
        return Err(Error::Corrupt("chunk count larger than chunk"));
    }
    let lens = (0..n).map(|_| r.usize()).collect::<Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(n);
    for len in lens {
        out.push(r.bytes(len)?);
    }
    if !r.0.is_empty() {
        return Err(Error::Corrupt("trailing bytes after chunk"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut b = vec![0xaa];
            put_varint(&mut b, v);
            assert_eq!(b.len() - 1, varint_len(v));
            assert_eq!(varint_at(&b, 1), Some((v, varint_len(v))));
        }
        assert_eq!(varint_at(&[0x80], 0), None);
        assert_eq!(varint_at(&[0xff; 10], 0), None);
    }

    /// A prefix-shaped blob: junk, an outputs section, junk.
    fn blob_with(outputs: &[(u64, [u8; 32])], head: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut b = head.to_vec();
        put_varint(&mut b, outputs.len() as u64);
        for (a, k) in outputs {
            put_varint(&mut b, *a);
            b.push(TAG_KEY_OUTPUT);
            b.extend_from_slice(k);
        }
        b.extend_from_slice(tail);
        b
    }

    #[test]
    fn strip_round_trips() {
        let outputs = vec![
            (5, [1_u8; 32]),
            (300_000, [2_u8; 32]),
            (u64::MAX, [3_u8; 32]),
        ];
        // The first key also appears earlier (as a "key image"): the walk must skip it.
        let mut head = vec![1, 0, 1, 2];
        head.extend_from_slice(&[1_u8; 32]);
        let blob = blob_with(&outputs, &head, &[4, 9, 9, 9, 9]);
        let (off, stripped) = strip_output_keys(&blob, &outputs).unwrap();
        assert_eq!(stripped.len(), blob.len() - 96);
        assert_eq!(
            stripped_amounts(&stripped, off, 3).unwrap(),
            vec![5, 300_000, u64::MAX]
        );
        let back = unstrip_output_keys(&stripped, off, 3, |i, amount| {
            assert_eq!(amount, outputs[i].0);
            Ok(outputs[i].1)
        })
        .unwrap();
        assert_eq!(back, blob);

        // Wrong amount or key: not strippable.
        assert!(strip_output_keys(&blob, &[(6, [1; 32])]).is_none());
        assert!(strip_output_keys(&blob, &[(5, [7; 32])]).is_none());
        assert!(strip_output_keys(&blob, &[]).is_none());
    }

    #[test]
    fn records_and_chunks_round_trip() {
        let rec = BlockRecord {
            blob: (0..200_u8).collect(),
            miner_outputs_at: 77,
            miner_global_indexes: vec![0, 1, 70_000],
            txs: vec![
                TxRecord {
                    prefix: vec![1, 2, 3],
                    outputs_at: 0,
                    global_indexes: vec![],
                    sig_len: 0,
                },
                TxRecord {
                    prefix: vec![9; 1000],
                    outputs_at: 5,
                    global_indexes: vec![u32::MAX, 3],
                    sig_len: 640,
                },
            ],
        };
        let enc = rec.encode();
        assert_eq!(BlockRecord::decode(&enc).unwrap(), rec);
        assert!(BlockRecord::decode(&enc[..enc.len() - 1]).is_err());
        assert_eq!(rec.sig_bytes(), 640);
        assert_eq!(rec.sig_offset(1), 0);

        let empty = BlockRecord::default().encode();
        let body = encode_chunk(&[enc.clone(), empty.clone(), enc.clone()]);
        let parts = decode_chunk(&body).unwrap();
        assert_eq!(parts, vec![&enc[..], &empty[..], &enc[..]]);
        assert!(decode_chunk(&body[..body.len() - 1]).is_err());
    }

    #[test]
    fn tx_hash_from_blob_tail() {
        let mut blob = vec![7; 50];
        blob.push(2);
        blob.extend_from_slice(&[0xa1; 32]);
        blob.extend_from_slice(&[0xa2; 32]);
        let rec = BlockRecord {
            blob,
            txs: vec![TxRecord::default(), TxRecord::default()],
            ..BlockRecord::default()
        };
        assert_eq!(rec.tx_hash(1), Some([0xa1; 32]));
        assert_eq!(rec.tx_hash(2), Some([0xa2; 32]));
        assert_eq!(rec.tx_hash(0), None);
        assert_eq!(rec.tx_hash(3), None);
    }
}
