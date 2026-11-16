//! Block (de)serialization: C++ `serializeBlockHeader`, `serialize(BlockTemplate&)`,
//! `serialize(ParentBlockSerializer&)` and `serialize(RawBlock&)` in
//! `src/cryptonote_core/cryptonote_serialization.cpp`.

use xkr_crypto::tree_hash::{tree_depth, tree_hash_from_branch};
use xkr_types::{Block, BlockHeader, ParentBlock, RawBlock, Transaction, merge_mining_tag};

use crate::error::{DecodeError, EncodeError};
use crate::hashing::base_transaction_hash;
use crate::reader::Reader;
use crate::tx::{encode_base_transaction, read_prefix, read_transaction, write_transaction};
use crate::varint::write_varint;

/// Highest block major version the deserializer accepts: 7 (fork 14, `docs/tx-v2.md`; 6 is
/// fork 13, `docs/forks.md`; C++ v1.2.2 and older stop at `BLOCK_MAJOR_VERSION_5`, the patched
/// C++ v1.2 accepts 6). Whether a version is valid at a height is consensus (`xkr-consensus`
/// fork table), not decoding.
pub const MAX_BLOCK_MAJOR_VERSION: u8 = 7;

/// Which parent-block serialization `makeParentBlockSerializer(block, hashing, headerOnly)`
/// produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentBlockMode {
    /// `hashing = false, headerOnly = false`: the wire format inside a block blob.
    Wire,
    /// `hashing = true, headerOnly = false`: appended to the hashing blob for the block id.
    Hashing,
    /// `hashing = true, headerOnly = true`: the proof-of-work blob for major >= 2.
    HashingHeaderOnly,
    /// `hashing = false, headerOnly = true` (`getParentBlockBinaryArray(true)`).
    HeaderOnly,
}

impl ParentBlockMode {
    fn hashing(self) -> bool {
        matches!(self, Self::Hashing | Self::HashingHeaderOnly)
    }

    fn header_only(self) -> bool {
        matches!(self, Self::HashingHeaderOnly | Self::HeaderOnly)
    }
}

/// Writes C++ `serializeBlockHeader`: major, minor, then for major 1 timestamp, prev hash and
/// nonce; for major >= 2 only the prev hash.
pub(crate) fn write_block_header(out: &mut Vec<u8>, h: &BlockHeader) -> Result<(), EncodeError> {
    if h.major_version > MAX_BLOCK_MAJOR_VERSION || h.major_version == 0 {
        return Err(EncodeError::WrongBlockMajorVersion(h.major_version));
    }
    write_varint(out, u64::from(h.major_version));
    write_varint(out, u64::from(h.minor_version));
    if h.major_version == 1 {
        write_varint(out, h.timestamp);
        out.extend_from_slice(&h.prev_hash.0);
        out.extend_from_slice(&h.nonce.to_le_bytes());
    } else {
        out.extend_from_slice(&h.prev_hash.0);
    }
    Ok(())
}

/// C++ `toBinaryArray(static_cast<const BlockHeader&>(block))`.
pub fn encode_block_header(header: &BlockHeader) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(48);
    write_block_header(&mut out, header)?;
    Ok(out)
}

/// C++ `getParentBlockBinaryArray` / `getParentBlockHashingBinaryArray`: serializes the parent
/// block of `block` (which must have major >= 2) in the given mode.
pub fn encode_parent_block(block: &Block, mode: ParentBlockMode) -> Result<Vec<u8>, EncodeError> {
    let pb = block
        .parent_block
        .as_ref()
        .ok_or(EncodeError::MissingParentBlock(block.header.major_version))?;
    let mut out = Vec::with_capacity(160);
    write_parent_block(&mut out, pb, &block.header, mode)?;
    Ok(out)
}

/// C++ `serialize(ParentBlockSerializer&)` on an output serializer.
pub(crate) fn write_parent_block(
    out: &mut Vec<u8>,
    pb: &ParentBlock,
    header: &BlockHeader,
    mode: ParentBlockMode,
) -> Result<(), EncodeError> {
    write_varint(out, u64::from(pb.major_version));
    write_varint(out, u64::from(pb.minor_version));
    write_varint(out, header.timestamp);
    out.extend_from_slice(&pb.prev_hash.0);
    out.extend_from_slice(&header.nonce.to_le_bytes());

    if mode.hashing() {
        let miner_tx_hash = base_transaction_hash(&pb.base_transaction.prefix);
        let merkle_root = tree_hash_from_branch(&pb.base_transaction_branch, &miner_tx_hash, None);
        out.extend_from_slice(&merkle_root.0);
    }

    write_varint(out, u64::from(pb.transaction_count));
    if pb.transaction_count < 1 {
        return Err(EncodeError::ZeroParentTransactionCount);
    }
    if mode.header_only() {
        return Ok(());
    }

    let branch_size = tree_depth(usize::from(pb.transaction_count));
    if pb.base_transaction_branch.len() != branch_size {
        return Err(EncodeError::BaseTransactionBranchLength {
            expected: branch_size,
            got: pb.base_transaction_branch.len(),
        });
    }
    for h in &pb.base_transaction_branch {
        out.extend_from_slice(&h.0);
    }

    out.extend_from_slice(&encode_base_transaction(&pb.base_transaction.prefix));

    let tag = merge_mining_tag(&pb.base_transaction.prefix.extra)
        .ok_or(EncodeError::MissingMergeMiningTag)?;
    if tag.depth > 256 {
        return Err(EncodeError::MergeMiningDepthTooLarge(tag.depth));
    }
    if tag.depth != pb.blockchain_branch.len() as u64 {
        return Err(EncodeError::BlockchainBranchLength {
            expected: tag.depth,
            got: pb.blockchain_branch.len(),
        });
    }
    for h in &pb.blockchain_branch {
        out.extend_from_slice(&h.0);
    }
    Ok(())
}

/// C++ `serialize(ParentBlockSerializer&)` on an input serializer (wire format). Writes the
/// block's `timestamp` and `nonce` into `header`.
fn read_parent_block(
    r: &mut Reader<'_>,
    header: &mut BlockHeader,
) -> Result<ParentBlock, DecodeError> {
    let major_version = r.varint_u8("parent block major version")?;
    let minor_version = r.varint_u8("parent block minor version")?;
    header.timestamp = r.varint_u64("timestamp")?;
    let prev_hash = r.hash("parent block prev hash")?;
    header.nonce = r.u32_le("nonce")?;

    // Read as uint64_t, then truncated to uint16_t exactly like C++.
    let transaction_count = r.varint_u64("parent block transaction count")? as u16;
    if transaction_count < 1 {
        return Err(DecodeError::ZeroParentTransactionCount);
    }

    let branch_size = tree_depth(usize::from(transaction_count));
    let mut base_transaction_branch = Vec::with_capacity(branch_size);
    for _ in 0..branch_size {
        base_transaction_branch.push(r.hash("parent miner tx branch")?);
    }

    let prefix = read_prefix(r, true)?;
    let tag = merge_mining_tag(&prefix.extra).ok_or(DecodeError::MissingMergeMiningTag)?;
    if tag.depth > 256 {
        return Err(DecodeError::MergeMiningDepthTooLarge(tag.depth));
    }
    let depth = tag.depth as usize;
    let mut blockchain_branch = Vec::with_capacity(depth);
    for _ in 0..depth {
        blockchain_branch.push(r.hash("blockchain branch")?);
    }

    Ok(ParentBlock {
        major_version,
        minor_version,
        prev_hash,
        transaction_count,
        base_transaction_branch,
        base_transaction: Transaction {
            prefix,
            signatures: Vec::new(),
            clsag_signatures: Vec::new(),
        },
        blockchain_branch,
    })
}

fn read_block(r: &mut Reader<'_>) -> Result<Block, DecodeError> {
    let mut header = BlockHeader {
        major_version: r.varint_u8("major_version")?,
        ..BlockHeader::default()
    };
    if header.major_version > MAX_BLOCK_MAJOR_VERSION {
        return Err(DecodeError::WrongBlockMajorVersion(header.major_version));
    }
    header.minor_version = r.varint_u8("minor_version")?;
    let parent_block = match header.major_version {
        1 => {
            header.timestamp = r.varint_u64("timestamp")?;
            header.prev_hash = r.hash("prev_id")?;
            header.nonce = r.u32_le("nonce")?;
            None
        }
        0 => return Err(DecodeError::WrongBlockMajorVersion(0)),
        _ => {
            header.prev_hash = r.hash("prev_id")?;
            Some(read_parent_block(r, &mut header)?)
        }
    };
    let miner_tx = read_transaction(r)?;
    let n = r.count(32, "tx_hashes")?;
    let mut tx_hashes = Vec::with_capacity(n);
    for _ in 0..n {
        tx_hashes.push(r.hash("tx hash")?);
    }
    Ok(Block {
        header,
        parent_block,
        miner_tx,
        tx_hashes,
    })
}

/// C++ `fromBinaryArray<BlockTemplate>`: decodes a block blob (all bytes consumed).
pub fn decode_block(blob: &[u8]) -> Result<Block, DecodeError> {
    let mut r = Reader::new(blob);
    let b = read_block(&mut r)?;
    r.finish("block")?;
    Ok(b)
}

/// C++ `toBinaryArray(BlockTemplate)`. For major version 1 a `parent_block` is ignored (as in
/// C++); for major >= 2 it is required.
pub fn encode_block(block: &Block) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(512);
    write_block_header(&mut out, &block.header)?;
    if block.header.major_version >= 2 {
        let pb = block
            .parent_block
            .as_ref()
            .ok_or(EncodeError::MissingParentBlock(block.header.major_version))?;
        write_parent_block(&mut out, pb, &block.header, ParentBlockMode::Wire)?;
    }
    write_transaction(&mut out, &block.miner_tx)?;
    write_varint(&mut out, block.tx_hashes.len() as u64);
    for h in &block.tx_hashes {
        out.extend_from_slice(&h.0);
    }
    Ok(out)
}

/// C++ `serialize(RawBlock&)` on an input serializer: `varint block_size, block bytes,
/// varint tx_count, (varint tx_size, tx bytes)*`.
pub fn decode_raw_block(blob: &[u8]) -> Result<RawBlock, DecodeError> {
    let mut r = Reader::new(blob);
    let n = r.count(1, "block_size")?;
    let block = r.bytes(n, "block")?.to_vec();
    let count = r.count(1, "tx_count")?;
    let mut transactions = Vec::with_capacity(count);
    for _ in 0..count {
        let n = r.count(1, "tx_size")?;
        transactions.push(r.bytes(n, "transaction")?.to_vec());
    }
    r.finish("raw block")?;
    Ok(RawBlock {
        block,
        transactions,
    })
}

/// C++ `serialize(RawBlock&)` on an output serializer.
#[must_use]
pub fn encode_raw_block(raw: &RawBlock) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        raw.block.len() + 16 + raw.transactions.iter().map(Vec::len).sum::<usize>(),
    );
    write_varint(&mut out, raw.block.len() as u64);
    out.extend_from_slice(&raw.block);
    write_varint(&mut out, raw.transactions.len() as u64);
    for t in &raw.transactions {
        write_varint(&mut out, t.len() as u64);
        out.extend_from_slice(t);
    }
    out
}
