//! What walletd shows of a transaction (`src/wallet_service/payment_service_json_rpc_messages.h`
//! `TransactionRpcInfo`, `TransferRpcInfo`; `src/wallet_service/wallet_service.cpp`
//! `convertTransactionWithTransfersToTransactionRpcInfo`) and the address, key and payment id
//! checks walletd applies to its parameters.
//!
//! `WalletGreen` keeps a list of transfers per transaction: for a transaction it sent, the
//! destinations and the change (`pushBackOutgoingTransfers`); when the transaction is seen in
//! the pool or a block, `updateTransactionTransfers` adds or adjusts one input (negative) and
//! one output (positive) transfer per own address, and transfers with an empty address for the
//! inputs and outputs of other wallets. [`wallet_transfers`] rebuilds that list from the
//! engine's record: the net amount per own address and the destinations of a sent transaction
//! (or the transfers imported from a C++ container).

use std::collections::{BTreeSet, HashMap};

use serde_json::{Map, Value, json};
use xkr_types::{Hash, PublicKey, SecretKey, payment_id};
use xkr_wallet::{Transaction, TransactionInfo, TransferKind, TxState};
use xkr_wallet_core::address::{ADDRESS_PREFIX, ADDRESS_PREFIX_ALT};
use xkr_wallet_core::{Address, base58};

use crate::errors::{AppError, ServiceCode, WalletCode};

/// `WALLET_UNCONFIRMED_TRANSACTION_HEIGHT`: the `blockIndex` of a transaction not in a block.
pub const UNCONFIRMED_BLOCK_INDEX: u32 = u32::MAX;

/// A C++ `uint64_t` as `JsonOutputStreamSerializer` writes it (through `int64_t`).
#[must_use]
pub fn uint(v: u64) -> Value {
    Value::from(v as i64)
}

// ---------------------------------------------------------------------------------------------
// parameter checks

/// `cryptonote::validateAddress` (`Currency::parseAccountAddressString`): a standard address
/// with the `SEKR` or the `Xkr` prefix, valid checksum and valid keys. Integrated addresses and
/// text with surrounding whitespace are not addresses here.
#[must_use]
pub fn standard_address(text: &str) -> Option<Address> {
    if text.trim() != text {
        return None;
    }
    Address::parse(text).ok().filter(|a| a.payment_id.is_none())
}

/// `validateAddresses`: `BAD_ADDRESS` for the first address that is not valid.
pub fn validate_addresses<S: AsRef<str>>(addresses: &[S]) -> Result<(), AppError> {
    if addresses
        .iter()
        .all(|a| standard_address(a.as_ref()).is_some())
    {
        Ok(())
    } else {
        Err(WalletCode::BadAddress.into())
    }
}

/// `checkPaymentId`: 64 hex characters (either case).
#[must_use]
pub fn check_payment_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `validatePaymentId`: `WRONG_PAYMENT_ID_FORMAT` unless [`check_payment_id`].
pub fn validate_payment_id(text: &str) -> Result<(), AppError> {
    if check_payment_id(text) {
        Ok(())
    } else {
        Err(ServiceCode::WrongPaymentIdFormat.into())
    }
}

/// `common::podFromHex` for 32 bytes: exactly 64 hex characters.
#[must_use]
pub fn hex32(text: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if text.len() != 64 {
        return None;
    }
    hex::decode_to_slice(text, &mut out).ok()?;
    Some(out)
}

/// `parseHash`: `WRONG_HASH_FORMAT` unless 64 hex characters.
pub fn parse_hash(text: &str) -> Result<Hash, AppError> {
    hex32(text)
        .map(Hash)
        .ok_or_else(|| ServiceCode::WrongHashFormat.into())
}

/// A secret key in hex (`WRONG_KEY_FORMAT` otherwise).
pub fn parse_secret_key(text: &str) -> Result<SecretKey, AppError> {
    hex32(text)
        .map(SecretKey)
        .ok_or_else(|| ServiceCode::WrongKeyFormat.into())
}

/// A public key in hex (`WRONG_KEY_FORMAT` otherwise).
pub fn parse_public_key(text: &str) -> Result<PublicKey, AppError> {
    hex32(text)
        .map(PublicKey)
        .ok_or_else(|| ServiceCode::WrongKeyFormat.into())
}

/// `decodeIntegratedAddress`: the standard address (with the integrated address's prefix) and
/// the payment id text exactly as stored in the address.
pub fn decode_integrated_address(text: &str) -> Result<(String, String), AppError> {
    let (prefix, data) = base58::decode_addr(text).ok_or(WalletCode::BadAddress)?;
    if prefix != ADDRESS_PREFIX && prefix != ADDRESS_PREFIX_ALT {
        return Err(WalletCode::BadAddress.into());
    }
    // `decoded.substr(0, 64)`: shorter data gives a shorter (and so invalid) payment id
    let split = data.len().min(64);
    let payment_id = String::from_utf8_lossy(&data[..split]).into_owned();
    validate_payment_id(&payment_id)?;
    let keys = &data[split..];
    if keys.len() != 64 {
        return Err(WalletCode::BadAddress.into());
    }
    let address = base58::encode_addr(prefix, keys);
    validate_addresses(&[&address])?;
    Ok((address, payment_id))
}

/// `WalletService::createIntegratedAddress` after its checks: `SEKR` prefix, the payment id
/// text as given, then the keys.
#[must_use]
pub fn integrated_address(address: &Address, payment_id: &str) -> String {
    let mut data = Vec::with_capacity(128);
    data.extend_from_slice(payment_id.as_bytes());
    data.extend_from_slice(&address.spend.0);
    data.extend_from_slice(&address.view.0);
    base58::encode_addr(ADDRESS_PREFIX, &data)
}

// ---------------------------------------------------------------------------------------------
// transactions

/// `WalletTransferType`: usual.
pub const TRANSFER_USUAL: u8 = 0;

/// One entry of `transfers` (`TransferRpcInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// `type`: 0 usual, 1 donation, 2 change.
    pub kind: u8,
    /// `address`; empty for the inputs and outputs of other wallets.
    pub address: String,
    /// `amount`: negative for money leaving the address.
    pub amount: i64,
}

impl Transfer {
    fn usual(address: &str, amount: i64) -> Self {
        Self {
            kind: TRANSFER_USUAL,
            address: address.to_owned(),
            amount,
        }
    }

    /// The JSON object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "type": self.kind, "address": self.address, "amount": self.amount })
    }
}

/// What the daemon's copy of a transaction adds to the engine's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxFacts {
    /// The full `extra`.
    pub extra: Vec<u8>,
    /// Sum of the inputs.
    pub total_in: u64,
    /// Sum of the outputs.
    pub total_out: u64,
}

impl TxFacts {
    /// The facts of a decoded transaction.
    #[must_use]
    pub fn of(tx: &xkr_types::Transaction) -> Self {
        let total_in = tx
            .prefix
            .inputs
            .iter()
            .map(|i| match i {
                xkr_types::TxInput::Key { amount, .. } => *amount,
                xkr_types::TxInput::Coinbase { .. } => 0,
            })
            .fold(0u64, u64::wrapping_add);
        let total_out = tx
            .prefix
            .outputs
            .iter()
            .map(|o| o.amount)
            .fold(0u64, u64::wrapping_add);
        Self {
            extra: tx.prefix.extra.clone(),
            total_in,
            total_out,
        }
    }
}

fn is_output(amount: i64) -> bool {
    amount > 0
}

/// `getKnownTransfersMap` for one address and direction.
fn known(list: &[Transfer], address: &str, output: bool) -> i64 {
    list.iter()
        .filter(|t| t.address == address && is_output(t.amount) == output)
        .map(|t| t.amount)
        .sum()
}

/// `eraseTransfers` with the predicate of `eraseTransfersByAddress`.
fn erase_by_address(list: &mut Vec<Transfer>, address: &str, output: bool) {
    list.retain(|t| !(is_output(t.amount) == output && t.address == address));
}

/// `eraseForeignTransfers`.
fn erase_foreign(list: &mut Vec<Transfer>, mine: &[&str], output: bool) {
    list.retain(|t| is_output(t.amount) != output || mine.contains(&t.address.as_str()));
}

/// `adjustTransfer`: the first transfer of `address` in the direction of `amount` takes the
/// amount, later ones are removed; a new usual transfer is appended if there is none.
fn adjust(list: &mut Vec<Transfer>, address: &str, amount: i64) {
    let output = is_output(amount);
    let mut found = false;
    list.retain_mut(|t| {
        if is_output(t.amount) != output || t.address != address {
            return true;
        }
        if found {
            return false;
        }
        t.amount = amount;
        found = true;
        true
    });
    if !found {
        list.push(Transfer::usual(address, amount));
    }
}

/// `updateAddressTransfers`.
fn update_address(list: &mut Vec<Transfer>, address: &str, target: i64, output: bool) {
    let known = known(list, address, output);
    if known == target {
        return;
    }
    if known == 0 {
        list.push(Transfer::usual(address, target));
    } else if target == 0 {
        erase_by_address(list, address, output);
    } else {
        adjust(list, address, target);
    }
}

/// `updateUnknownTransfers`.
fn update_unknown(
    list: &mut Vec<Transfer>,
    mine: &[&str],
    known: i64,
    my_amount: i64,
    total: i64,
    output: bool,
) {
    if known.abs() > total.abs() {
        erase_foreign(list, mine, output);
        if total == my_amount {
            erase_by_address(list, "", output);
        } else {
            adjust(list, "", total - my_amount);
        }
    } else if known == total {
        erase_by_address(list, "", output);
    } else {
        adjust(list, "", total - known);
    }
}

/// The wallet's addresses, indexed by spend key and by text.
#[derive(Debug)]
pub struct Own<'a> {
    list: &'a [(PublicKey, String)],
    by_key: HashMap<PublicKey, usize>,
    by_address: HashMap<&'a str, usize>,
}

impl<'a> Own<'a> {
    /// The index of `list` (addresses with their spend keys, in creation order).
    #[must_use]
    pub fn new(list: &'a [(PublicKey, String)]) -> Self {
        Self {
            list,
            by_key: list.iter().enumerate().map(|(i, (k, _))| (*k, i)).collect(),
            by_address: list
                .iter()
                .enumerate()
                .map(|(i, (_, a))| (a.as_str(), i))
                .collect(),
        }
    }
}

/// The `WalletGreen` transfers of `tx` (`pushBackOutgoingTransfers` +
/// `updateTransactionTransfers`). `own` lists the wallet's addresses in creation order;
/// `pending` is true while the transaction is not in a block (or not sent).
///
/// * A transaction the wallet sent starts from its destinations and change; a fusion
///   transaction starts empty (C++ passes no destinations), its destination entry only says
///   how much went to the destination address.
/// * Per own address, the output part is what the destinations say the address received and
///   the input part is the rest of the net amount. While a sent transaction is pending the
///   engine's net amount holds only the inputs and the change (as `storeSentTransaction`), so
///   only the change is taken off. Without destinations a positive net amount is an output, a
///   negative one an input.
/// * With `facts` (the daemon's copy), a transaction the wallet did not send also gets the
///   empty-address transfers for the inputs and outputs of other wallets.
#[must_use]
pub fn wallet_transfers(
    tx: &Transaction,
    pending: bool,
    own: &Own<'_>,
    facts: Option<&TxFacts>,
) -> Vec<Transfer> {
    wallet_transfers_with(tx, pending, own, facts, &[])
}

/// [`wallet_transfers`] knowing what each own address received in `tx` (`received`, the sum
/// of the address's outputs of the transaction, as the wallet found them). Without
/// destinations the address then has an output part of what it received and an input part
/// of the rest of its net amount, as the `ContainerAmounts` input and output of
/// `WalletGreen::transactionUpdated` (a transaction another program sent from this wallet's
/// keys shows the spent inputs and the change apart).
#[must_use]
pub fn wallet_transfers_with(
    tx: &Transaction,
    pending: bool,
    own: &Own<'_>,
    facts: Option<&TxFacts>,
    received: &[(PublicKey, u64)],
) -> Vec<Transfer> {
    let fusion = tx.fee == 0 && !tx.is_coinbase;
    let mut list: Vec<Transfer> = if fusion {
        Vec::new()
    } else {
        tx.destinations
            .iter()
            .map(|d| Transfer {
                kind: d.kind.number(),
                address: d.address.clone(),
                amount: d.amount,
            })
            .collect()
    };
    let (mut my_in, mut my_out) = (0i64, 0i64);
    let mut in_addresses: Vec<&str> = Vec::new();
    let mut out_addresses: Vec<&str> = Vec::new();
    // the own addresses taking part, in creation order
    let involved: BTreeSet<usize> = tx
        .transfers
        .iter()
        .filter_map(|(k, _)| own.by_key.get(k).copied())
        .chain(
            tx.destinations
                .iter()
                .filter_map(|d| own.by_address.get(d.address.as_str()).copied()),
        )
        .collect();
    for (key, address) in involved.into_iter().map(|i| &own.list[i]) {
        let listed = |change_only: bool| -> i64 {
            tx.destinations
                .iter()
                .filter(|d| d.address == *address && d.amount > 0)
                .filter(|d| !change_only || d.kind == TransferKind::Change)
                .map(|d| d.amount)
                .sum()
        };
        let listed_out = listed(false);
        if !tx.involves(key) && listed_out == 0 {
            continue;
        }
        let net = tx.amount_for(key);
        let got: Option<i64> = received
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, a)| i64::try_from(*a).unwrap_or(i64::MAX));
        let (mut target_in, mut target_out) = if tx.destinations.is_empty() {
            match got {
                Some(out) => (net - out, out),
                None if net >= 0 => (0, net),
                None => (net, 0),
            }
        } else if pending {
            (net - listed(true), listed_out)
        } else {
            (net - listed_out, listed_out)
        };
        if target_in > 0 {
            target_out += target_in;
            target_in = 0;
        }
        update_address(&mut list, address, target_in, false);
        update_address(&mut list, address, target_out, true);
        my_in += target_in;
        my_out += target_out;
        if target_in != 0 {
            in_addresses.push(address);
        }
        if target_out != 0 {
            out_addresses.push(address);
        }
    }
    if let Some(facts) = facts.filter(|_| tx.destinations.is_empty()) {
        let known_in: i64 = list
            .iter()
            .filter(|t| !t.address.is_empty() && t.amount < 0)
            .map(|t| t.amount)
            .sum();
        let known_out: i64 = list
            .iter()
            .filter(|t| !t.address.is_empty() && t.amount > 0)
            .map(|t| t.amount)
            .sum();
        update_unknown(
            &mut list,
            &in_addresses,
            known_in,
            my_in,
            -(facts.total_in as i64),
            false,
        );
        update_unknown(
            &mut list,
            &out_addresses,
            known_out,
            my_out,
            facts.total_out as i64,
            true,
        );
    }
    list
}

/// Whether walletd shows `info` as not in a block (`blockIndex`
/// [`UNCONFIRMED_BLOCK_INDEX`]).
#[must_use]
pub fn is_pending(info: &TransactionInfo) -> bool {
    info.state != TxState::Succeeded || info.transaction.is_unconfirmed()
}

/// `TransactionRpcInfo` of a transaction with its state, transfers and full `extra`.
#[must_use]
pub fn transaction_json(info: &TransactionInfo, transfers: &[Transfer], extra: &[u8]) -> Value {
    let tx = &info.transaction;
    let block_index = if is_pending(info) {
        UNCONFIRMED_BLOCK_INDEX
    } else {
        tx.block_height as u32
    };
    let mut out = Map::new();
    out.insert("state".into(), json!(info.state.number()));
    out.insert("transactionHash".into(), json!(tx.hash.to_hex()));
    out.insert("blockIndex".into(), json!(block_index));
    out.insert("timestamp".into(), uint(tx.timestamp));
    out.insert("isBase".into(), json!(tx.is_coinbase));
    out.insert("unlockTime".into(), uint(tx.unlock_time));
    out.insert("amount".into(), json!(tx.total_amount()));
    out.insert("fee".into(), uint(tx.fee));
    out.insert(
        "transfers".into(),
        Value::Array(transfers.iter().map(Transfer::to_json).collect()),
    );
    out.insert("extra".into(), json!(hex::encode(extra)));
    out.insert(
        "paymentId".into(),
        json!(payment_id(extra).map_or_else(String::new, |p| p.to_hex())),
    );
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_wallet::TransferDestination;
    use xkr_wallet_core::WalletKeys;

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    fn own() -> Vec<(PublicKey, String)> {
        vec![(key(1), "A".to_owned()), (key(2), "B".to_owned())]
    }

    fn dest(address: &str, amount: i64, kind: TransferKind) -> TransferDestination {
        TransferDestination {
            address: address.to_owned(),
            amount,
            kind,
        }
    }

    fn pairs(list: &[Transfer]) -> Vec<(&str, i64, u8)> {
        list.iter()
            .map(|t| (t.address.as_str(), t.amount, t.kind))
            .collect()
    }

    #[test]
    fn sent_transaction_lists_destinations_change_and_the_input() {
        // A sends 100 to K with 40 change from an input of 150 (fee 10)
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.transfers = vec![(key(1), -110)];
        tx.destinations = vec![
            dest("K", 100, TransferKind::Usual),
            dest("A", 40, TransferKind::Change),
        ];
        let list = wallet_transfers(&tx, false, &Own::new(&own()), None);
        assert_eq!(
            pairs(&list),
            vec![("K", 100, 0), ("A", 40, 2), ("A", -150, 0)]
        );
        assert_eq!(
            list.iter().map(|t| t.amount).sum::<i64>() - 100,
            tx.total_amount()
        );
    }

    #[test]
    fn send_to_an_own_address_keeps_its_destination() {
        // A sends 130 to B, change 60 to A, input 200 from A
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.transfers = vec![(key(1), -140), (key(2), 130)];
        tx.destinations = vec![
            dest("B", 130, TransferKind::Usual),
            dest("A", 60, TransferKind::Change),
        ];
        let list = wallet_transfers(&tx, false, &Own::new(&own()), None);
        assert_eq!(
            pairs(&list),
            vec![("B", 130, 0), ("A", 60, 2), ("A", -200, 0)]
        );
    }

    #[test]
    fn fusion_lists_input_then_output() {
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.transfers = vec![(key(1), 0)];
        tx.destinations = vec![dest("A", 5000, TransferKind::Usual)];
        let list = wallet_transfers(&tx, false, &Own::new(&own()), None);
        assert_eq!(pairs(&list), vec![("A", -5000, 0), ("A", 5000, 0)]);
        // pending: the engine's record holds only the inputs
        tx.transfers = vec![(key(1), -5000)];
        let list = wallet_transfers(&tx, true, &Own::new(&own()), None);
        assert_eq!(pairs(&list), vec![("A", -5000, 0), ("A", 5000, 0)]);
    }

    #[test]
    fn pending_send_to_an_own_address() {
        // the pending record holds A's input and change only (storeSentTransaction)
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.transfers = vec![(key(1), -140)];
        tx.destinations = vec![
            dest("B", 130, TransferKind::Usual),
            dest("A", 60, TransferKind::Change),
        ];
        let list = wallet_transfers(&tx, true, &Own::new(&own()), None);
        assert_eq!(
            pairs(&list),
            vec![("B", 130, 0), ("A", 60, 2), ("A", -200, 0)]
        );
    }

    #[test]
    fn received_transaction_gets_the_foreign_parts_from_the_facts() {
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.block_height = 5;
        tx.transfers = vec![(key(2), 300)];
        let facts = TxFacts {
            extra: vec![],
            total_in: 1000,
            total_out: 990,
        };
        let list = wallet_transfers(&tx, false, &Own::new(&own()), Some(&facts));
        assert_eq!(
            pairs(&list),
            vec![("B", 300, 0), ("", -1000, 0), ("", 690, 0)]
        );
        // without the daemon's copy only the own part is known
        assert_eq!(
            pairs(&wallet_transfers(&tx, false, &Own::new(&own()), None)),
            vec![("B", 300, 0)]
        );
        // a coinbase paying only this wallet has no foreign part
        let mut cb = Transaction::new(Hash([8; 32]));
        cb.is_coinbase = true;
        cb.block_height = 3;
        cb.transfers = vec![(key(1), 500)];
        let facts = TxFacts {
            extra: vec![],
            total_in: 0,
            total_out: 500,
        };
        assert_eq!(
            pairs(&wallet_transfers(
                &cb,
                false,
                &Own::new(&own()),
                Some(&facts)
            )),
            vec![("A", 500, 0)]
        );
    }

    #[test]
    fn transaction_sent_by_another_program_shows_inputs_and_change_apart() {
        // the same keys sent 1 000 000 from A to B with 39 000 130 change, 10 fee: A's net is
        // -1 000 010, A found 39 000 130 of its own outputs in it
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.block_height = 5;
        tx.transfers = vec![(key(1), -1_000_010), (key(2), 1_000_000)];
        let facts = TxFacts {
            extra: vec![],
            total_in: 40_000_140,
            total_out: 40_000_130,
        };
        let received = [(key(1), 39_000_130), (key(2), 1_000_000)];
        let list = wallet_transfers_with(&tx, false, &Own::new(&own()), Some(&facts), &received);
        assert_eq!(
            pairs(&list),
            vec![
                ("A", -40_000_140, 0),
                ("A", 39_000_130, 0),
                ("B", 1_000_000, 0)
            ]
        );
    }

    #[test]
    fn imported_cpp_transfers_stay_as_they_are() {
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.block_height = 7;
        tx.transfers = vec![(key(1), -110)];
        tx.destinations = vec![
            dest("K", 100, TransferKind::Usual),
            dest("A", 40, TransferKind::Change),
            dest("A", -150, TransferKind::Usual),
        ];
        let list = wallet_transfers(&tx, false, &Own::new(&own()), None);
        assert_eq!(
            pairs(&list),
            vec![("K", 100, 0), ("A", 40, 2), ("A", -150, 0)]
        );
    }

    #[test]
    fn json_of_a_pending_transaction() {
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.fee = 10;
        tx.transfers = vec![(key(1), -110)];
        let mut extra = vec![0x01];
        extra.extend_from_slice(&[7; 32]);
        extra.extend_from_slice(&[0x02, 0x21, 0x00]);
        extra.extend_from_slice(&[0xab; 32]);
        let info = TransactionInfo {
            transaction: tx,
            state: TxState::Succeeded,
        };
        let v = transaction_json(&info, &[], &extra);
        assert_eq!(v["blockIndex"], json!(4_294_967_295_u32));
        assert_eq!(v["amount"], json!(-110));
        assert_eq!(v["paymentId"], json!("ab".repeat(32)));
        assert_eq!(v["extra"], json!(hex::encode(&extra)));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "amount",
                "blockIndex",
                "extra",
                "fee",
                "isBase",
                "paymentId",
                "state",
                "timestamp",
                "transactionHash",
                "transfers",
                "unlockTime"
            ]
        );
    }

    #[test]
    fn address_checks() {
        let keys = WalletKeys::generate(&mut xkr_crypto::os_rng());
        let address = keys.address();
        let text = address.to_string();
        assert!(standard_address(&text).is_some());
        assert!(standard_address(&format!(" {text}")).is_none());
        assert!(standard_address("SEKRnope").is_none());
        let pid = "0123456789abcdef".repeat(4);
        let integrated = integrated_address(&address, &pid);
        assert!(standard_address(&integrated).is_none());
        assert_eq!(
            decode_integrated_address(&integrated).unwrap(),
            (text.clone(), pid.clone())
        );
        assert_eq!(decode_integrated_address("SEKRnope").unwrap_err().code, 7);
        // a valid standard address read as an integrated one has no hex payment id
        assert_eq!(decode_integrated_address(&text).unwrap_err().code, 2);
        assert!(check_payment_id(&pid.to_uppercase()));
        assert!(!check_payment_id(&pid[1..]));
        assert_eq!(parse_hash("zz").unwrap_err().code, 3);
        assert_eq!(parse_secret_key("zz").unwrap_err().code, 1);
    }
}
