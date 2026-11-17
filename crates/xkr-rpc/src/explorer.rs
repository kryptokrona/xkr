//! Block explorer JSON-RPC methods (`f_blocks_list_json`, `f_block_json`,
//! `f_transaction_json`, `f_on_transactions_pool_json`) and the C++ JSON form of
//! transactions (`serialize(TransactionPrefix&)` / `serialize(Transaction&)` on the JSON
//! serializer: `vin` entries `{ "type": "ff"|"02", "value": … }`, `vout` targets
//! `{ "type": "02", "data": { "key" } }`, `extra` as hex).
//!
//! Transaction version 2 (fork 14, `docs/tx-v2.md` §9): a tagged output's target is
//! `{ "type": "03", "data": { "key", "view_tag": "<2 hex digits>" } }` and the CLSAG
//! signatures are listed under `clsag_signatures` (`[{ "s": [hex…], "c1": hex }]`, one per
//! input) next to an empty `signatures` array.
//!
//! Like C++ (`--enable-blockexplorer`), xkrd answers these only when started with `--explorer`
//! ([`RpcState::explorer`]); otherwise `/json_rpc` reports the method as not found.
//!
//! On a pruned node a block or transaction whose ring signatures were dropped is answered with
//! the error `-1` "block N is pruned on this node (its ring signatures were dropped)" for that
//! request, not an internal error (`ApiError::Pruned`).

use serde_json::{Value, json};
use xkr_chain::Chain;
use xkr_consensus::get_block_reward;
use xkr_consensus::reward::{full_reward_zone, median};
use xkr_serialization::{decode_block, decode_transaction, encode_transaction, transaction_hash};
use xkr_storage::Reader;
use xkr_types::{
    ClsagSignature, Hash, Transaction, TransactionPrefix, TxInput, TxOutputTarget, payment_id,
};

use crate::RpcState;
use crate::handlers::{ApiError, STATUS_OK, parse_hash, top_index, transaction_blob};
use crate::jsonrpc::RpcError;
use crate::mining::{CORE_RPC_ERROR_CODE_INTERNAL_ERROR, CORE_RPC_ERROR_CODE_WRONG_PARAM};

const CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT: i64 = -2;
/// `print_blocks_count` of `f_on_blocks_list_json`.
const BLOCKS_LIST_COUNT: u64 = 30;
/// Most transactions listed by `f_on_transactions_pool_json` (C++ lists all; a Kryptokrona
/// pool holds tens to hundreds). A larger pool is listed in pool (hash map) order up to this.
pub(crate) const MAX_POOL_LIST_TXS: usize = 10_000;

/// C++ JSON of a `TransactionPrefix`.
pub(crate) fn prefix_json(p: &TransactionPrefix) -> Value {
    let vin: Vec<Value> = p
        .inputs
        .iter()
        .map(|i| match i {
            TxInput::Coinbase { height } => json!({ "type": "ff", "value": { "height": height } }),
            TxInput::Key { amount, output_indexes, key_image } => json!({
                "type": "02",
                "value": { "amount": amount, "key_offsets": output_indexes, "k_image": key_image.to_string() },
            }),
        })
        .collect();
    let vout: Vec<Value> = p
        .outputs
        .iter()
        .map(|o| json!({ "amount": o.amount, "target": target_json(&o.target) }))
        .collect();
    json!({
        "version": p.version,
        "unlock_time": p.unlock_time,
        "vin": vin,
        "vout": vout,
        "extra": hex::encode(&p.extra),
    })
}

/// C++ JSON of an output target: `{ "type": "02", "data": { "key" } }`; a version-2 tagged
/// output is `{ "type": "03", "data": { "key", "view_tag" } }` (tag `0x03` as on the wire, the
/// view tag as two lowercase hex digits).
pub(crate) fn target_json(target: &TxOutputTarget) -> Value {
    match target {
        TxOutputTarget::Key(key) => json!({ "type": "02", "data": { "key": key.to_string() } }),
        TxOutputTarget::TaggedKey { key, view_tag } => json!({
            "type": "03",
            "data": { "key": key.to_string(), "view_tag": hex::encode([*view_tag]) },
        }),
    }
}

/// C++ JSON of a `Transaction`: the prefix plus `signatures` (one array of hex signatures per
/// input). Version 2 has an empty `signatures` and its CLSAG signatures in
/// `clsag_signatures`.
pub(crate) fn transaction_json(tx: &Transaction) -> Value {
    let mut v = prefix_json(&tx.prefix);
    let sigs: Vec<Value> = tx
        .signatures
        .iter()
        .map(|ring| Value::from(ring.iter().map(|s| hex::encode(s.0)).collect::<Vec<_>>()))
        .collect();
    v["signatures"] = Value::from(sigs);
    if !tx.clsag_signatures.is_empty() {
        v["clsag_signatures"] = clsag_json(&tx.clsag_signatures);
    }
    v
}

/// `[{ "s": [hex…], "c1": hex }]`, one per input (`docs/tx-v2.md` §9).
pub(crate) fn clsag_json(signatures: &[ClsagSignature]) -> Value {
    signatures
        .iter()
        .map(|c| {
            json!({
                "s": c.s.iter().map(|x| hex::encode(x.0)).collect::<Vec<_>>(),
                "c1": hex::encode(c.c1.0),
            })
        })
        .collect()
}

/// `(inputs, outputs)` amount sums (wrapping, like C++).
pub(crate) fn amounts(tx: &Transaction) -> (u64, u64) {
    let input: u64 = tx
        .prefix
        .inputs
        .iter()
        .map(TxInput::amount)
        .fold(0, u64::wrapping_add);
    let output: u64 = tx
        .prefix
        .outputs
        .iter()
        .map(|o| o.amount)
        .fold(0, u64::wrapping_add);
    (input, output)
}

/// `f_transaction_short_response`.
fn tx_short(tx: &Transaction, blob_len: usize, hash: &Hash) -> Value {
    let (input, output) = amounts(tx);
    let fee = if input == 0 {
        0
    } else {
        input.wrapping_sub(output)
    };
    json!({ "hash": hash.to_string(), "fee": fee, "amount_out": output, "size": blob_len })
}

/// Reward fields of a main-chain block (`Core::getBlockDetails`).
pub(crate) struct RewardInfo {
    /// `sizeMedian`: median size of the 100 blocks before it (0 for genesis).
    pub(crate) size_median: u64,
    /// `baseReward`: the reward without the size penalty.
    pub(crate) base_reward: u64,
    /// Share of the base reward lost to the size penalty.
    pub(crate) penalty: f64,
}

/// [`RewardInfo`] of the main-chain block at `height` with major version `major` and
/// `transactionsCumulativeSize` `txs_size`.
pub(crate) fn reward_info(
    reader: &Reader,
    height: u64,
    major: u8,
    txs_size: u64,
) -> Result<RewardInfo, ApiError> {
    let (size_median, prev_coins) = if height > 0 {
        let sizes = reader.median_block_size_window(height - 1, 100)?;
        let prev = reader
            .block_info(height - 1)?
            .map_or(0, |i| i.already_generated_coins);
        (median(&sizes), prev)
    } else {
        (0, 0)
    };
    let base_reward = get_block_reward(major, size_median, 0, prev_coins, 0).map_or(0, |r| r.0);
    let current_reward =
        get_block_reward(major, size_median, txs_size, prev_coins, 0).map_or(0, |r| r.0);
    #[expect(
        clippy::cast_precision_loss,
        reason = "C++ computes the penalty as a double"
    )]
    let penalty = if base_reward == 0 {
        0.0
    } else {
        base_reward.saturating_sub(current_reward) as f64 / base_reward as f64
    };
    Ok(RewardInfo {
        size_median,
        base_reward,
        penalty,
    })
}

/// Size fields of a main-chain block: `(blockSize, transactionsCumulativeSize)` as
/// `Core::getBlockDetails` computes them.
pub(crate) fn block_sizes(
    info_block_size: u64,
    blob_len: usize,
    coinbase_len: usize,
) -> (u64, u64) {
    (
        blob_len as u64 + info_block_size - coinbase_len as u64,
        info_block_size,
    )
}

/// `f_block_short_response` of the main-chain block at `height`.
fn block_short(chain: &Chain, reader: &Reader, height: u64) -> Result<Value, ApiError> {
    let info = reader
        .block_info(height)?
        .ok_or_else(|| ApiError::Internal(format!("no block at {height}")))?;
    let blob = chain
        .with_compressor(|c| reader.block_blob(height, c))??
        .ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
    let block = decode_block(&blob)?;
    let coinbase_len = encode_transaction(&block.miner_tx)?.len();
    let (block_size, _) = block_sizes(info.block_size, blob.len(), coinbase_len);
    Ok(json!({
        "difficulty": info.difficulty,
        "timestamp": block.header.timestamp,
        "height": height,
        "hash": Hash(info.hash).to_string(),
        "cumul_size": block_size,
        "tx_count": block.tx_hashes.len() + 1,
    }))
}

/// `f_blocks_list_json { height }`: the block at `height` and up to 30 below it, newest first.
pub(crate) fn blocks_list(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let height = params
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            RpcError::new(
                CORE_RPC_ERROR_CODE_WRONG_PARAM,
                "Wrong parameters, expected height",
            )
        })?;
    let reader = s.chain.reader().map_err(ApiError::from)?;
    let top = top_index(&reader)?;
    if height > top {
        return Err(RpcError::new(
            CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT,
            format!("To big height: {height}, current blockchain height = {top}"),
        ));
    }
    let last = height.saturating_sub(BLOCKS_LIST_COUNT);
    let mut blocks = Vec::with_capacity((height - last + 1) as usize);
    for h in (last..=height).rev() {
        blocks.push(block_short(&s.chain, &reader, h)?);
    }
    Ok(json!({ "blocks": blocks, "status": STATUS_OK }))
}

/// `f_block_json { hash }` (`hash` may also be a height).
pub(crate) fn block_details(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let reader = s.chain.reader().map_err(ApiError::from)?;
    let top = top_index(&reader)?;
    let height = if let Ok(h) = text.trim().parse::<u32>() {
        Some(u64::from(h))
    } else {
        let hash = parse_hash(&text).ok_or_else(|| {
            RpcError::new(
                CORE_RPC_ERROR_CODE_WRONG_PARAM,
                format!("Failed to parse hex representation of block hash. Hex = {text}."),
            )
        })?;
        reader.height_of(&hash).map_err(ApiError::from)?
    };
    let not_found = || {
        RpcError::new(
            CORE_RPC_ERROR_CODE_INTERNAL_ERROR,
            format!("Internal error: can't get block by hash. Hash = {text}."),
        )
    };
    let height = height.filter(|h| *h <= top).ok_or_else(not_found)?;

    let chain = &s.chain;
    let info = reader
        .block_info(height)
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;
    let blob = chain
        .with_compressor(|c| reader.block_blob(height, c))
        .map_err(ApiError::from)?
        .map_err(ApiError::from)?
        .ok_or_else(not_found)?;
    let block = decode_block(&blob).map_err(ApiError::from)?;
    let coinbase_blob = encode_transaction(&block.miner_tx).map_err(ApiError::from)?;
    let (block_size, txs_size) = block_sizes(info.block_size, blob.len(), coinbase_blob.len());
    let major = block.header.major_version;
    let RewardInfo {
        size_median,
        base_reward,
        penalty,
    } = reward_info(&reader, height, major, txs_size)?;

    let mut transactions = Vec::with_capacity(block.tx_hashes.len() + 1);
    transactions.push(tx_short(
        &block.miner_tx,
        coinbase_blob.len(),
        &transaction_hash(&block.miner_tx).map_err(ApiError::from)?,
    ));
    let mut total_fee: u64 = 0;
    for h in &block.tx_hashes {
        let Some(tx_blob) = transaction_blob(chain, &reader, h)? else {
            continue;
        };
        let tx = decode_transaction(&tx_blob).map_err(ApiError::from)?;
        let (input, output) = amounts(&tx);
        total_fee = total_fee.wrapping_add(input.wrapping_sub(output));
        transactions.push(tx_short(&tx, tx_blob.len(), h));
    }
    let reward: u64 = block.miner_tx.prefix.outputs.iter().map(|o| o.amount).sum();
    Ok(json!({
        "block": {
            "major_version": major,
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
            "blockSize": block_size,
            "sizeMedian": size_median,
            "effectiveSizeMedian": size_median.max(full_reward_zone(major) as u64),
            "transactionsCumulativeSize": txs_size,
            "alreadyGeneratedCoins": info.already_generated_coins.to_string(),
            "alreadyGeneratedTransactions": info.cumulative_tx_count,
            "baseReward": base_reward,
            "penalty": penalty,
            "transactions": transactions,
            "totalFeeAmount": total_fee,
        },
        "status": STATUS_OK,
    }))
}

/// `f_transaction_json { hash }`.
pub(crate) fn transaction_details(s: &RpcState, params: &Value) -> Result<Value, RpcError> {
    let text = params
        .get("hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let hash = parse_hash(text).ok_or_else(|| {
        RpcError::new(
            CORE_RPC_ERROR_CODE_WRONG_PARAM,
            format!("Failed to parse hex representation of transaction hash. Hex = {text}."),
        )
    })?;
    let reader = s.chain.reader().map_err(ApiError::from)?;
    let blob = transaction_blob(&s.chain, &reader, &hash)?.ok_or_else(|| {
        RpcError::new(
            CORE_RPC_ERROR_CODE_WRONG_PARAM,
            format!("transaction wasn't found. Hash = {text}."),
        )
    })?;
    let tx = decode_transaction(&blob).map_err(ApiError::from)?;
    let block = match reader.tx_location(&hash).map_err(ApiError::from)? {
        Some(loc) => block_short(&s.chain, &reader, loc.height)?,
        None => {
            json!({ "difficulty": 0, "timestamp": 0, "height": 0, "hash": "", "cumul_size": 0, "tx_count": 0 })
        }
    };
    let (input, output) = amounts(&tx);
    let fee = if input == 0 {
        0
    } else {
        input.wrapping_sub(output)
    };
    let mixin = tx
        .prefix
        .inputs
        .iter()
        .map(TxInput::signature_count)
        .max()
        .unwrap_or(0);
    Ok(json!({
        "tx": transaction_json(&tx),
        "txDetails": {
            "hash": hash.to_string(),
            "size": blob.len(),
            "paymentId": payment_id(&tx.prefix.extra).as_ref().map_or_default(ToString::to_string),
            "mixin": mixin,
            "fee": fee,
            "amount_out": output,
        },
        "block": block,
        "status": STATUS_OK,
    }))
}

/// `f_on_transactions_pool_json`.
pub(crate) fn pool(s: &RpcState) -> Value {
    // One entry cloned at a time instead of the whole pool per call (`entries()`), and at most
    // MAX_POOL_LIST_TXS of them.
    let pool = s.chain.pool();
    let transactions: Vec<Value> = pool
        .hashes()
        .iter()
        .take(MAX_POOL_LIST_TXS)
        .filter_map(|h| pool.entry(h))
        .map(|e| tx_short(&e.tx, e.size(), &e.hash))
        .collect();
    json!({ "transactions": transactions, "status": STATUS_OK })
}
