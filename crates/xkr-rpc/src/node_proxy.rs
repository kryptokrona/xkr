//! The two block routes of the C++ `NodeRpcProxy` (`src/node_rpc_proxy/node_rpc_proxy.cpp`)
//! that wallet-backend-js does not call:
//!
//! * `/queryblockslite` (`RpcServer::on_query_blocks_lite`, `Core::queryBlocksLite`): how
//!   kryptokrona-service (walletd) and xkrwallet sync (`NodeRpcProxy::doQueryBlocksLite`, called
//!   from `BlockchainSynchronizer::startBlockchainSync` in
//!   `src/transfers/blockchain_synchronizer.cpp`);
//! * `/get_blocks_hashes_by_timestamps` (`RpcServer::onGetBlocksHashesByTimestamps`,
//!   `Core::getBlockHashesByTimestamps`; `NodeRpcProxy::doGetBlockHashesByTimestamps`, used by
//!   the C++ blockchain explorer).
//!
//! Both are plain JSON like every other C++ route (`jsonMethod` in `src/rpc/rpc_server.cpp`;
//! the proxy sends them with `jsonCommand`). Request and response fields are those of
//! `COMMAND_RPC_QUERY_BLOCKS_LITE` and `COMMAND_RPC_GET_BLOCKS_HASHES_BY_TIMESTAMPS` in
//! `src/rpc/core_rpc_server_commands_definitions.h`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use xkr_consensus::params::{
    BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT, BLOCKS_SYNCHRONIZING_DEFAULT_COUNT,
};
use xkr_serialization::{decode_block, decode_transaction_prefix};
use xkr_storage::Reader;
use xkr_types::{Hash, TransactionPrefix};

use crate::RpcState;
use crate::handlers::{ApiError, STATUS_OK, parse, parse_hash, run, run_bytes, top_index};
use crate::listing::{ByteArray, JsonList, PrefixEntry, Shown};

/// Status of a `/queryblockslite` call that cannot be answered (C++ `on_query_blocks_lite`).
const QUERY_FAILED: &str = "Failed to perform query";
/// Most `blockIds` a `/queryblockslite` call looks at (the rest is ignored). The C++ wallets
/// send a sparse history (`SynchronizationState::getShortHistory`: the last 10 hashes, then
/// exponentially spaced ones, then genesis), about 30 entries at height 3 M.
pub(crate) const MAX_QUERY_BLOCK_IDS: usize = 10_000;
/// Most bytes of JSON in one `/queryblockslite` answer before further full blocks are left for
/// the next call (one block more may follow the last check, and the first block is always
/// written). The C++ wallet continues from the last block it received, so a shorter answer
/// only means one more call.
pub(crate) const MAX_QUERY_BLOCKS_ANSWER_BYTES: usize = 16 * 1024 * 1024;
/// Most hashes one `/get_blocks_hashes_by_timestamps` answer carries (the earliest by
/// timestamp); a caller that needs more asks again from the last timestamp it received.
pub(crate) const MAX_TIMESTAMP_HASHES: usize = 100_000;
/// C++ `roundToMidnight` (`database_blockchain_cache.cpp`) refuses timestamps above
/// `std::numeric_limits<time_t>::max()`.
const TIME_T_MAX: u64 = (1 << 63) - 1;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct QueryBlocksLiteRequest {
    block_ids: Vec<String>,
    timestamp: u64,
}

/// One C++ `BlockShortInfo` (`src/cryptonote_core/icore_definitions.h`), serialized by
/// `serialize(BlockShortInfo &, ISerializer &)` in `src/rpc/rpc_server.cpp` with the dotted
/// `KV_MEMBER` keys. `block` is the block blob (header, miner transaction, transaction hashes)
/// as a JSON array of byte values, the way C++ serializes a `BinaryArray`; it and `txPrefixes`
/// are empty for the hash-only entries before `fullOffset`.
#[derive(Serialize)]
struct ShortInfo<'a> {
    #[serde(rename = "blockShortInfo.blockId")]
    block_id: Shown<'a, Hash>,
    #[serde(rename = "blockShortInfo.block")]
    block: ByteArray<'a>,
    #[serde(rename = "blockShortInfo.txPrefixes")]
    tx_prefixes: Vec<PrefixEntry<'a>>,
}

/// A main-chain block as `/queryblockslite` sends it in full.
struct FullBlock {
    hash: Hash,
    blob: Vec<u8>,
    txs: Vec<(Hash, TransactionPrefix)>,
}

/// `/queryblockslite` (`RpcServer::on_query_blocks_lite`).
///
/// Request `{ blockIds, timestamp }`; answer `{ status, startHeight, currentHeight, fullOffset,
/// items }` as `Core::queryBlocksLite` fills it:
///
/// * `startHeight`: height of the first `blockIds` entry on the main chain (0 for an empty
///   list); none on the main chain gives status `"Failed to perform query"` with zeros and no
///   items (`Core::findBlockchainSupplement` throws);
/// * a `timestamp` of 0 with `startHeight > 0` is replaced by the timestamp of that block;
/// * `fullOffset`: the first block of the timestamp's day (or of the latest earlier day that
///   has blocks; [`Reader::timestamp_lower_bound_height`]), raised to `startHeight`;
/// * `currentHeight`: the top block's height (an index, not a count);
/// * `items`: hash-only entries from `startHeight` up to `fullOffset` (at most
///   `BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT` = 10 000); only when they reach `fullOffset`,
///   full entries (block blob and the prefixes of its transactions, miner transaction
///   excluded) from `fullOffset`, at most `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT` = 100 and
///   [`MAX_QUERY_BLOCKS_ANSWER_BYTES`] of JSON.
///
/// A `blockIds` entry that is not a hash is answered `400` (C++ fails to load the request).
pub async fn query_blocks_lite(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: QueryBlocksLiteRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let known = match req
        .block_ids
        .iter()
        .take(MAX_QUERY_BLOCK_IDS)
        .map(|text| {
            parse_hash(text)
                .ok_or_else(|| ApiError::BadRequest(format!("Failed to parse block id {text}")))
        })
        .collect::<Result<Vec<Hash>, ApiError>>()
    {
        Ok(k) => k,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        query_blocks_lite_answer(s, &known, req.timestamp)
    })
    .await
}

/// The `/queryblockslite` answer as JSON bytes.
fn query_blocks_lite_answer(
    s: &RpcState,
    known: &[Hash],
    mut timestamp: u64,
) -> Result<Vec<u8>, ApiError> {
    let reader = s.chain.reader()?;
    let current = top_index(&reader)?;

    // Core::findBlockchainSupplement: the first known hash on the main chain.
    let start = if known.is_empty() {
        0
    } else {
        let mut found = None;
        for hash in known {
            if let Some(height) = reader.height_of(hash)? {
                found = Some(height);
                break;
            }
        }
        match found {
            Some(h) => h,
            None => return Ok(failed_answer()),
        }
    };

    if start > 0 && timestamp == 0 && start <= current {
        timestamp = reader
            .block_info(start)?
            .ok_or_else(|| ApiError::Internal(format!("no block at {start}")))?
            .timestamp;
    }
    if timestamp > TIME_T_MAX {
        return Ok(failed_answer());
    }
    let full_offset = reader.timestamp_lower_bound_height(timestamp)?.max(start);

    let mut list = JsonList::new(b"{\"items\":", MAX_QUERY_BLOCKS_ANSWER_BYTES);

    // Core::pushBlockHashes
    let hash_count = (full_offset - start).min(BLOCKS_IDS_SYNCHRONIZING_DEFAULT_COUNT as u64);
    let mut pushed = 0_u64;
    for hash in reader.block_hashes(start, hash_count)? {
        let entry = ShortInfo {
            block_id: Shown(&hash),
            block: ByteArray(&[]),
            tx_prefixes: Vec::new(),
        };
        if !list.push(&entry) {
            break;
        }
        pushed += 1;
    }

    // Core::fillQueryBlockShortInfo, only when the hashes reached `fullOffset`.
    if start + pushed == full_offset {
        let count = BLOCKS_SYNCHRONIZING_DEFAULT_COUNT.min(current.saturating_sub(full_offset) + 1);
        for height in full_offset..full_offset + count {
            let block = full_block(s, &reader, height)?;
            let entry = ShortInfo {
                block_id: Shown(&block.hash),
                block: ByteArray(&block.blob),
                tx_prefixes: block
                    .txs
                    .iter()
                    .map(|(hash, prefix)| PrefixEntry::new(hash, prefix))
                    .collect(),
            };
            if !list.push(&entry) {
                break;
            }
        }
    }

    let suffix = format!(
        ",\"currentHeight\":{current},\"fullOffset\":{full_offset},\"startHeight\":{start},\"status\":\"{STATUS_OK}\"}}"
    );
    Ok(list.finish(suffix.as_bytes()))
}

/// The answer of a query `Core::queryBlocksLite` refuses: the response fields keep their zero
/// values.
fn failed_answer() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "status": QUERY_FAILED,
        "startHeight": 0,
        "currentHeight": 0,
        "fullOffset": 0,
        "items": [],
    }))
    .unwrap_or_default()
}

/// The main-chain block at `height` with the hashes and prefixes of its transactions (miner
/// transaction excluded; it is part of the blob). Prefixes stay readable on a pruned node.
fn full_block(s: &RpcState, reader: &Reader, height: u64) -> Result<FullBlock, ApiError> {
    let info = reader
        .block_info(height)?
        .ok_or_else(|| ApiError::Internal(format!("no block at {height}")))?;
    let blob = s
        .chain
        .with_compressor(|c| reader.block_blob(height, c))??
        .ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
    let block = decode_block(&blob)?;
    let count = u32::try_from(block.tx_hashes.len())
        .map_err(|_| ApiError::Internal("too many transactions".into()))?;
    let prefixes = s.chain.with_compressor(|c| {
        (1..=count)
            .map(|i| reader.tx_prefix_at(height, i, c))
            .collect::<Result<Vec<_>, _>>()
    })??;
    let mut txs = Vec::with_capacity(prefixes.len());
    for (i, (hash, prefix)) in block.tx_hashes.iter().zip(prefixes).enumerate() {
        let prefix = prefix.ok_or_else(|| {
            ApiError::Internal(format!("no transaction {} in block {height}", i + 1))
        })?;
        txs.push((*hash, decode_transaction_prefix(&prefix)?));
    }
    Ok(FullBlock {
        hash: Hash(info.hash),
        blob,
        txs,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct HashesByTimestampsRequest {
    timestamp_begin: u64,
    seconds_count: u64,
}

/// `/get_blocks_hashes_by_timestamps` (`RpcServer::onGetBlocksHashesByTimestamps`).
///
/// Request `{ timestampBegin, secondsCount }`; answer `{ status, blockHashes }`: the main-chain
/// blocks with a timestamp in `timestampBegin .. timestampBegin + secondsCount` (end
/// exclusive), ordered by timestamp and, within one timestamp, by height, at most
/// [`MAX_TIMESTAMP_HASHES`]. `secondsCount` 0 gives an empty list. A range whose end does not
/// fit in 64 bits is answered `"Error: Timestamp overflow"` with no hashes, as C++
/// (`Core::getBlockHashesByTimestamps` throws, the handler prefixes `"Error: "`).
pub async fn get_blocks_hashes_by_timestamps(
    State(s): State<Arc<RpcState>>,
    body: Bytes,
) -> Response {
    let req: HashesByTimestampsRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let Some(end) = req.timestamp_begin.checked_add(req.seconds_count) else {
            return Ok(json!({ "status": "Error: Timestamp overflow", "blockHashes": [] }));
        };
        let reader = s.chain.reader()?;
        let heights =
            reader.heights_by_timestamps(req.timestamp_begin, end, MAX_TIMESTAMP_HASHES)?;
        let mut hashes = Vec::with_capacity(heights.len());
        // C++ never puts the genesis block into its timestamp index
        // (`DatabaseBlockchainCache::addGenesisBlock`), so it is never listed.
        for height in heights.into_iter().filter(|&h| h != 0) {
            let hash = reader
                .block_hash(height)?
                .ok_or_else(|| ApiError::Internal(format!("no block at {height}")))?;
            hashes.push(hash.to_string());
        }
        Ok(json!({ "status": STATUS_OK, "blockHashes": hashes }))
    })
    .await
}
