//! Block and transaction details (`Core::getBlockDetails`, `Core::getTransactionDetails`) in
//! the JSON form of the C++ `blockchain_explorer_data_serialization.cpp`, and the REST routes
//! that return them:
//!
//! | path | request | response |
//! |---|---|---|
//! | `/get_block_details_by_height` | `{ blockHeight }` | `{ status, block }` |
//! | `/get_blocks_details_by_heights` | `{ blockHeights: [..] }` | `{ status, blocks }` |
//! | `/get_blocks_details_by_hashes` | `{ blockHashes: [..] }` | `{ status, blocks }` |
//! | `/get_transaction_details_by_hashes` | `{ transactionHashes: [..] }` | `{ status, transactions }` |
//! | `/get_transaction_hashes_by_payment_id` | `{ paymentId }` | `{ status, transactionHashes }` |
//!
//! `BlockDetails` keys: `majorVersion`, `minorVersion`, `timestamp`, `prevBlockHash`, `nonce`,
//! `index`, `hash`, `difficulty`, `reward`, `baseReward`, `blockSize`,
//! `transactionsCumulativeSize`, `alreadyGeneratedCoins`, `alreadyGeneratedTransactions`,
//! `sizeMedian`, `totalFeeAmount`, `transactions` (C++ leaves `penalty` and `isAlternative`
//! out). `TransactionDetails` keys: `hash`, `size`, `fee`, `totalInputsAmount`,
//! `totalOutputsAmount`, `mixin`, `unlockTime`, `timestamp`, `paymentId` (zeros when none),
//! `inBlockchain`, `blockHash`, `blockIndex`, `extra { publicKey, nonce: [bytes], raw }`,
//! `inputs`, `outputs`, `signaturesSize`, `signatures: [{ first: input, second: hex }]`.
//! Inputs are `{ "type": "ff", "data": { "input": { "height" }, "amount" } }` and
//! `{ "type": "02", "data": { "input": { "amount", "key_offsets", "k_image" }, "mixin",
//! "output": { "transactionHash", "number" } } }`, where `output` is the ring member with the
//! highest global index (C++ `outputReferences.back()`). Outputs are `{ "output": { "amount",
//! "target" }, "globalIndex" }` with the target of `explorer::target_json` (a version-2 tagged
//! output is `{ "type": "03", "data": { "key", "view_tag" } }`, `docs/tx-v2.md` §9).
//!
//! Differences from C++, all additive or about limits:
//!
//! * `extra.raw` is the transaction's whole `extra` as hex (C++ never fills it and writes `""`).
//! * A version-2 transaction also carries `clsag_signatures` (as in `f_transaction_json`); its
//!   `signatures` is empty and `signaturesSize` 0.
//! * Only the main chain and the pool are searched (C++ also finds alternative blocks).
//! * Request lists are capped ([`MAX_BLOCK_DETAILS`], [`MAX_TX_DETAILS`]); a longer list is
//!   answered with an `"Error: ..."` status, like the C++ failures. C++ has no caps.
//! * An answer holds at most [`MAX_DETAILS_ANSWER_BYTES`] of JSON; a request whose answer would
//!   be larger is answered with an `"Error: ..."` status (ask for fewer). A hash or height that a
//!   request repeats is answered at every position, like C++; its details are built once.
//! * A failed `/get_block_details_by_height` has `"block": null` (C++ writes a zeroed block).
//! * A block or transaction whose ring signatures this node pruned (`docs/storage.md` "Stripe
//!   pruning") fails the request like a missing one, with the status `"Error: block N is pruned
//!   on this node (its ring signatures were dropped)."` (C++ nodes never prune).
//!
//! Failures keep the C++ shape: HTTP 200 with `status` = `"Error: <reason>"` and an empty
//! list. An unparsable hash in the request is `400` (C++ drops the request).

use std::collections::HashMap;
use std::hash::Hash as HashKey;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use xkr_chain::Chain;
use xkr_serialization::{
    decode_block, decode_transaction, decode_transaction_prefix, encode_transaction,
    transaction_hash,
};
use xkr_storage::{Compressor, Reader};
use xkr_types::{ExtraField, Hash, Transaction, TxInput, parse_extra_partial, payment_id};

use crate::RpcState;
use crate::explorer::{RewardInfo, amounts, block_sizes, clsag_json, reward_info, target_json};
use crate::handlers::{ApiError, STATUS_OK, parse, parse_hash, run, run_bytes, transaction_blob};
use crate::listing::JsonList;

/// Most blocks per `/get_blocks_details_by_heights` and `/get_blocks_details_by_hashes` call.
/// Every block carries the details of all its transactions; explorers ask for one page (tens).
pub(crate) const MAX_BLOCK_DETAILS: usize = 100;
/// Most transactions per `/get_transaction_details_by_hashes` call (one block's worth is far
/// below it).
pub(crate) const MAX_TX_DETAILS: usize = 1_000;
/// Most bytes of JSON in one `/get_blocks_details_by_*` or `/get_transaction_details_by_hashes`
/// answer.
pub const MAX_DETAILS_ANSWER_BYTES: usize = 16 * 1024 * 1024;
/// Heights whose output list [`Details`] keeps per request (ring-member lookups); the cache is
/// emptied when it is full, so memory stays bounded for any request.
const MAX_CACHED_HEIGHTS: usize = 4_096;

/// C++ `std::runtime_error` texts of `Core::getBlockDetails` / `getTransactionDetails`.
const BLOCK_HEIGHT_NOT_FOUND: &str = "Error: Requested block height wasn't found in blockchain.";
const BLOCK_HASH_NOT_FOUND: &str = "Error: Requested hash wasn't found in blockchain.";
const TX_NOT_FOUND: &str = "Error: Requested transaction wasn't found.";

/// The C++-shaped failure status of an item in a block whose signatures this node pruned.
fn pruned_status(height: u64) -> String {
    format!("Error: {}.", ApiError::pruned_message(height))
}

/// Where a transaction is.
#[derive(Debug, Clone, Copy)]
enum TxPlace {
    /// In the main-chain block at `height`.
    Chain {
        height: u64,
        block_hash: Hash,
        timestamp: u64,
    },
    /// In the pool since `received`.
    Pool { received: u64 },
}

/// Every transaction of one block: its hash and `(amount, global index)` per output.
type BlockOutputs = Vec<(Hash, Vec<(u64, u32)>)>;

/// One request's read state: a snapshot and the outputs of the blocks ring members came from.
struct Details<'a> {
    chain: &'a Chain,
    reader: &'a Reader,
    outputs_by_height: HashMap<u64, BlockOutputs>,
}

/// C++ `CachedTransaction::getTransactionFee`: 0 for a transaction with a coinbase input,
/// inputs minus outputs (wrapping) otherwise.
fn tx_fee(tx: &Transaction) -> u64 {
    if tx
        .prefix
        .inputs
        .iter()
        .any(|i| matches!(i, TxInput::Coinbase { .. }))
    {
        return 0;
    }
    let (input, output) = amounts(tx);
    input.wrapping_sub(output)
}

impl<'a> Details<'a> {
    fn new(chain: &'a Chain, reader: &'a Reader) -> Self {
        Self {
            chain,
            reader,
            outputs_by_height: HashMap::new(),
        }
    }

    /// Runs a storage read with a pooled decompression context.
    fn read<R>(
        &self,
        f: impl FnOnce(&Reader, &mut Compressor) -> xkr_storage::Result<R>,
    ) -> Result<R, ApiError> {
        let reader = self.reader;
        Ok(self.chain.with_compressor(|c| f(reader, c))??)
    }

    /// [`BlockOutputs`] of the main-chain block at `height` (empty if there is none).
    fn load_block_outputs(&self, height: u64) -> Result<BlockOutputs, ApiError> {
        let Some(blob) = self.read(|r, c| r.block_blob(height, c))? else {
            return Ok(Vec::new());
        };
        let block = decode_block(&blob)?;
        let pair = |amounts: Vec<u64>, indexes: Option<Vec<u32>>| -> Vec<(u64, u32)> {
            amounts
                .into_iter()
                .zip(indexes.unwrap_or_default())
                .collect()
        };
        let mut out = Vec::with_capacity(block.tx_hashes.len() + 1);
        let miner_indexes = self.read(|r, c| r.tx_global_indexes(height, 0, c))?;
        let miner_amounts = block.miner_tx.prefix.outputs.iter().map(|o| o.amount);
        out.push((
            transaction_hash(&block.miner_tx)?,
            pair(miner_amounts.collect(), miner_indexes),
        ));
        for (i, hash) in block.tx_hashes.iter().enumerate() {
            let index = u32::try_from(i + 1)
                .map_err(|_| ApiError::Internal("too many transactions".into()))?;
            let Some(prefix) = self.read(|r, c| r.tx_prefix_at(height, index, c))? else {
                continue;
            };
            let prefix = decode_transaction_prefix(&prefix)?;
            let indexes = self.read(|r, c| r.tx_global_indexes(height, index, c))?;
            out.push((
                *hash,
                pair(prefix.outputs.iter().map(|o| o.amount).collect(), indexes),
            ));
        }
        Ok(out)
    }

    /// C++ `extractKeyOtputReferences` for one ring member: the transaction that created the
    /// output `(amount, global_index)` and the output's index in it. Zeros when unknown (C++
    /// asserts instead).
    fn output_reference(&mut self, amount: u64, global_index: u32) -> Result<Value, ApiError> {
        let row = self
            .reader
            .outputs(&[(amount, u64::from(global_index))])?
            .pop()
            .flatten();
        let mut found = None;
        if let Some(info) = row {
            if !self.outputs_by_height.contains_key(&info.height) {
                if self.outputs_by_height.len() >= MAX_CACHED_HEIGHTS {
                    self.outputs_by_height.clear();
                }
                let outputs = self.load_block_outputs(info.height)?;
                self.outputs_by_height.insert(info.height, outputs);
            }
            found = self.outputs_by_height.get(&info.height).and_then(|txs| {
                txs.iter().find_map(|(hash, outs)| {
                    outs.iter()
                        .position(|&(a, gi)| a == amount && gi == global_index)
                        .map(|n| (*hash, n))
                })
            });
        }
        let (hash, number) = found.unwrap_or((Hash::ZERO, 0));
        Ok(json!({ "transactionHash": hash.to_string(), "number": number }))
    }

    /// C++ `TransactionDetails` of `tx` (blob size `size`), with the global indexes of its
    /// outputs (`None`: zeros, as C++ writes for pool transactions).
    fn transaction_json(
        &mut self,
        hash: &Hash,
        tx: &Transaction,
        size: usize,
        place: TxPlace,
        global_indexes: Option<&[u32]>,
    ) -> Result<Value, ApiError> {
        let (input_total, output_total) = amounts(tx);
        let mixin = tx
            .prefix
            .inputs
            .iter()
            .map(TxInput::signature_count)
            .max()
            .unwrap_or(0);
        let extra = &tx.prefix.extra;
        let fields = parse_extra_partial(extra).0;
        let public_key = fields
            .iter()
            .find_map(|f| match f {
                ExtraField::PublicKey(k) => Some(k.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| Hash::ZERO.to_string());
        let nonce: Vec<u8> = fields
            .into_iter()
            .find_map(|f| match f {
                ExtraField::Nonce(n) => Some(n),
                _ => None,
            })
            .unwrap_or_default();

        let mut inputs = Vec::with_capacity(tx.prefix.inputs.len());
        for input in &tx.prefix.inputs {
            match input {
                TxInput::Coinbase { height } => inputs.push(json!({
                    "type": "ff",
                    "data": { "input": { "height": height }, "amount": output_total },
                })),
                TxInput::Key {
                    amount,
                    output_indexes,
                    key_image,
                } => {
                    let output = match input.absolute_output_indexes().last() {
                        Some(gi) => self.output_reference(*amount, *gi)?,
                        None => json!({ "transactionHash": Hash::ZERO.to_string(), "number": 0 }),
                    };
                    inputs.push(json!({
                        "type": "02",
                        "data": {
                            "input": {
                                "amount": amount,
                                "key_offsets": output_indexes,
                                "k_image": key_image.to_string(),
                            },
                            "mixin": output_indexes.len(),
                            "output": output,
                        },
                    }));
                }
            }
        }
        let outputs: Vec<Value> = tx
            .prefix
            .outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let global = global_indexes.and_then(|g| g.get(i)).copied().unwrap_or(0);
                json!({
                    "output": { "amount": o.amount, "target": target_json(&o.target) },
                    "globalIndex": global,
                })
            })
            .collect();
        let signatures: Vec<Value> = tx
            .signatures
            .iter()
            .enumerate()
            .flat_map(|(i, ring)| {
                ring.iter()
                    .map(move |s| json!({ "first": i, "second": hex::encode(s.0) }))
            })
            .collect();
        let (in_blockchain, block_hash, block_index, timestamp) = match place {
            TxPlace::Chain {
                height,
                block_hash,
                timestamp,
            } => (true, block_hash, height, timestamp),
            TxPlace::Pool { received } => (false, Hash::ZERO, 0, received),
        };
        let mut v = json!({
            "hash": hash.to_string(),
            "size": size,
            "fee": tx_fee(tx),
            "totalInputsAmount": input_total,
            "totalOutputsAmount": output_total,
            "mixin": mixin,
            "unlockTime": tx.prefix.unlock_time,
            "timestamp": timestamp,
            "paymentId": payment_id(extra).unwrap_or(Hash::ZERO).to_string(),
            "inBlockchain": in_blockchain,
            "blockHash": block_hash.to_string(),
            "blockIndex": block_index,
            "extra": { "publicKey": public_key, "nonce": nonce, "raw": hex::encode(extra) },
            "inputs": inputs,
            "outputs": outputs,
            "signaturesSize": tx.signatures.len(),
            "signatures": signatures,
        });
        if !tx.clsag_signatures.is_empty() {
            v["clsag_signatures"] = clsag_json(&tx.clsag_signatures);
        }
        Ok(v)
    }

    /// `TransactionDetails` of a main-chain or pool transaction; `None` if unknown.
    fn transaction(&mut self, hash: &Hash) -> Result<Option<Value>, ApiError> {
        if let Some(loc) = self.reader.tx_location(hash)? {
            let Some(info) = self.reader.block_info(loc.height)? else {
                return Ok(None);
            };
            let Some(blob) = transaction_blob(self.chain, self.reader, hash)? else {
                return Ok(None);
            };
            let tx = decode_transaction(&blob)?;
            let indexes = self.read(|r, c| r.tx_global_indexes(loc.height, loc.tx_index, c))?;
            let place = TxPlace::Chain {
                height: loc.height,
                block_hash: Hash(info.hash),
                timestamp: info.timestamp,
            };
            return self
                .transaction_json(hash, &tx, blob.len(), place, indexes.as_deref())
                .map(Some);
        }
        match self.chain.pool().entry(hash) {
            Some(e) => {
                let place = TxPlace::Pool {
                    received: e.received,
                };
                self.transaction_json(hash, &e.tx, e.size(), place, None)
                    .map(Some)
            }
            None => Ok(None),
        }
    }

    /// `BlockDetails` of the main-chain block at `height`; `None` if there is none.
    fn block(&mut self, height: u64) -> Result<Option<Value>, ApiError> {
        let Some(info) = self.reader.block_info(height)? else {
            return Ok(None);
        };
        let Some(blob) = self.read(|r, c| r.block_blob(height, c))? else {
            return Ok(None);
        };
        let block = decode_block(&blob)?;
        let coinbase_len = encode_transaction(&block.miner_tx)?.len();
        let (block_size, txs_size) = block_sizes(info.block_size, blob.len(), coinbase_len);
        let major = block.header.major_version;
        let RewardInfo {
            size_median,
            base_reward,
            ..
        } = reward_info(self.reader, height, major, txs_size)?;
        let place = TxPlace::Chain {
            height,
            block_hash: Hash(info.hash),
            timestamp: block.header.timestamp,
        };

        let mut transactions = Vec::with_capacity(block.tx_hashes.len() + 1);
        let miner_hash = transaction_hash(&block.miner_tx)?;
        let miner_indexes = self.read(|r, c| r.tx_global_indexes(height, 0, c))?;
        transactions.push(self.transaction_json(
            &miner_hash,
            &block.miner_tx,
            coinbase_len,
            place,
            miner_indexes.as_deref(),
        )?);
        let mut total_fee: u64 = 0;
        for (i, hash) in block.tx_hashes.iter().enumerate() {
            let index = u32::try_from(i + 1)
                .map_err(|_| ApiError::Internal("too many transactions".into()))?;
            let tx_blob = self
                .read(|r, c| r.transaction_blob(hash, c))?
                .ok_or_else(|| {
                    ApiError::Internal(format!("transaction {hash} of block {height} missing"))
                })?;
            let tx = decode_transaction(&tx_blob)?;
            let indexes = self.read(|r, c| r.tx_global_indexes(height, index, c))?;
            total_fee = total_fee.wrapping_add(tx_fee(&tx));
            transactions.push(self.transaction_json(
                hash,
                &tx,
                tx_blob.len(),
                place,
                indexes.as_deref(),
            )?);
        }
        let reward = block
            .miner_tx
            .prefix
            .outputs
            .iter()
            .map(|o| o.amount)
            .fold(0u64, u64::wrapping_add);
        Ok(Some(json!({
            "majorVersion": major,
            "minorVersion": block.header.minor_version,
            "timestamp": block.header.timestamp,
            "prevBlockHash": block.header.prev_hash.to_string(),
            "nonce": block.header.nonce,
            "index": height,
            "hash": Hash(info.hash).to_string(),
            "difficulty": info.difficulty,
            "reward": reward,
            "baseReward": base_reward,
            "blockSize": block_size,
            "transactionsCumulativeSize": txs_size,
            "alreadyGeneratedCoins": info.already_generated_coins,
            "alreadyGeneratedTransactions": info.cumulative_tx_count,
            "sizeMedian": size_median,
            "totalFeeAmount": total_fee,
            "transactions": transactions,
        })))
    }
}

/// Parses a list of hex hashes; `400` naming the first bad one.
fn parse_hashes(texts: &[String], what: &str) -> Result<Vec<Hash>, ApiError> {
    texts
        .iter()
        .map(|t| {
            parse_hash(t).ok_or_else(|| ApiError::BadRequest(format!("Failed to parse {what} {t}")))
        })
        .collect()
}

fn too_many(what: &str, max: usize) -> String {
    format!("Error: too many {what} requested (at most {max})")
}

fn too_large(what: &str) -> String {
    format!("Error: the requested {what} exceed the response size limit, ask for fewer")
}

/// A C++-shaped failure (`{ status, <list>: [] }`) as bytes.
fn failure_bytes(list: &str, status: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "status": status, list: [] })).unwrap_or_default()
}

/// The answer list of a detail route: one entry per requested key, in request order, built
/// into `list` (started with the route's prefix) and closed with `suffix`. Each distinct key is
/// looked up once; a repeated key is written again from its serialized form. A key `lookup`
/// does not find fails the request with `not_found`, a pruned block with its status, and an
/// answer over the list's budget with [`too_large`].
fn answer_each<K: Copy + Eq + HashKey>(
    name: &str,
    keys: &[K],
    not_found: &str,
    mut list: JsonList,
    suffix: &[u8],
    mut lookup: impl FnMut(K) -> Result<Option<Value>, ApiError>,
) -> Result<Vec<u8>, ApiError> {
    let mut built: HashMap<K, Vec<u8>> = HashMap::new();
    for &key in keys {
        if let Some(bytes) = built.get(&key) {
            if !list.push_raw(bytes) {
                return Ok(failure_bytes(name, &too_large(name)));
            }
            continue;
        }
        match lookup(key) {
            Ok(Some(v)) => {
                let bytes =
                    serde_json::to_vec(&v).map_err(|e| ApiError::Internal(e.to_string()))?;
                if !list.push_raw(&bytes) {
                    return Ok(failure_bytes(name, &too_large(name)));
                }
                built.insert(key, bytes);
            }
            Ok(None) => return Ok(failure_bytes(name, not_found)),
            Err(ApiError::Pruned(h)) => return Ok(failure_bytes(name, &pruned_status(h))),
            Err(e) => return Err(e),
        }
    }
    Ok(list.finish(suffix))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BlockHeightRequest {
    block_height: u64,
}

/// `/get_block_details_by_height` (`onGetBlockDetailsByHeight`).
pub async fn get_block_details_by_height(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: BlockHeightRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let reader = s.chain.reader()?;
        let mut d = Details::new(&s.chain, &reader);
        Ok(match d.block(req.block_height) {
            Ok(Some(block)) => json!({ "status": STATUS_OK, "block": block }),
            Ok(None) => json!({ "status": BLOCK_HEIGHT_NOT_FOUND, "block": null }),
            Err(ApiError::Pruned(h)) => json!({ "status": pruned_status(h), "block": null }),
            Err(e) => return Err(e),
        })
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BlockHeightsRequest {
    block_heights: Vec<u64>,
}

/// `/get_blocks_details_by_heights` (`onGetBlocksDetailsByHeights`).
pub async fn get_blocks_details_by_heights(
    State(s): State<Arc<RpcState>>,
    body: Bytes,
) -> Response {
    let req: BlockHeightsRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        if req.block_heights.len() > MAX_BLOCK_DETAILS {
            return Ok(failure_bytes(
                "blocks",
                &too_many("blocks", MAX_BLOCK_DETAILS),
            ));
        }
        let reader = s.chain.reader()?;
        let mut d = Details::new(&s.chain, &reader);
        answer_each(
            "blocks",
            &req.block_heights,
            BLOCK_HEIGHT_NOT_FOUND,
            JsonList::new(b"{\"blocks\":", MAX_DETAILS_ANSWER_BYTES),
            b",\"status\":\"OK\"}",
            |height| d.block(height),
        )
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct BlockHashesRequest {
    block_hashes: Vec<String>,
}

/// `/get_blocks_details_by_hashes` (`onGetBlocksDetailsByHashes`).
pub async fn get_blocks_details_by_hashes(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: BlockHashesRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        if req.block_hashes.len() > MAX_BLOCK_DETAILS {
            return Ok(failure_bytes(
                "blocks",
                &too_many("blocks", MAX_BLOCK_DETAILS),
            ));
        }
        let hashes = parse_hashes(&req.block_hashes, "block hash")?;
        let reader = s.chain.reader()?;
        let mut d = Details::new(&s.chain, &reader);
        answer_each(
            "blocks",
            &hashes,
            BLOCK_HASH_NOT_FOUND,
            JsonList::new(b"{\"blocks\":", MAX_DETAILS_ANSWER_BYTES),
            b",\"status\":\"OK\"}",
            |hash| match reader.height_of(&hash)? {
                Some(height) => d.block(height),
                None => Ok(None),
            },
        )
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TxHashesRequest {
    transaction_hashes: Vec<String>,
}

/// `/get_transaction_details_by_hashes` (`onGetTransactionDetailsByHashes`).
pub async fn get_transaction_details_by_hashes(
    State(s): State<Arc<RpcState>>,
    body: Bytes,
) -> Response {
    let req: TxHashesRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        if req.transaction_hashes.len() > MAX_TX_DETAILS {
            return Ok(failure_bytes(
                "transactions",
                &too_many("transactions", MAX_TX_DETAILS),
            ));
        }
        let hashes = parse_hashes(&req.transaction_hashes, "transaction hash")?;
        let reader = s.chain.reader()?;
        let mut d = Details::new(&s.chain, &reader);
        answer_each(
            "transactions",
            &hashes,
            TX_NOT_FOUND,
            JsonList::new(
                b"{\"status\":\"OK\",\"transactions\":",
                MAX_DETAILS_ANSWER_BYTES,
            ),
            b"}",
            |hash| d.transaction(&hash),
        )
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PaymentIdRequest {
    payment_id: String,
}

/// `/get_transaction_hashes_by_payment_id` (`Core::getTransactionHashesByPaymentId`): the
/// main-chain transactions in chain order, then the pool's. The answer is bounded by what the
/// chain holds for that payment id (each transaction paid a fee), not by the request.
pub async fn get_transaction_hashes_by_payment_id(
    State(s): State<Arc<RpcState>>,
    body: Bytes,
) -> Response {
    let req: PaymentIdRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let pid = parse_hash(&req.payment_id).ok_or_else(|| {
            ApiError::BadRequest(format!("Failed to parse payment id {}", req.payment_id))
        })?;
        let reader = s.chain.reader()?;
        let mut hashes = s
            .chain
            .with_compressor(|c| reader.tx_hashes_by_payment_id(&pid.0, c))??;
        hashes.extend(s.chain.pool().hashes_by_payment_id(&pid));
        let hashes: Vec<String> = hashes.iter().map(ToString::to_string).collect();
        Ok(json!({ "status": STATUS_OK, "transactionHashes": hashes }))
    })
    .await
}
