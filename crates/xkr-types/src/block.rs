//! Block object model, field-for-field the C++ `cryptonote.h` structs
//! (`BlockHeader`, `ParentBlock`, `BlockTemplate`, `RawBlock`).

use crate::keys::Hash;
use crate::transaction::Transaction;

/// C++ `BlockHeader`.
///
/// For major version 1 all five fields are serialized in the block header. For major
/// version >= 2 only `major_version`, `minor_version` and `prev_hash` are in the header; `timestamp`
/// and `nonce` are serialized inside the merge-mining parent block (C++ `ParentBlockSerializer`
/// takes references to `BlockTemplate::timestamp` / `nonce`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHeader {
    /// Block major version (1..=5 on mainnet today; 6 after fork 13).
    pub major_version: u8,
    /// Block minor version (used for upgrade voting / miner signalling).
    pub minor_version: u8,
    /// Unix timestamp.
    pub timestamp: u64,
    /// Hash of the previous block.
    pub prev_hash: Hash,
    /// PoW nonce (serialized as 4 raw little-endian bytes).
    pub nonce: u32,
}

/// C++ `ParentBlock`: the merge-mining parent header carried by every block with
/// major version >= 2. On Kryptokrona it is a mostly-empty shell, but it is part of the
/// hashing blob and the PoW blob, so it must round-trip exactly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParentBlock {
    /// Parent block major version.
    pub major_version: u8,
    /// Parent block minor version.
    pub minor_version: u8,
    /// Parent block previous hash.
    pub prev_hash: Hash,
    /// Number of transactions in the parent block (varint on the wire, truncated to `u16`
    /// exactly like C++).
    pub transaction_count: u16,
    /// Merkle branch from the parent miner transaction to the parent tx-tree root;
    /// length is `tree_depth(transaction_count)`.
    pub base_transaction_branch: Vec<Hash>,
    /// Parent miner transaction (C++ `BaseTransaction`: a prefix without signatures, so
    /// `signatures` is always empty here and is ignored by the serializer).
    pub base_transaction: Transaction,
    /// Merkle branch for the aux (merge-mined) chain; length equals the merge-mining tag
    /// depth found in `base_transaction.prefix.extra`.
    pub blockchain_branch: Vec<Hash>,
}

/// C++ `BlockTemplate`: a full block (header + optional parent block + miner tx + tx hashes).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Block {
    /// Header. For major >= 2, `timestamp` and `nonce` are carried by the parent-block blob.
    pub header: BlockHeader,
    /// Merge-mining parent block; `Some` iff `header.major_version >= 2`.
    pub parent_block: Option<ParentBlock>,
    /// Coinbase transaction.
    pub miner_tx: Transaction,
    /// Hashes of the non-coinbase transactions, in block order.
    pub tx_hashes: Vec<Hash>,
}

impl Block {
    /// Block height claimed by the coinbase input, or 0 if the miner transaction is not a
    /// single coinbase input (C++ `CachedBlock::getBlockIndex`).
    #[must_use]
    pub fn height(&self) -> u32 {
        match self.miner_tx.prefix.inputs.as_slice() {
            [crate::transaction::TxInput::Coinbase { height }] => *height,
            _ => 0,
        }
    }
}

/// C++ `RawBlock`: a serialized block plus its serialized transactions, as relayed over P2P
/// and stored by the C++ node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawBlock {
    /// Serialized `BlockTemplate`.
    pub block: Vec<u8>,
    /// Serialized transactions, in `tx_hashes` order.
    pub transactions: Vec<Vec<u8>>,
}
