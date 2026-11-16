//! Block and transaction queries: `getBlockHashes`, `getTransactionHashes`, `getTransactions`,
//! `getUnconfirmedTransactionHashes`, `getTransaction` (`src/wallet_service/wallet_service.cpp`
//! and `src/wallet/wallet_green.cpp` `WalletGreen::getBlockHashes`, `getTransactions`,
//! `getTransactionsInBlocks`, `getUnconfirmedTransactions`, `getTransaction`).
//!
//! Block-indexed queries follow `WalletGreen::getTransactionsInBlocks`: block index 0 and a
//! count of 0 are `WRONG_PARAMETERS`; an index at or past the wallet's block count finds
//! nothing (`OBJECT_NOT_FOUND` of the service); each block with wallet transactions is listed
//! with the hash of the block before it (C++ reads `m_blockchain[height - 1]`); and, as
//! `filterTransactions` does, a block whose transactions are all filtered out stays in the
//! answer with an empty list. The transactions of a block are listed in their order in the
//! block (the order in which `WalletGreen` files them under the block's height as it
//! processes the block).

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_types::Hash;
use xkr_wallet::{TransactionInfo, TxState};

use super::MethodResult;
use crate::errors::{AppError, ServiceCode, WalletCode};
use crate::model;
use crate::params::{Invalid, Params};
use crate::service::Service;

/// `WalletService::getBlockHashes`.
pub async fn get_block_hashes<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let first = p.u32("firstBlockIndex")?;
    let count = p.u32("blockCount")?;
    let (Some(first), Some(count)) = (first, count) else {
        return Err(Invalid.into());
    };
    let _ready = service.serialize().await;
    let hashes = service
        .block_hashes(u64::from(first), u64::from(count))
        .await?;
    Ok(json!({ "blockHashes": hashes.iter().map(Hash::to_hex).collect::<Vec<_>>() }))
}

/// The parameters of `getTransactions` and `getTransactionHashes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockQuery {
    /// Only transactions with a transfer to or from one of these addresses (all if empty).
    pub addresses: Vec<String>,
    /// Start at the block with this hash (`blockHash` given and not empty).
    pub block_hash: String,
    /// Start at this block index (`UINT32_MAX` when not given).
    pub first_block_index: u32,
    /// Blocks to look at.
    pub block_count: u32,
    /// Only transactions with this payment id (all if empty).
    pub payment_id: String,
}

impl BlockQuery {
    /// `GetTransactions::Request::serialize`: exactly one of `blockHash` and
    /// `firstBlockIndex`, and `blockCount`.
    pub fn read(params: &Value) -> Result<Self, Invalid> {
        let p = Params::new(params)?;
        let addresses = p.strings("addresses")?.unwrap_or_default();
        let block_hash = p.string("blockHash")?;
        let first_block_index = p.u32("firstBlockIndex")?;
        if block_hash.is_some() == first_block_index.is_some() {
            return Err(Invalid);
        }
        let block_count = p.u32("blockCount")?.ok_or(Invalid)?;
        let payment_id = p.string("paymentId")?.unwrap_or_default();
        Ok(Self {
            addresses,
            block_hash: block_hash.unwrap_or_default(),
            first_block_index: first_block_index.unwrap_or(u32::MAX),
            block_count,
            payment_id,
        })
    }
}

/// The confirmed transactions of the blocks `index .. index + count` that have any, with the
/// hash C++ lists for each block; `None` if `index` is past the wallet's blocks.
async fn in_blocks<T: Transport + 'static>(
    service: &Service<T>,
    index: u32,
    count: u32,
) -> Result<Option<Vec<(Hash, Vec<TransactionInfo>)>>, AppError> {
    if count == 0 || index == 0 {
        return Err(WalletCode::WrongParameters.into());
    }
    let total = service.block_count();
    let start = u64::from(index);
    if start >= total {
        return Ok(None);
    }
    let stop = total.min(start + u64::from(count));
    let mut by_height: BTreeMap<u64, Vec<TransactionInfo>> = BTreeMap::new();
    for tx in service.wallet().transactions(Some(start..stop), None)? {
        by_height
            .entry(tx.block_height)
            .or_default()
            .push(TransactionInfo {
                transaction: tx,
                state: TxState::Succeeded,
            });
    }
    // the hashes of the blocks before, fetched in runs of consecutive heights
    let wanted: Vec<u64> = by_height.keys().map(|h| h - 1).collect();
    let mut hashes: BTreeMap<u64, Hash> = BTreeMap::new();
    let mut i = 0;
    while i < wanted.len() {
        let mut j = i + 1;
        while j < wanted.len() && wanted[j] == wanted[j - 1] + 1 {
            j += 1;
        }
        let run = service.block_hashes(wanted[i], (j - i) as u64).await?;
        for (k, hash) in run.into_iter().enumerate() {
            hashes.insert(wanted[i] + k as u64, hash);
        }
        i = j;
    }
    by_height
        .into_iter()
        .map(|(height, txs)| {
            hashes
                .get(&(height - 1))
                .map(|h| (*h, txs))
                .ok_or_else(|| WalletCode::InternalWalletError.into())
        })
        .collect::<Result<Vec<_>, AppError>>()
        .map(Some)
}

/// `TransactionsInBlockInfoFilter::checkTransaction` on a `TransactionRpcInfo`.
fn passes(tx: &Value, addresses: &HashSet<&str>, payment_id: Option<&str>) -> bool {
    if let Some(wanted) = payment_id
        && tx["paymentId"].as_str() != Some(wanted)
    {
        return false;
    }
    addresses.is_empty()
        || tx["transfers"].as_array().is_some_and(|transfers| {
            transfers
                .iter()
                .any(|t| t["address"].as_str().is_some_and(|a| addresses.contains(a)))
        })
}

/// The blocks and transactions of a [`BlockQuery`]: `(block hash, transactions)` per block.
async fn query<T: Transport + 'static>(
    service: &Service<T>,
    q: &BlockQuery,
) -> Result<Vec<(String, Vec<Value>)>, AppError> {
    model::validate_addresses(&q.addresses)?;
    let payment_id = if q.payment_id.is_empty() {
        None
    } else {
        model::validate_payment_id(&q.payment_id)?;
        Some(q.payment_id.to_ascii_lowercase())
    };
    let index = if q.block_hash.is_empty() {
        q.first_block_index
    } else {
        let hash = model::parse_hash(&q.block_hash)?;
        let Some(i) = service.block_index_of(&hash).await? else {
            return Err(ServiceCode::ObjectNotFound.into());
        };
        u32::try_from(i).unwrap_or(u32::MAX)
    };
    let Some(blocks) = in_blocks(service, index, q.block_count).await? else {
        return Err(ServiceCode::ObjectNotFound.into());
    };
    let infos: Vec<TransactionInfo> = blocks.iter().flat_map(|(_, t)| t.clone()).collect();
    let mut all = service.transactions_json(&infos).await.into_iter();
    let addresses: HashSet<&str> = q.addresses.iter().map(String::as_str).collect();
    Ok(blocks
        .iter()
        .map(|(hash, txs)| {
            let kept: Vec<Value> = all
                .by_ref()
                .take(txs.len())
                .filter(|tx| passes(tx, &addresses, payment_id.as_deref()))
                .collect();
            (hash.to_hex(), kept)
        })
        .collect())
}

/// `WalletService::getTransactionHashes`.
pub async fn get_transaction_hashes<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let q = BlockQuery::read(params)?;
    let _ready = service.serialize().await;
    let items: Vec<Value> = query(service, &q)
        .await?
        .into_iter()
        .map(|(hash, txs)| {
            let hashes: Vec<&Value> = txs.iter().map(|t| &t["transactionHash"]).collect();
            json!({ "blockHash": hash, "transactionHashes": hashes })
        })
        .collect();
    Ok(json!({ "items": items }))
}

/// `WalletService::getTransactions`.
pub async fn get_transactions<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let q = BlockQuery::read(params)?;
    let _ready = service.serialize().await;
    let items: Vec<Value> = query(service, &q)
        .await?
        .into_iter()
        .map(|(hash, txs)| json!({ "blockHash": hash, "transactions": txs }))
        .collect();
    Ok(json!({ "items": items }))
}

/// `WalletService::getUnconfirmedTransactionHashes`: the sent transactions not in a block yet.
pub async fn get_unconfirmed_transaction_hashes<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let addresses = Params::new(params)?
        .strings("addresses")?
        .unwrap_or_default();
    let _ready = service.serialize().await;
    model::validate_addresses(&addresses)?;
    let infos: Vec<TransactionInfo> = service
        .wallet()
        .unconfirmed(None)?
        .into_iter()
        .map(|transaction| TransactionInfo {
            transaction,
            state: TxState::Succeeded,
        })
        .collect();
    let wanted: HashSet<&str> = addresses.iter().map(String::as_str).collect();
    let hashes: Vec<Value> = service
        .transactions_json(&infos)
        .await
        .into_iter()
        .filter(|tx| passes(tx, &wanted, None))
        .map(|tx| tx["transactionHash"].clone())
        .collect();
    Ok(json!({ "transactionHashes": hashes }))
}

/// `WalletService::getTransaction`: any transaction of the wallet, also pending, cancelled and
/// delayed ones.
pub async fn get_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let text = Params::new(params)?.required_string("transactionHash")?;
    let _ready = service.serialize().await;
    let hash = model::parse_hash(&text)?;
    let info = service
        .wallet()
        .transaction(&hash)
        .filter(|i| i.state != TxState::Deleted)
        .ok_or_else(|| AppError::wallet(WalletCode::ObjectNotFound))?;
    let tx = service
        .transactions_json(std::slice::from_ref(&info))
        .await
        .pop()
        .unwrap_or(Value::Null);
    Ok(json!({ "transaction": tx }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_query_needs_one_start_and_a_count() {
        let both = json!({ "blockHash": "ab", "firstBlockIndex": 1, "blockCount": 1 });
        assert_eq!(BlockQuery::read(&both), Err(Invalid));
        let neither = json!({ "blockCount": 1 });
        assert_eq!(BlockQuery::read(&neither), Err(Invalid));
        let no_count = json!({ "firstBlockIndex": 1 });
        assert_eq!(BlockQuery::read(&no_count), Err(Invalid));
        let q = BlockQuery::read(&json!({ "blockHash": "", "blockCount": 2 })).unwrap();
        assert_eq!((q.first_block_index, q.block_count), (u32::MAX, 2));
        let q = BlockQuery::read(&json!({ "firstBlockIndex": 7, "blockCount": 2 })).unwrap();
        assert_eq!((q.first_block_index, q.block_hash.as_str()), (7, ""));
    }

    #[test]
    fn filter_by_address_and_payment_id() {
        let tx = json!({
            "paymentId": "ab".repeat(32),
            "transfers": [{"address": "A", "amount": 1, "type": 0}]
        });
        let none = HashSet::new();
        let a: HashSet<&str> = ["A"].into_iter().collect();
        let b: HashSet<&str> = ["B"].into_iter().collect();
        assert!(passes(&tx, &none, None));
        assert!(passes(&tx, &a, None));
        assert!(!passes(&tx, &b, None));
        let pid = "ab".repeat(32);
        assert!(passes(&tx, &a, Some(&pid)));
        assert!(!passes(&tx, &a, Some(&"cd".repeat(32))));
    }
}
