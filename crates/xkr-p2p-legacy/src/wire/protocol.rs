//! Block-chain protocol notifications: `src/cryptonote_protocol/cryptonote_protocol_definitions.h`
//! plus the free `serialize()` overloads at the top of `cryptonote_protocol_handler.cpp`
//! (several requests have no member `serialize`, their keys live there).
//!
//! All of these are Levin *notifications* (`have_to_return_data = false`); the
//! "responses" (`NOTIFY_RESPONSE_*`) are separate notifications, not Levin responses.

use xkr_epee::epee_object;
use xkr_types::{Hash, RawBlock};

use crate::blob::{HashValue, PodList};
use crate::constants::{MAX_BLOCKS_IN_RESPONSE, MIN_TX_BLOB_WIRE_SIZE};

/// `RawBlockLegacy` / epee `block_complete_entry`: `{ "block": string, "txs": [string] }`.
///
/// C++ unpacks `BinaryArray`s into `std::string`s "to maintain protocol compatibility",
/// so `txs` is an epee *array of strings* (not a blob). Empty `txs` is omitted. Every tx blob
/// must take at least [`MIN_TX_BLOB_WIRE_SIZE`] bytes on the wire (allocation bound).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawBlockLegacy {
    pub block: Vec<u8>,
    pub txs: Vec<Vec<u8>>,
}

epee_object!(
    RawBlockLegacy,
    block: Vec<u8>,
    txs: Vec<Vec<u8>> [min_element: MIN_TX_BLOB_WIRE_SIZE],
);

impl From<RawBlock> for RawBlockLegacy {
    fn from(value: RawBlock) -> Self {
        Self {
            block: value.block,
            txs: value.transactions,
        }
    }
}

impl From<RawBlockLegacy> for RawBlock {
    fn from(value: RawBlockLegacy) -> Self {
        Self {
            block: value.block,
            transactions: value.txs,
        }
    }
}

/// `NOTIFY_NEW_BLOCK` (2001): `{ "b": RawBlockLegacy, "current_blockchain_height": u32, "hop": u32 }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlock {
    pub block: RawBlockLegacy,
    pub current_blockchain_height: u32,
    pub hop: u32,
}

epee_object!(
    NewBlock,
    block("b"): RawBlockLegacy,
    current_blockchain_height: u32,
    hop: u32,
);

/// `NOTIFY_NEW_TRANSACTIONS` (2002): `{ "txs": [string] }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewTransactions {
    pub txs: Vec<Vec<u8>>,
}

epee_object!(
    NewTransactions,
    txs: Vec<Vec<u8>> [min_element: MIN_TX_BLOB_WIRE_SIZE],
);

/// `NOTIFY_REQUEST_GET_OBJECTS` (2003): `{ "txs": blob<hash>, "blocks": blob<hash> }`.
/// The C++ node logs a warning and ignores `txs`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestGetObjects {
    pub txs: Vec<Hash>,
    pub blocks: Vec<Hash>,
}

epee_object!(
    RequestGetObjects,
    txs: Vec<Hash> as PodList<Hash>,
    blocks: Vec<Hash> as PodList<Hash>,
);

/// `NOTIFY_RESPONSE_GET_OBJECTS` (2004):
/// `{ "txs": [string], "blocks": [RawBlockLegacy], "missed_ids": blob<hash>, "current_blockchain_height": u32 }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResponseGetObjects {
    pub txs: Vec<Vec<u8>>,
    pub blocks: Vec<RawBlockLegacy>,
    pub missed_ids: Vec<Hash>,
    pub current_blockchain_height: u32,
}

epee_object!(
    ResponseGetObjects,
    txs: Vec<Vec<u8>> [min_element: MIN_TX_BLOB_WIRE_SIZE],
    blocks: Vec<RawBlockLegacy> [max_len: MAX_BLOCKS_IN_RESPONSE],
    missed_ids: Vec<Hash> as PodList<Hash>,
    current_blockchain_height: u32,
);

/// `NOTIFY_REQUEST_CHAIN` (2006): `{ "block_ids": blob<hash> }`, a sparse chain that must end
/// with the genesis hash (see [`crate::sync::sparse_chain_indices`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestChain {
    pub block_ids: Vec<Hash>,
}

epee_object!(
    RequestChain,
    block_ids: Vec<Hash> as PodList<Hash>,
);

/// `NOTIFY_RESPONSE_CHAIN_ENTRY` (2007):
/// `{ "start_height": u32, "total_height": u32, "m_block_ids": blob<hash> }`.
/// `start_height` is the main-chain index of `block_ids[0]` (a block the requester has).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResponseChainEntry {
    pub start_height: u32,
    pub total_height: u32,
    pub block_ids: Vec<Hash>,
}

epee_object!(
    ResponseChainEntry,
    start_height: u32,
    total_height: u32,
    block_ids("m_block_ids"): Vec<Hash> as PodList<Hash>,
);

/// `NOTIFY_REQUEST_TX_POOL` (2008): `{ "txs": blob<hash> }` — the hashes the sender already
/// has; the receiver answers with `NOTIFY_NEW_TRANSACTIONS` for the rest of its pool.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestTxPool {
    pub txs: Vec<Hash>,
}

epee_object!(
    RequestTxPool,
    txs: Vec<Hash> as PodList<Hash>,
);

/// `NOTIFY_NEW_LITE_BLOCK` (2009):
/// `{ "current_blockchain_height": u32, "hop": u32, "blockTemplate": string }`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewLiteBlock {
    pub current_blockchain_height: u32,
    pub hop: u32,
    pub block_template: Vec<u8>,
}

epee_object!(
    NewLiteBlock,
    current_blockchain_height: u32,
    hop: u32,
    block_template("blockTemplate"): Vec<u8>,
);

/// `NOTIFY_MISSING_TXS` (2010):
/// `{ "current_blockchain_height": u32, "blockHash": hash, "missing_txs": blob<hash> }`.
/// Answered with `NOTIFY_NEW_TRANSACTIONS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTxs {
    pub current_blockchain_height: u32,
    pub block_hash: Hash,
    pub missing_txs: Vec<Hash>,
}

epee_object!(
    MissingTxs,
    current_blockchain_height: u32,
    block_hash("blockHash"): Hash as HashValue,
    missing_txs: Vec<Hash> as PodList<Hash>,
);
