//! `getblocktemplate` and `submitblock` (`RpcServer::on_getblocktemplate`, `on_submitblock`).
//!
//! Request and response shapes are the C++ ones the bundled `miner`, pools and p2pool use:
//!
//! * `getblocktemplate` `{ "wallet_address": "SEKR…", "reserve_size": n }` ->
//!   `{ "blocktemplate_blob", "difficulty", "height", "reserved_offset", "status": "OK" }`;
//! * `submitblock` `["<block blob hex>"]` -> `{ "status": "OK" }` or JSON-RPC error -7
//!   "Block not accepted";
//! * `get_miner_data` (no params, what p2pool polls) -> `{ "status", "major_version", "height",
//!   "prev_id", "seed_hash", "difficulty", "median_weight", "already_generated_coins",
//!   "median_timestamp", "unlock_window", "tx_backlog": [{ "id", "weight", "fee" }] }`
//!   (`on_get_miner_data` of the C++ daemon: `difficulty` as a hex string without `0x`,
//!   `seed_hash` 64 zeros because CryptoNight has no seed);
//! * `get_block_headers_range` `{ "start_height", "end_height" }` (inclusive, at most 1 000
//!   blocks) -> `{ "status", "headers": [block_header], "untrusted": false }` with the C++
//!   "lightweight" header (`num_txes` = hashes + 1, `block_size` = block blob size).

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use xkr_chain::{AddBlockOutcome, AddBlockResult, Chain, MinerAddress, MinerDataError};
use xkr_serialization::decode_block;
use xkr_types::extra::TX_EXTRA_NONCE_MAX_COUNT;
use xkr_types::{Hash, RawBlock};
use xkr_wallet_core::Address;

use crate::explorer::MAX_POOL_LIST_TXS;
use crate::handlers::{ApiError, STATUS_OK, top_index};
use crate::jsonrpc::RpcError;
use crate::{RpcCommand, RpcState};

/// `core_rpc_server_error_codes.h`.
pub(crate) const CORE_RPC_ERROR_CODE_WRONG_PARAM: i64 = -1;
pub(crate) const CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT: i64 = -2;
pub(crate) const CORE_RPC_ERROR_CODE_TOO_BIG_RESERVE_SIZE: i64 = -3;
pub(crate) const CORE_RPC_ERROR_CODE_WRONG_WALLET_ADDRESS: i64 = -4;
pub(crate) const CORE_RPC_ERROR_CODE_INTERNAL_ERROR: i64 = -5;
pub(crate) const CORE_RPC_ERROR_CODE_WRONG_BLOCKBLOB: i64 = -6;
pub(crate) const CORE_RPC_ERROR_CODE_BLOCK_NOT_ACCEPTED: i64 = -7;

/// Parses a mining address the way C++ does (`Currency::parseAccountAddressString`: `SEKR` or
/// `Xkr` prefix, standard addresses only).
#[must_use]
pub fn parse_miner_address(text: &str) -> Option<MinerAddress> {
    let a = Address::parse(text).ok()?;
    if a.payment_id.is_some() {
        return None;
    }
    Some(MinerAddress {
        spend_public: a.spend,
        view_public: a.view,
    })
}

/// `getblocktemplate`.
pub(crate) fn get_block_template(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let reserve_size = params
        .get("reserve_size")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if reserve_size > TX_EXTRA_NONCE_MAX_COUNT as u64 {
        return Err(RpcError::new(
            CORE_RPC_ERROR_CODE_TOO_BIG_RESERVE_SIZE,
            "To big reserved size, maximum 255",
        ));
    }
    let address = params
        .get("wallet_address")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let miner = parse_miner_address(address).ok_or_else(|| {
        RpcError::new(
            CORE_RPC_ERROR_CODE_WRONG_WALLET_ADDRESS,
            "Failed to parse wallet address",
        )
    })?;
    let nonce = vec![0u8; reserve_size as usize];
    let t = s.chain.block_template(&miner, &nonce).map_err(|e| {
        if matches!(&e, xkr_chain::TemplateError::Chain(c) if c.is_closed()) {
            tracing::debug!(error = %e, "no block template, the node is stopping");
        } else {
            tracing::error!(error = %e, "failed to create block template");
        }
        RpcError::new(
            CORE_RPC_ERROR_CODE_INTERNAL_ERROR,
            "Internal error: failed to create block template",
        )
    })?;
    Ok(json!({
        "blocktemplate_blob": hex::encode(&t.blob),
        "difficulty": t.difficulty,
        "height": t.height,
        "reserved_offset": t.reserved_offset,
        "status": STATUS_OK,
    }))
}

/// Hands a mined block to the P2P manager (chain thread + relay) and waits for the result.
/// Falls back to adding it directly when the P2P layer is not running.
pub(crate) fn submit_raw_block(s: &RpcState, block: RawBlock) -> Result<AddBlockResult, String> {
    let (tx, rx) = oneshot::channel();
    match s
        .commands
        .send(RpcCommand::SubmitBlock { block, reply: tx })
    {
        Ok(()) => rx
            .blocking_recv()
            .map_err(|_| "the node is shutting down".to_owned())?,
        Err(e) => {
            let RpcCommand::SubmitBlock { block, .. } = e.0 else {
                return Err("the node is shutting down".to_owned());
            };
            s.chain.add_block(block).map_err(|e| e.to_string())
        }
    }
}

/// The async form of the `submitblock` path for in-process miners (the built-in miner, the
/// embedded p2pool): hands a mined block to the P2P manager (chain thread, then relay with
/// hop 0 if it changed the main chain) and waits for the result. Adds it directly when the
/// P2P layer is not running. Build `block` with [`Chain::assemble_mined_block`].
pub async fn submit_mined_block(
    chain: &Chain,
    commands: &mpsc::UnboundedSender<RpcCommand>,
    block: RawBlock,
) -> Result<AddBlockResult, String> {
    let (tx, rx) = oneshot::channel();
    match commands.send(RpcCommand::SubmitBlock { block, reply: tx }) {
        Ok(()) => rx
            .await
            .map_err(|_| "the node is shutting down".to_owned())?,
        Err(e) => match e.0 {
            RpcCommand::SubmitBlock { block, .. } => {
                chain.add_block(block).map_err(|e| e.to_string())
            }
            RpcCommand::RelayTransactions(_) => Err("the node is shutting down".to_owned()),
        },
    }
}

/// `submitblock`.
pub(crate) fn submit_block(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let hex_blob = params
        .as_array()
        .filter(|a| a.len() == 1)
        .and_then(|a| a[0].as_str())
        .ok_or_else(|| RpcError::new(CORE_RPC_ERROR_CODE_WRONG_PARAM, "Wrong param"))?;
    let blob = hex::decode(hex_blob.trim())
        .map_err(|_| RpcError::new(CORE_RPC_ERROR_CODE_WRONG_BLOCKBLOB, "Wrong block blob"))?;
    let not_accepted =
        || RpcError::new(CORE_RPC_ERROR_CODE_BLOCK_NOT_ACCEPTED, "Block not accepted");
    let raw = s.chain.assemble_mined_block(blob).map_err(|e| {
        tracing::info!(error = %e, "submitted block refused");
        not_accepted()
    })?;
    let result = submit_raw_block(s, raw).map_err(|e| {
        tracing::error!(error = %e, "submitted block could not be stored");
        RpcError::new(
            CORE_RPC_ERROR_CODE_INTERNAL_ERROR,
            format!("Internal error: {e}"),
        )
    })?;
    match result.outcome {
        // C++ AddBlockErrorCondition::BLOCK_ADDED
        AddBlockOutcome::AddedToMain
        | AddBlockOutcome::AddedAndSwitched
        | AddBlockOutcome::AddedToAlternative
        | AddBlockOutcome::AlreadyExists => {
            tracing::info!(hash = %result.hash, height = ?result.height, outcome = ?result.outcome, "submitted block accepted");
            Ok(json!({ "status": STATUS_OK }))
        }
        AddBlockOutcome::Orphan | AddBlockOutcome::Invalid(_) => {
            tracing::info!(hash = %result.hash, outcome = ?result.outcome, "submitted block refused");
            Err(not_accepted())
        }
    }
}

/// Most headers per `get_block_headers_range` call (C++ `MAX_BLOCK_HEADERS_RANGE`).
pub(crate) const MAX_BLOCK_HEADERS_RANGE: u64 = 1_000;

/// `get_miner_data` (`Core::getMinerDataBlockPart` + `getMinerTxBacklog`): the fields of
/// [`xkr_chain::Chain::miner_data`] (one storage snapshot). Unlike C++ (whole pool in pool
/// order) the backlog is exactly what `getblocktemplate` would select for the next block
/// ([`xkr_chain::Chain::template_backlog`]: template rules, priority order, deterministic so
/// p2pool's "response unchanged" check holds), capped at [`MAX_POOL_LIST_TXS`].
pub(crate) fn get_miner_data(s: &RpcState) -> Result<Value, RpcError> {
    let failed = || {
        RpcError::new(
            CORE_RPC_ERROR_CODE_INTERNAL_ERROR,
            "Internal error: failed to gather miner data",
        )
    };
    let data = s.chain.miner_data().map_err(|e| match e {
        MinerDataError::Chain(e) => RpcError::from(ApiError::from(e)),
        MinerDataError::MissingTop(_) => failed(),
        MinerDataError::DifficultyOverhead => {
            tracing::error!("get_miner_data: difficulty overhead");
            failed()
        }
    })?;
    let tx_backlog: Vec<Value> = data
        .tx_backlog
        .iter()
        .take(MAX_POOL_LIST_TXS)
        .map(|t| json!({ "id": t.hash.to_string(), "weight": t.size, "fee": t.fee }))
        .collect();
    Ok(json!({
        "status": STATUS_OK,
        "major_version": data.major_version,
        "height": data.height,
        "prev_id": data.prev_id.to_string(),
        "seed_hash": Hash::ZERO.to_string(),
        "difficulty": format!("{:x}", data.difficulty),
        "median_weight": data.median_weight,
        "already_generated_coins": data.already_generated_coins,
        "median_timestamp": data.median_timestamp,
        "unlock_window": data.unlock_window,
        "tx_backlog": tx_backlog,
    }))
}

/// `get_block_headers_range` (`on_get_block_headers_range`): main-chain headers
/// `start_height..=end_height`, both at most the top index.
pub(crate) fn block_headers_range(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let start = params
        .get("start_height")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let end = params
        .get("end_height")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let chain = &s.chain;
    let reader = chain.reader().map_err(ApiError::from)?;
    let top = top_index(&reader)?;
    if start > top || end > top || start > end {
        return Err(RpcError::new(
            CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT,
            "Invalid start/end heights.",
        ));
    }
    if end - start + 1 > MAX_BLOCK_HEADERS_RANGE {
        return Err(RpcError::new(
            CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT,
            format!(
                "Requested block header range is too large (max {MAX_BLOCK_HEADERS_RANGE} blocks)."
            ),
        ));
    }
    let infos = reader.block_infos(start, end + 1).map_err(ApiError::from)?;
    let mut headers = Vec::with_capacity(infos.len());
    for (info, height) in infos.iter().zip(start..) {
        let blob = chain
            .with_compressor(|c| reader.block_blob(height, c))
            .map_err(ApiError::from)?
            .map_err(ApiError::from)?
            .ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
        let block = decode_block(&blob).map_err(ApiError::from)?;
        let reward = block
            .miner_tx
            .prefix
            .outputs
            .iter()
            .map(|o| o.amount)
            .fold(0u64, u64::wrapping_add);
        headers.push(json!({
            "major_version": block.header.major_version,
            "minor_version": block.header.minor_version,
            "timestamp": block.header.timestamp,
            "prev_hash": block.header.prev_hash.to_string(),
            "nonce": block.header.nonce,
            "orphan_status": false,
            "height": height,
            "depth": top - height,
            "hash": Hash(info.hash).to_string(),
            "difficulty": info.difficulty,
            "reward": reward,
            "num_txes": block.tx_hashes.len() + 1,
            "block_size": blob.len(),
        }));
    }
    Ok(json!({ "status": STATUS_OK, "headers": headers, "untrusted": false }))
}
