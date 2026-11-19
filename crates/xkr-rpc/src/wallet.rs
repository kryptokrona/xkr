//! Wallet sync responses (`Core::getWalletSyncData`, `Core::getGlobalIndexesForRange`) in the
//! exact JSON shape of the C++ `wallet_types::WalletBlockInfo` serializers in `rpc_server.cpp`.

use serde_json::{Value, json};
use xkr_chain::Chain;
use xkr_serialization::{decode_block, decode_transaction_prefix, transaction_hash};
use xkr_types::{Block, Hash, TransactionPrefix, TxInput};

use crate::handlers::{ApiError, top_index};

/// `BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`: most blocks per `/getwalletsyncdata` call.
const WALLET_SYNC_MAX_BLOCKS: u64 = 100;

/// `Core::getPubKeyFromExtra` (`xkr_types::wallet_tx_public_key`): the fields are read in
/// order, so the key is found whatever comes before it, also a payment id nonce
/// (`WalletBackend` writes the nonce first). When the fields do not parse up to a key, the 32
/// bytes after the first `0x01` byte (all zeros if absent or truncated). The state hashes
/// (`xkr_chain::state`) use the same function.
#[must_use]
pub fn pub_key_from_extra(extra: &[u8]) -> [u8; 32] {
    xkr_types::wallet_tx_public_key(extra)
}

/// `Core::getPaymentIDFromExtra`: the payment id of `extra` as lowercase hex, or an empty
/// string. The fields are read in order (`xkr_types::payment_id`); when they do not parse,
/// the first `0x02 <size> 0x00 <32 bytes>` occurrence.
#[must_use]
pub fn payment_id_from_extra(extra: &[u8]) -> String {
    if let Some(id) = xkr_types::payment_id(extra) {
        return hex::encode(id.0);
    }
    if xkr_types::parse_extra(extra).is_ok() {
        return String::new();
    }
    for (i, b) in extra.iter().enumerate() {
        if *b != 0x02 {
            continue;
        }
        if extra.len() - i - 1 < 32 + 1 + 1 {
            return String::new();
        }
        if extra[i + 2] == 0x00 {
            return hex::encode(&extra[i + 3..i + 35]);
        }
    }
    String::new()
}

/// `[{ "key", "amount" }]` as C++; a version-2 tagged output (fork 14, `docs/tx-v2.md` §9)
/// adds `"view_tag"` (two lowercase hex digits), so version-1 responses are unchanged and old
/// wallets that ignore unknown fields keep working (they just scan without the fast path).
fn outputs_json(prefix: &TransactionPrefix) -> Vec<Value> {
    prefix
        .outputs
        .iter()
        .map(|o| match o.view_tag() {
            Some(tag) => json!({
                "key": o.key().to_string(),
                "amount": o.amount,
                "view_tag": hex::encode([tag]),
            }),
            None => json!({ "key": o.key().to_string(), "amount": o.amount }),
        })
        .collect()
}

fn coinbase_json(block: &Block) -> Result<Value, ApiError> {
    let p = &block.miner_tx.prefix;
    Ok(json!({
        "outputs": outputs_json(p),
        "hash": transaction_hash(&block.miner_tx)?.to_string(),
        "txPublicKey": hex::encode(pub_key_from_extra(&p.extra)),
        "unlockTime": p.unlock_time,
    }))
}

fn transaction_json(hash: &Hash, p: &TransactionPrefix) -> Value {
    let inputs: Vec<Value> = p
        .inputs
        .iter()
        .filter_map(|i| match i {
            TxInput::Key {
                amount,
                output_indexes,
                key_image,
            } => Some(json!({
                "amount": amount,
                "key_offsets": output_indexes,
                "k_image": key_image.to_string(),
            })),
            TxInput::Coinbase { .. } => None,
        })
        .collect();
    json!({
        "inputs": inputs,
        "paymentID": payment_id_from_extra(&p.extra),
        "outputs": outputs_json(p),
        "hash": hash.to_string(),
        "txPublicKey": hex::encode(pub_key_from_extra(&p.extra)),
        "unlockTime": p.unlock_time,
    })
}

/// `Core::getWalletSyncData`. `None` = "Failed to perform query" (none of the checkpoint
/// hashes is on the main chain).
pub fn wallet_sync_data(
    chain: &Chain,
    checkpoints: &[Hash],
    start_height: u64,
    start_timestamp: u64,
    block_count: u64,
) -> Result<Option<Vec<Value>>, ApiError> {
    let reader = chain.reader()?;
    let current = top_index(&reader)?;
    let count = match block_count.min(WALLET_SYNC_MAX_BLOCKS) {
        0 => WALLET_SYNC_MAX_BLOCKS,
        n => n,
    };

    // C++ getBlockHeightForTimestamp works on timestamps rounded down to midnight.
    let timestamp_height = if start_timestamp == 0 {
        0
    } else {
        match reader.height_by_timestamp(start_timestamp - start_timestamp % 86_400)? {
            Some(h) => h,
            // Not synced that far yet: nothing to return.
            None => return Ok(Some(Vec::new())),
        }
    };
    let first_height = if start_height == 0 {
        timestamp_height
    } else {
        start_height
    };

    let mut last_known = 0;
    if !checkpoints.is_empty() {
        let mut found = None;
        for h in checkpoints {
            if let Some(height) = reader.height_of(h)? {
                found = Some(height);
                break;
            }
        }
        match found {
            Some(h) => last_known = h,
            None => return Ok(None),
        }
    }
    let start = (if last_known == 0 { 0 } else { last_known + 1 }).max(first_height);
    if current < start {
        return Ok(Some(Vec::new()));
    }
    let end = count.min(current - start + 1) + start;

    let mut items = Vec::with_capacity((end - start) as usize);
    for height in start..end {
        let info = reader
            .block_info(height)?
            .ok_or_else(|| ApiError::Internal(format!("no block at {height}")))?;
        let blob = chain
            .with_compressor(|c| reader.block_blob(height, c))??
            .ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
        let block = decode_block(&blob)?;
        let mut transactions = Vec::with_capacity(block.tx_hashes.len());
        for (i, hash) in block.tx_hashes.iter().enumerate() {
            let index = u32::try_from(i + 1)
                .map_err(|_| ApiError::Internal("too many transactions".into()))?;
            let prefix = chain
                .with_compressor(|c| reader.tx_prefix_at(height, index, c))??
                .ok_or_else(|| {
                    ApiError::Internal(format!("no transaction {index} in block {height}"))
                })?;
            let prefix = decode_transaction_prefix(&prefix)?;
            transactions.push(transaction_json(hash, &prefix));
        }
        items.push(json!({
            "coinbaseTX": coinbase_json(&block)?,
            "transactions": transactions,
            "blockHeight": height,
            "blockHash": Hash(info.hash).to_string(),
            "blockTimestamp": block.header.timestamp,
        }));
    }
    Ok(Some(items))
}

/// Heights read from storage at once by [`global_indexes_for_range`].
const INDEX_RANGE_CHUNK: u64 = 500;

/// `Core::getGlobalIndexesForRange(start, end)` (end exclusive): every transaction of the
/// blocks with its global output indexes, as the C++ map serialization
/// `[{ "key": hash, "value": [indexes] }]`.
///
/// Memory: storage rows are read [`INDEX_RANGE_CHUNK`] heights at a time and only the block being
/// answered is kept decoded (rows come in height order), instead of every block of the range (up to
/// 10 000 decoded blocks before). One read snapshot serves the whole range, so the answer is the
/// same.
pub fn global_indexes_for_range(
    chain: &Chain,
    start: u64,
    end: u64,
) -> Result<Vec<Value>, ApiError> {
    let reader = chain.reader()?;
    let mut out = Vec::new();
    let mut from = start;
    while from < end {
        let to = from.saturating_add(INDEX_RANGE_CHUNK).min(end);
        let rows = chain.with_compressor(|c| reader.global_indexes_for_range(from, to, c))??;
        if rows.is_empty() {
            // Above the top block: nothing further either.
            break;
        }
        let mut current: Option<(u64, Block)> = None;
        for ((height, tx_index), indexes) in rows {
            if current.as_ref().is_none_or(|(h, _)| *h != height) {
                let blob = chain
                    .with_compressor(|c| reader.block_blob(height, c))??
                    .ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
                current = Some((height, decode_block(&blob)?));
            }
            let Some((_, block)) = current.as_ref() else {
                continue;
            };
            let hash = if tx_index == 0 {
                transaction_hash(&block.miner_tx)?
            } else {
                match block.tx_hashes.get(tx_index as usize - 1) {
                    Some(h) => *h,
                    None => continue,
                }
            };
            let indexes: Vec<u64> = indexes.into_iter().map(u64::from).collect();
            out.push(json!({ "key": hash.to_string(), "value": indexes }));
        }
        from = to;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_outputs_carry_their_view_tag() {
        use xkr_types::{PublicKey, TxOutput, TxOutputTarget};
        let key = PublicKey([9u8; 32]);
        let prefix = TransactionPrefix {
            version: 2,
            outputs: vec![
                TxOutput {
                    amount: 5,
                    target: TxOutputTarget::TaggedKey {
                        key,
                        view_tag: 0x0a,
                    },
                },
                TxOutput {
                    amount: 6,
                    target: TxOutputTarget::Key(key),
                },
            ],
            ..TransactionPrefix::default()
        };
        let out = outputs_json(&prefix);
        assert_eq!(out[0]["view_tag"], "0a");
        assert_eq!(out[0]["key"], key.to_string());
        assert_eq!(out[0]["amount"], 5);
        assert!(out[1].get("view_tag").is_none());
        let t = crate::explorer::target_json(&prefix.outputs[0].target);
        assert_eq!(t["type"], "03");
        assert_eq!(t["data"]["view_tag"], "0a");
        assert_eq!(
            crate::explorer::target_json(&prefix.outputs[1].target)["type"],
            "02"
        );
    }

    #[test]
    fn extra_scanners_match_cpp() {
        let mut extra = vec![0x01];
        extra.extend_from_slice(&[7u8; 32]);
        extra.extend_from_slice(&[0x02, 33, 0x00]);
        extra.extend_from_slice(&[0xab; 32]);
        assert_eq!(pub_key_from_extra(&extra), [7u8; 32]);
        assert_eq!(payment_id_from_extra(&extra), "ab".repeat(32));
        assert_eq!(pub_key_from_extra(&[0x01, 1, 2]), [0u8; 32]);
        assert_eq!(payment_id_from_extra(&[0x02, 1, 0x00]), "");
        // the payment id nonce first (`WalletBackend`), with 0x01 bytes in the payment id
        let mut nonce_first = vec![0x02, 33, 0x00];
        nonce_first.extend_from_slice(&[0x01; 32]);
        nonce_first.push(0x01);
        nonce_first.extend_from_slice(&[7u8; 32]);
        assert_eq!(pub_key_from_extra(&nonce_first), [7u8; 32]);
        assert_eq!(payment_id_from_extra(&nonce_first), "01".repeat(32));
        // a key with 0x02 0x?? 0x00 inside and no payment id
        let mut key = [9u8; 32];
        key[3..6].copy_from_slice(&[0x02, 0x21, 0x00]);
        let mut no_id = vec![0x01];
        no_id.extend_from_slice(&key);
        no_id.extend_from_slice(&[0u8; 8]);
        assert_eq!(pub_key_from_extra(&no_id), key);
        assert_eq!(payment_id_from_extra(&no_id), "");
    }
}
