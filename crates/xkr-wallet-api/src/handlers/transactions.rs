//! Transactions and sending (`ApiDispatcher::getTransactions`, `getUnconfirmedTransactions`,
//! `getUnconfirmedTransactionsForAddress`, `getTransactionsFromHeight`,
//! `getTransactionsFromHeightToHeight`, `getTransactionsFromHeightWithAddress`,
//! `getTransactionsFromHeightToHeightWithAddress`, `getTxPrivateKey`,
//! `getTransactionDetails`, `sendBasicTransaction`, `sendAdvancedTransaction`,
//! `sendBasicFusionTransaction`, `sendAdvancedFusionTransaction`).
//!
//! * `/transactions/{h}` lists `h <= blockHeight < h + 1000`, `/transactions/{start}/{end}` lists
//!   `start <= blockHeight < end`; a height that does not fit in 64 bits, or `start >= end`, is
//!   400 with an empty body.
//! * The address variants check the address first and keep the transactions with a transfer of
//!   the subwallet whose address is exactly the one given.
//! * `/transactions/unconfirmed/{address}` keeps the sent, unconfirmed transactions with a
//!   transfer of that subwallet.
//! * `/transactions/hash/{hash}` looks at confirmed transactions only (404 for any other hash)
//!   and names the transfers by public spend key (`{publicKey, amount}`).
//! * Sends answer 201 `{transactionHash}`, except `/transactions/send/advanced`, which answers
//!   200.

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_types::Hash;
use xkr_wallet::validate::validate_address;
use xkr_wallet::{FusionRequest, SendRequest, Transaction, Wallet};

use super::{Answer, Fail};
use crate::json::{self, AddressNames, Transfers};
use crate::params;
use crate::respond::Reply;

/// Blocks `/transactions/{h}` covers.
pub const HEIGHT_WINDOW: u64 = 1000;

fn listed<T: Transport + 'static>(wallet: &Wallet<T>, txs: &[Transaction]) -> Reply {
    Reply::json(200, &json::transactions(txs, &AddressNames::of(wallet)))
}

/// GET `/transactions`: every confirmed transaction.
pub fn all<T: Transport + 'static>(wallet: &Wallet<T>) -> Answer {
    let txs = wallet.transactions(None, None)?;
    Ok(listed(wallet, &txs))
}

/// GET `/transactions/unconfirmed`: sent transactions not in a block yet.
pub fn unconfirmed<T: Transport + 'static>(wallet: &Wallet<T>) -> Answer {
    let txs = wallet.unconfirmed(None)?;
    Ok(listed(wallet, &txs))
}

/// GET `/transactions/unconfirmed/{address}`: the unconfirmed transactions with a transfer of
/// the subwallet `address` (the address is not checked; an unknown one lists nothing).
pub fn unconfirmed_for<T: Transport + 'static>(wallet: &Wallet<T>, address: &str) -> Answer {
    let names = AddressNames::of(wallet);
    let txs: Vec<Transaction> = wallet
        .unconfirmed(None)?
        .into_iter()
        .filter(|tx| names.touches(tx, address))
        .collect();
    Ok(Reply::json(200, &json::transactions(&txs, &names)))
}

/// `std::stoull` of a path part that matched `\d+`: only too large values fail.
fn height(text: &str) -> Result<u64, Fail> {
    text.parse::<u64>().map_err(|_| Fail::Param)
}

/// The confirmed transactions with `start <= blockHeight < end` (an `end` that wrapped around
/// lists nothing, as the C++ `uint64_t` sum does), touching `address` if given.
fn in_range<T: Transport + 'static>(
    wallet: &Wallet<T>,
    address: Option<&str>,
    start: u64,
    end: u64,
) -> Answer {
    let names = AddressNames::of(wallet);
    let txs: Vec<Transaction> = wallet
        .transactions(None, None)?
        .into_iter()
        .filter(|tx| tx.block_height >= start && tx.block_height < end)
        .filter(|tx| address.is_none_or(|a| names.touches(tx, a)))
        .collect();
    Ok(Reply::json(200, &json::transactions(&txs, &names)))
}

/// GET `/transactions/{h}` and `/transactions/address/{address}/{h}`.
pub fn from_height<T: Transport + 'static>(
    wallet: &Wallet<T>,
    address: Option<&str>,
    start: &str,
) -> Answer {
    if let Some(a) = address {
        validate_address(a, false)?;
    }
    let start = height(start)?;
    in_range(wallet, address, start, start.wrapping_add(HEIGHT_WINDOW))
}

/// GET `/transactions/{start}/{end}` and `/transactions/address/{address}/{start}/{end}`.
pub fn range<T: Transport + 'static>(
    wallet: &Wallet<T>,
    address: Option<&str>,
    start: &str,
    end: &str,
) -> Answer {
    if let Some(a) = address {
        validate_address(a, false)?;
    }
    let start = height(start)?;
    let end = height(end)?;
    if start >= end {
        return Err(Fail::Param);
    }
    in_range(wallet, address, start, end)
}

/// GET `/transactions/privatekey/{hash}`: `{transactionPrivateKey}` of a transaction this
/// wallet sent, else `TX_PRIVATE_KEY_NOT_FOUND`.
pub fn private_key<T: Transport + 'static>(wallet: &Wallet<T>, hash: &str) -> Answer {
    let hash: Hash = hash.parse().map_err(|_| Fail::Internal)?;
    let key = wallet.tx_private_key(&hash)?;
    Ok(Reply::json(
        200,
        &json!({ "transactionPrivateKey": key.to_hex() }),
    ))
}

/// GET `/transactions/hash/{hash}`: `{transaction}` with `publicKey` transfers, or 404 with an
/// empty body.
pub fn by_hash<T: Transport + 'static>(wallet: &Wallet<T>, hash: &str) -> Answer {
    let hash: Hash = hash.parse().map_err(|_| Fail::Internal)?;
    let found = wallet
        .transactions(None, None)?
        .into_iter()
        .find(|tx| tx.hash == hash);
    Ok(match found {
        Some(tx) => Reply::json(
            200,
            &json!({ "transaction": json::transaction(&tx, Transfers::PublicKeys) }),
        ),
        None => Reply::empty(404),
    })
}

fn sent(status: u16, hash: &Hash) -> Reply {
    Reply::json(status, &json!({ "transactionHash": hash.to_hex() }))
}

/// POST `/transactions/send/basic`: `destination`, `amount`, `paymentID?`; 201.
pub async fn send_basic<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let destination = params::string(body, "destination")?;
    let amount = params::u64_param(body, "amount")?;
    let payment_id = params::opt_string(body, "paymentID")?.unwrap_or_default();
    let hash = Box::pin(wallet.send_basic(&destination, amount, &payment_id)).await?;
    Ok(sent(201, &hash))
}

/// The parameters of POST `/transactions/send/advanced`: `destinations: [{address, amount}]`,
/// `mixin?` (default for the height), `fee?` (default the minimum fee), `sourceAddresses?`,
/// `paymentID?`, `changeAddress?` (empty: the primary address), `unlockTime?`.
pub fn advanced_request(body: &Value) -> Result<SendRequest, Fail> {
    let destinations = params::elements(params::value(body, "destinations")?)
        .into_iter()
        .map(|d| {
            Ok((
                params::string(d, "address")?,
                params::u64_param(d, "amount")?,
            ))
        })
        .collect::<Result<Vec<_>, Fail>>()?;
    let mixin = params::opt_u64(body, "mixin")?;
    let fee = params::opt_u64(body, "fee")?;
    let source_addresses = params::opt_strings(body, "sourceAddresses")?;
    let payment_id = params::opt_string(body, "paymentID")?.unwrap_or_default();
    let change_address = params::opt_string(body, "changeAddress")?.filter(|a| !a.is_empty());
    let unlock_time = params::opt_u64(body, "unlockTime")?.unwrap_or(0);
    Ok(SendRequest {
        destinations,
        mixin,
        fee,
        payment_id,
        source_addresses,
        change_address,
        unlock_time,
        extra: Vec::new(),
    })
}

/// POST `/transactions/send/advanced`: 200 (not 201).
pub async fn send_advanced<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let request = advanced_request(body)?;
    let hash = Box::pin(wallet.send_advanced(request)).await?;
    Ok(sent(200, &hash))
}

/// POST `/transactions/send/fusion/basic`: 201.
pub async fn fusion_basic<T: Transport + 'static>(wallet: &Wallet<T>) -> Answer {
    let hash = Box::pin(wallet.fusion_basic()).await?;
    Ok(sent(201, &hash))
}

/// The parameters of POST `/transactions/send/fusion/advanced`: `destination` (required; empty:
/// the primary address), `mixin?`, `sourceAddresses?`.
pub fn fusion_request(body: &Value) -> Result<FusionRequest, Fail> {
    let destination = params::string(body, "destination")?;
    let mixin = params::opt_u64(body, "mixin")?;
    let source_addresses = params::opt_strings(body, "sourceAddresses")?;
    Ok(FusionRequest {
        mixin,
        source_addresses,
        destination: Some(destination).filter(|d| !d.is_empty()),
        threshold: None,
    })
}

/// POST `/transactions/send/fusion/advanced`: 201.
pub async fn fusion_advanced<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let request = fusion_request(body)?;
    let hash = Box::pin(wallet.fusion_advanced(request)).await?;
    Ok(sent(201, &hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advanced_parameters() {
        let r = advanced_request(&json!({
            "destinations": [{"address": "A", "amount": 5}, {"address": "B", "amount": 6}],
            "mixin": 3, "fee": 12, "sourceAddresses": ["S"], "paymentID": "p",
            "changeAddress": "", "unlockTime": 9,
        }))
        .unwrap();
        assert_eq!(r.destinations, vec![("A".into(), 5), ("B".into(), 6)]);
        assert_eq!((r.mixin, r.fee, r.unlock_time), (Some(3), Some(12), 9));
        assert_eq!(r.source_addresses, vec!["S".to_owned()]);
        assert_eq!(r.payment_id, "p");
        assert_eq!(r.change_address, None);
        let r = advanced_request(&json!({"destinations": []})).unwrap();
        assert!(r.destinations.is_empty());
        assert_eq!((r.mixin, r.fee), (None, None));
        // null iterates as nothing (the engine then reports NO_DESTINATIONS_GIVEN)
        assert!(
            advanced_request(&json!({"destinations": null}))
                .unwrap()
                .destinations
                .is_empty()
        );
        assert_eq!(advanced_request(&json!({})), Err(Fail::Param));
        assert_eq!(
            advanced_request(&json!({"destinations": [{"address": "A"}]})),
            Err(Fail::Param)
        );
        assert_eq!(
            advanced_request(&json!({"destinations": "A"})),
            Err(Fail::Param)
        );
        assert_eq!(
            advanced_request(&json!({"destinations": [], "sourceAddresses": "A"})),
            Err(Fail::Param)
        );
        let r =
            advanced_request(&json!({"destinations": [{"address": "A", "amount": -5}]})).unwrap();
        assert_eq!(r.destinations[0].1, u64::MAX - 4);
    }

    #[test]
    fn fusion_parameters() {
        let r = fusion_request(&json!({"destination": "", "mixin": 2})).unwrap();
        assert_eq!((r.destination, r.mixin, r.threshold), (None, Some(2), None));
        assert_eq!(fusion_request(&json!({})), Err(Fail::Param));
    }

    #[test]
    fn heights() {
        assert_eq!(height("0042"), Ok(42));
        assert_eq!(height("18446744073709551616"), Err(Fail::Param));
    }
}
