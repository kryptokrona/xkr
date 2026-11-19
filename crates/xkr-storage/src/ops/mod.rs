//! Batch write operations and read functions over the [`crate::schema`] tables.
//!
//! Writes take a `&redb::WriteTransaction` so the caller decides the transaction scope:
//! the sync pipeline does exactly one write transaction per verified batch
//! (`push_blocks` + commit). Reads go through [`read::Reader`], a set of read-only tables
//! opened once from a read transaction (an MVCC snapshot).

pub(crate) mod chunk;
pub mod read;
pub mod write;

use xkr_types::{Block, Hash, RawBlock, Transaction};

pub use read::{BlockPrefixes, Reader, TxGlobalIndexes};
pub use write::{
    pop_blocks, prune_all_below, prune_alt_blocks_below, prune_stripe, push_blocks, put_alt_block,
    remove_alt_block,
};

/// A block that consensus has fully verified, with everything storage needs precomputed
/// by the node (hashes, sizes, difficulty), so this crate never hashes or decodes.
#[derive(Debug, Clone)]
pub struct VerifiedBlock {
    pub height: u64,
    /// Block id.
    pub hash: Hash,
    /// Decoded block (header timestamp, miner tx outputs, tx hash order).
    pub block: Block,
    /// The serialized block exactly as on the wire (header + miner tx + tx hashes).
    pub block_blob: Vec<u8>,
    /// Hash of `block.miner_tx`.
    pub miner_tx_hash: Hash,
    /// Difficulty the block was checked against.
    pub difficulty: u64,
    /// `alreadyGeneratedCoins` after this block (emission, excluding fees).
    pub already_generated_coins: u64,
    /// CryptoNote cumulative block size (miner tx + txs).
    pub block_size: u64,
    /// Transactions in `block.tx_hashes` order.
    pub txs: Vec<VerifiedTx>,
}

/// A verified non-coinbase transaction of a [`VerifiedBlock`].
#[derive(Debug, Clone)]
pub struct VerifiedTx {
    pub hash: Hash,
    /// Decoded transaction (inputs for key images, outputs, unlock time).
    pub tx: Transaction,
    /// Full serialized transaction.
    pub blob: Vec<u8>,
    /// `blob[..prefix_len]` is the `TransactionPrefix`, the rest the signatures.
    pub prefix_len: usize,
    /// Payment id from `tx_extra` (nonce tag `0x00`), if any.
    pub payment_id: Option<[u8; 32]>,
}

/// Packs `u32` global indexes little-endian.
#[must_use]
pub fn pack_u32s(v: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Reverses [`pack_u32s`] (a trailing partial chunk is ignored).
#[must_use]
pub fn unpack_u32s(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect()
}

/// Length-prefixed encoding of a [`RawBlock`] (alt-block storage):
/// `u32 len | block | u32 n | (u32 len | tx)*`, all little-endian.
#[must_use]
pub fn encode_raw_block(raw: &RawBlock) -> Vec<u8> {
    let size = 8 + raw.block.len() + raw.transactions.iter().map(|t| 4 + t.len()).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(raw.block.len() as u32).to_le_bytes());
    out.extend_from_slice(&raw.block);
    out.extend_from_slice(&(raw.transactions.len() as u32).to_le_bytes());
    for t in &raw.transactions {
        out.extend_from_slice(&(t.len() as u32).to_le_bytes());
        out.extend_from_slice(t);
    }
    out
}

/// Reverses [`encode_raw_block`].
#[must_use]
pub fn decode_raw_block(mut b: &[u8]) -> Option<RawBlock> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        if b.len() < n {
            return None;
        }
        let (head, tail) = b.split_at(n);
        *b = tail;
        Some(head)
    }
    fn take_u32(b: &mut &[u8]) -> Option<usize> {
        take(b, 4).map(|s| u32::from_le_bytes(s.try_into().expect("4 bytes")) as usize)
    }
    let len = take_u32(&mut b)?;
    let block = take(&mut b, len)?.to_vec();
    let n = take_u32(&mut b)?;
    let mut transactions = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let len = take_u32(&mut b)?;
        transactions.push(take(&mut b, len)?.to_vec());
    }
    b.is_empty().then_some(RawBlock {
        block,
        transactions,
    })
}
