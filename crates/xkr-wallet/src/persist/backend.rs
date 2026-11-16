//! Format A: the wallet state as a `WalletBackend` file (`src/wallet_backend/wallet_backend.cpp`
//! `WalletBackend::toJSON` / `fromJSON`, `SubWallets::toJSON` / `fromJSON`,
//! `SubWallet::toJSON` / `fromJSON`, `WalletSynchronizer::toJSON` / `fromJSON`).
//!
//! The JSON types and the encryption are `xkr_wallet_files::backend`; this module converts
//! between them and [`WalletState`]. Fields the state does not model are kept and written back,
//! so wallet-backend-js and Hugin files keep their extra data.
//!
//! What the C++ file has no place for is stored in extra fields that C++ and wallet-backend-js
//! ignore when they read the file: per transaction `xkrExtra` (hex), `xkrDestinations` and
//! `xkrSpentKeyImages`, per subwallet `xkrCreationTimestamp`, and in `subWallets` the
//! transactions the node forgot (`xkrCancelledTransactions`, transactions as in
//! `transactions`) and walletd's delayed transactions (`xkrDelayedTransactions`, see
//! [`delayed_to_json`]).

use serde_json::{Value, json};
use xkr_serialization::decode_transaction;
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};
use xkr_wallet_core::BuiltTransaction;
use xkr_wallet_files::backend::{
    SubWalletJson, SubWalletsJson, TransactionInputJson, TransactionJson, TransferJson,
    TxPrivateKeyJson, UnconfirmedInputJson, UnknownFields, WALLET_FILE_FORMAT_VERSION,
    WalletBackendJson, WalletSynchronizerJson,
};

use crate::error::{ErrorCode, WalletError};
use crate::send::Prepared;
use crate::state::{
    OwnedInput, SubWallet, Transaction, TransferDestination, TransferKind, UnconfirmedInput,
    UnknownParts, WalletState,
};
use crate::sync::status::SyncStatus;

/// Transaction field: the full `extra` as hex.
const KEY_EXTRA: &str = "xkrExtra";
/// Transaction field: `[{address, amount, type}]` of a sent transaction.
const KEY_DESTINATIONS: &str = "xkrDestinations";
/// Transaction field: key images of the own outputs a sent transaction spends.
const KEY_SPENT_KEY_IMAGES: &str = "xkrSpentKeyImages";
/// Subwallet field: the `WalletGreen` creation timestamp.
const KEY_CREATION_TIMESTAMP: &str = "xkrCreationTimestamp";
/// `subWallets` field: the transactions the node forgot.
const KEY_CANCELLED: &str = "xkrCancelledTransactions";
/// `subWallets` field: walletd's delayed transactions.
const KEY_DELAYED: &str = "xkrDelayedTransactions";

fn corrupted(what: &str) -> WalletError {
    WalletError::new(ErrorCode::WalletFileCorrupted).with_detail(what.to_owned())
}

/// `WalletBackend::fromJSON`: the state of a parsed file.
///
/// The subwallets come in the order of `publicSpendKeys` (creation order); pending sent
/// transactions (`lockedTransactions`) are read back as pending.
pub(crate) fn state_from_json(json: WalletBackendJson) -> Result<WalletState, WalletError> {
    let WalletBackendJson {
        sub_wallets,
        wallet_synchronizer,
        other: top,
        ..
    } = json;
    let SubWalletsJson {
        public_spend_keys,
        sub_wallet,
        transactions,
        locked_transactions,
        private_view_key,
        is_view_wallet,
        tx_private_keys,
        other: mut sub_wallets_other,
    } = sub_wallets;
    let mut state = WalletState::empty(private_view_key, is_view_wallet)
        .map_err(|_| corrupted("private view key"))?;

    let mut pending: Vec<SubWalletJson> = sub_wallet;
    let mut ordered = Vec::with_capacity(pending.len());
    for key in &public_spend_keys {
        if let Some(pos) = pending.iter().position(|s| s.public_spend_key == *key) {
            ordered.push(pending.remove(pos));
        }
    }
    ordered.extend(pending);
    // only a walletd container can be left without addresses
    let subwallets: Vec<SubWallet> = ordered
        .into_iter()
        .map(|s| subwallet_from_json(s, is_view_wallet))
        .collect();
    state.subwallets = subwallets;
    if !state.subwallets.iter().any(|s| s.is_primary)
        && let Some(first) = state.subwallets.first_mut()
    {
        first.is_primary = true;
    }
    state.keys_changed();

    state.transactions = transactions
        .into_iter()
        .map(transaction_from_json)
        .collect();
    state.locked_transactions = locked_transactions
        .into_iter()
        .map(transaction_from_json)
        .collect();
    state.tx_private_keys = tx_private_keys
        .into_iter()
        .map(|k| (k.transaction_hash, k.tx_private_key))
        .collect();
    if let Some(Value::Array(list)) = sub_wallets_other.remove(KEY_CANCELLED) {
        state.cancelled_transactions = list
            .into_iter()
            .filter_map(|v| serde_json::from_value::<TransactionJson>(v).ok())
            .map(transaction_from_json)
            .collect();
    }
    if let Some(Value::Array(list)) = sub_wallets_other.remove(KEY_DELAYED) {
        for v in &list {
            if let Some(prepared) = delayed_from_json(v) {
                state.delayed_insert(prepared);
            } else {
                tracing::warn!("a delayed transaction of the wallet file is not readable");
            }
        }
    }

    let WalletSynchronizerJson {
        transaction_synchronizer_status,
        start_timestamp,
        start_height,
        other: synchronizer_other,
        ..
    } = wallet_synchronizer;
    state.sync = SyncStatus::from_json(&transaction_synchronizer_status);
    state.start_height = start_height;
    state.start_timestamp = start_timestamp;
    state.unknown = UnknownParts {
        top,
        sub_wallets: sub_wallets_other,
        synchronizer: synchronizer_other,
    };
    Ok(state)
}

/// `WalletBackend::toJSON`.
pub(crate) fn state_to_json(state: &WalletState) -> WalletBackendJson {
    let sub_wallet = state.subwallets.iter().map(subwallet_to_json).collect();
    let mut sub_wallets_other = state.unknown.sub_wallets.clone();
    if !state.cancelled_transactions.is_empty() {
        let list: Vec<Value> = state
            .cancelled_transactions
            .iter()
            .map(|t| serde_json::to_value(transaction_to_json(t)).unwrap_or(Value::Null))
            .collect();
        sub_wallets_other.insert(KEY_CANCELLED.to_owned(), Value::Array(list));
    }
    if !state.delayed.items().is_empty() {
        let list: Vec<Value> = state.delayed.items().iter().map(delayed_to_json).collect();
        sub_wallets_other.insert(KEY_DELAYED.to_owned(), Value::Array(list));
    }
    WalletBackendJson {
        wallet_file_format_version: WALLET_FILE_FORMAT_VERSION,
        sub_wallets: SubWalletsJson {
            public_spend_keys: state
                .subwallets
                .iter()
                .map(|s| s.public_spend_key)
                .collect(),
            sub_wallet,
            transactions: state.transactions.iter().map(transaction_to_json).collect(),
            locked_transactions: state
                .locked_transactions
                .iter()
                .map(transaction_to_json)
                .collect(),
            private_view_key: state.view_secret,
            is_view_wallet: state.is_view_wallet,
            tx_private_keys: state
                .tx_private_keys
                .iter()
                .map(|(h, k)| TxPrivateKeyJson {
                    transaction_hash: *h,
                    tx_private_key: *k,
                    other: UnknownFields::new(),
                })
                .collect(),
            other: sub_wallets_other,
        },
        wallet_synchronizer: WalletSynchronizerJson {
            transaction_synchronizer_status: state.sync.to_json(),
            start_timestamp: state.start_timestamp,
            start_height: state.start_height,
            private_view_key: state.view_secret,
            other: state.unknown.synchronizer.clone(),
        },
        other: state.unknown.top.clone(),
    }
}

fn subwallet_from_json(json: SubWalletJson, is_view_wallet: bool) -> SubWallet {
    let SubWalletJson {
        public_spend_key,
        private_spend_key,
        address,
        sync_start_timestamp,
        unspent_inputs,
        locked_inputs,
        spent_inputs,
        sync_start_height,
        is_primary_address,
        unconfirmed_incoming_amounts,
        mut other,
    } = json;
    let creation_timestamp = other
        .remove(KEY_CREATION_TIMESTAMP)
        .and_then(|v| v.as_u64());
    let inputs = |list: Vec<TransactionInputJson>| -> Vec<OwnedInput> {
        list.into_iter()
            .map(|i| input_from_json(i, is_view_wallet))
            .collect()
    };
    SubWallet {
        public_spend_key,
        private_spend_key: (!is_view_wallet && !private_spend_key.is_zero())
            .then_some(private_spend_key),
        address,
        sync_start_timestamp,
        sync_start_height,
        is_primary: is_primary_address,
        unspent: inputs(unspent_inputs),
        locked: inputs(locked_inputs),
        spent: inputs(spent_inputs),
        unconfirmed_incoming: unconfirmed_incoming_amounts
            .into_iter()
            .map(|u| UnconfirmedInput {
                amount: u.amount,
                key: u.key,
                parent_transaction_hash: u.parent_transaction_hash,
                other: u.other,
            })
            .collect(),
        creation_timestamp,
        other,
    }
}

fn subwallet_to_json(sub: &SubWallet) -> SubWalletJson {
    let mut other = sub.other.clone();
    if let Some(ts) = sub.creation_timestamp {
        other.insert(KEY_CREATION_TIMESTAMP.to_owned(), json!(ts));
    }
    SubWalletJson {
        public_spend_key: sub.public_spend_key,
        private_spend_key: sub.private_spend_key.unwrap_or(SecretKey::ZERO),
        address: sub.address.clone(),
        sync_start_timestamp: sub.sync_start_timestamp,
        unspent_inputs: sub.unspent.iter().map(input_to_json).collect(),
        locked_inputs: sub.locked.iter().map(input_to_json).collect(),
        spent_inputs: sub.spent.iter().map(input_to_json).collect(),
        sync_start_height: sub.sync_start_height,
        is_primary_address: sub.is_primary,
        unconfirmed_incoming_amounts: sub
            .unconfirmed_incoming
            .iter()
            .map(|u| UnconfirmedInputJson {
                amount: u.amount,
                key: u.key,
                parent_transaction_hash: u.parent_transaction_hash,
                other: u.other.clone(),
            })
            .collect(),
        other,
    }
}

fn input_from_json(json: TransactionInputJson, is_view_wallet: bool) -> OwnedInput {
    OwnedInput {
        key_image: json.key_image,
        amount: json.amount,
        block_height: json.block_height,
        transaction_public_key: json.transaction_public_key,
        transaction_index: json.transaction_index,
        global_output_index: (!is_view_wallet).then_some(json.global_output_index),
        key: json.key,
        spend_height: json.spend_height,
        unlock_time: json.unlock_time,
        parent_transaction_hash: json.parent_transaction_hash,
        other: json.other,
    }
}

fn input_to_json(input: &OwnedInput) -> TransactionInputJson {
    TransactionInputJson {
        key_image: input.key_image,
        amount: input.amount,
        block_height: input.block_height,
        transaction_public_key: input.transaction_public_key,
        transaction_index: input.transaction_index,
        global_output_index: input.global_output_index.unwrap_or(0),
        key: input.key,
        spend_height: input.spend_height,
        unlock_time: input.unlock_time,
        parent_transaction_hash: input.parent_transaction_hash,
        other: input.other.clone(),
    }
}

fn transaction_from_json(json: TransactionJson) -> Transaction {
    let TransactionJson {
        transfers,
        hash,
        fee,
        block_height,
        timestamp,
        payment_id,
        unlock_time,
        is_coinbase_transaction,
        mut other,
    } = json;
    let extra = other
        .remove(KEY_EXTRA)
        .and_then(|v| v.as_str().and_then(|s| hex::decode(s).ok()));
    let destinations = other
        .remove(KEY_DESTINATIONS)
        .and_then(|v| v.as_array().cloned())
        .map_or_default(|list| list.iter().filter_map(destination_from_json).collect());
    let spent_key_images = other
        .remove(KEY_SPENT_KEY_IMAGES)
        .and_then(|v| v.as_array().cloned())
        .map_or_default(|list| {
            list.iter()
                .filter_map(|v| v.as_str().and_then(|s| s.parse::<KeyImage>().ok()))
                .collect()
        });
    Transaction {
        transfers: transfers
            .into_iter()
            .map(|t| (t.public_key, t.amount))
            .collect(),
        hash,
        fee,
        block_height,
        timestamp,
        payment_id,
        unlock_time,
        is_coinbase: is_coinbase_transaction,
        extra,
        destinations,
        spent_key_images,
        other,
    }
}

fn transaction_to_json(tx: &Transaction) -> TransactionJson {
    let mut other = tx.other.clone();
    if let Some(extra) = &tx.extra {
        other.insert(KEY_EXTRA.to_owned(), json!(hex::encode(extra)));
    }
    if !tx.destinations.is_empty() {
        let list: Vec<Value> = tx.destinations.iter().map(destination_to_json).collect();
        other.insert(KEY_DESTINATIONS.to_owned(), Value::Array(list));
    }
    if !tx.spent_key_images.is_empty() {
        let list: Vec<Value> = tx
            .spent_key_images
            .iter()
            .map(|k| json!(k.to_hex()))
            .collect();
        other.insert(KEY_SPENT_KEY_IMAGES.to_owned(), Value::Array(list));
    }
    TransactionJson {
        transfers: tx
            .transfers
            .iter()
            .map(|(k, a)| TransferJson {
                public_key: *k,
                amount: *a,
                other: UnknownFields::new(),
            })
            .collect(),
        hash: tx.hash,
        fee: tx.fee,
        block_height: tx.block_height,
        timestamp: tx.timestamp,
        payment_id: tx.payment_id.clone(),
        unlock_time: tx.unlock_time,
        is_coinbase_transaction: tx.is_coinbase,
        other,
    }
}

fn destination_from_json(v: &Value) -> Option<TransferDestination> {
    Some(TransferDestination {
        address: v.get("address")?.as_str()?.to_owned(),
        amount: v.get("amount")?.as_i64()?,
        kind: TransferKind::from_number(
            v.get("type")
                .and_then(Value::as_u64)
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(0),
        ),
    })
}

fn destination_to_json(d: &TransferDestination) -> Value {
    json!({ "address": d.address, "amount": d.amount, "type": d.kind.number() })
}

/// A delayed transaction as stored in `xkrDelayedTransactions`: `hash`, `transaction` (the
/// serialized transaction, hex), `txPrivateKey`, `fee`, `paymentID`, `inputs` (`{owner,
/// input}` with the input as in `unspentInputs`), `change` (`{owner, amount}` or null),
/// `destinations` (as `xkrDestinations`) and `unconfirmed` (`{owner, input}` with the input as
/// in `unconfirmedIncomingAmounts`).
pub(crate) fn delayed_to_json(p: &Prepared) -> Value {
    let owned =
        |owner: &PublicKey, input: Value| json!({ "owner": owner.to_hex(), "input": input });
    let inputs: Vec<Value> = p
        .inputs
        .iter()
        .map(|(owner, input)| {
            owned(
                owner,
                serde_json::to_value(input_to_json(input)).unwrap_or(Value::Null),
            )
        })
        .collect();
    let unconfirmed: Vec<Value> = p
        .unconfirmed
        .iter()
        .map(|(owner, u)| {
            let json = UnconfirmedInputJson {
                amount: u.amount,
                key: u.key,
                parent_transaction_hash: u.parent_transaction_hash,
                other: u.other.clone(),
            };
            owned(owner, serde_json::to_value(json).unwrap_or(Value::Null))
        })
        .collect();
    let change = p
        .change
        .map(|(owner, amount)| json!({ "owner": owner.to_hex(), "amount": amount }));
    let destinations: Vec<Value> = p.destinations.iter().map(destination_to_json).collect();
    json!({
        "hash": p.built.hash.to_hex(),
        "transaction": hex::encode(&p.built.blob),
        "txPrivateKey": p.built.tx_secret_key.to_hex(),
        "fee": p.fee,
        "paymentID": p.payment_id,
        "inputs": inputs,
        "change": change,
        "destinations": destinations,
        "unconfirmed": unconfirmed,
    })
}

/// The `owner` public key of an item of [`delayed_to_json`].
fn owner_of(item: &Value) -> Option<PublicKey> {
    item.get("owner")?.as_str()?.parse().ok()
}

/// [`delayed_to_json`] read back; `None` if a field is missing or malformed.
pub(crate) fn delayed_from_json(v: &Value) -> Option<Prepared> {
    let text = |key: &str| v.get(key).and_then(Value::as_str);
    let blob = hex::decode(text("transaction")?).ok()?;
    let tx = decode_transaction(&blob).ok()?;
    let fee = v.get("fee")?.as_u64()?;
    let built = BuiltTransaction {
        tx,
        blob,
        hash: text("hash")?.parse::<Hash>().ok()?,
        tx_secret_key: text("txPrivateKey")?.parse::<SecretKey>().ok()?,
        fee,
    };
    let inputs = v
        .get("inputs")?
        .as_array()?
        .iter()
        .map(|item| {
            let json: TransactionInputJson =
                serde_json::from_value(item.get("input")?.clone()).ok()?;
            Some((owner_of(item)?, input_from_json(json, false)))
        })
        .collect::<Option<Vec<_>>>()?;
    let unconfirmed = v
        .get("unconfirmed")?
        .as_array()?
        .iter()
        .map(|item| {
            let json: UnconfirmedInputJson =
                serde_json::from_value(item.get("input")?.clone()).ok()?;
            let input = UnconfirmedInput {
                amount: json.amount,
                key: json.key,
                parent_transaction_hash: json.parent_transaction_hash,
                other: json.other,
            };
            Some((owner_of(item)?, input))
        })
        .collect::<Option<Vec<_>>>()?;
    let change = match v.get("change")? {
        Value::Null => None,
        c => Some((owner_of(c)?, c.get("amount")?.as_u64()?)),
    };
    Some(Prepared {
        built,
        inputs,
        fee,
        payment_id: text("paymentID")?.to_owned(),
        change,
        destinations: v
            .get("destinations")?
            .as_array()?
            .iter()
            .filter_map(destination_from_json)
            .collect(),
        unconfirmed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::subwallet::tests::input;

    #[test]
    fn state_round_trips_through_the_file_json() {
        let keys = xkr_wallet_core::WalletKeys::generate(&mut xkr_crypto::os_rng());
        let mut state =
            WalletState::new_spend(keys.spend_secret, keys.view_secret, 7, false, 1_700_000_000)
                .unwrap();
        let primary = state.primary().public_spend_key;
        let (_, extra_key) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        state.add_subwallet(extra_key, 1_700_000_000).unwrap();
        state.store_input(&primary, input(1, 100, 9));
        state.store_input(&primary, input(2, 200, 9));
        state.mark_spent(&primary, &KeyImage([2; 32]), 10);
        let mut tx = Transaction::new(Hash([3; 32]));
        tx.add_transfer(primary, 100);
        tx.block_height = 9;
        tx.timestamp = 1_600_000_000;
        tx.extra = Some(vec![1, 2, 3]);
        state.add_transaction(tx);
        let mut sent = Transaction::new(Hash([4; 32]));
        sent.add_transfer(primary, -200);
        sent.destinations.push(TransferDestination {
            address: "SEKRx".into(),
            amount: 190,
            kind: TransferKind::Usual,
        });
        sent.spent_key_images = vec![KeyImage([2; 32])];
        state.add_unconfirmed(sent);
        let mut forgotten = Transaction::new(Hash([8; 32]));
        forgotten.add_transfer(primary, -50);
        forgotten.spent_key_images = vec![KeyImage([9; 32])];
        state.cancelled_transactions.push(forgotten);
        state.store_tx_private_key(Hash([4; 32]), SecretKey([5; 32]));
        state.sync.store_block_hash(Hash([6; 32]), 10);
        state.unknown.top.insert("hugin".into(), json!({"x": 1}));

        let json = state_to_json(&state);
        assert_eq!(json.sub_wallets.sub_wallet.len(), 2);
        let back = state_from_json(json.clone()).unwrap();
        assert_eq!(state_to_json(&back), json);
        assert_eq!(back.transactions, state.transactions);
        assert_eq!(back.locked_transactions, state.locked_transactions);
        assert_eq!(back.cancelled_transactions, state.cancelled_transactions);
        assert!(back.unknown.sub_wallets.is_empty());
        assert_eq!(back.subwallets, state.subwallets);
        assert_eq!(back.sync, state.sync);
        assert_eq!(back.key_image_owner(&KeyImage([1; 32])), Some(primary));
        // the extra fields are plain JSON that C++ ignores
        let text = String::from_utf8(xkr_wallet_files::backend::json::to_bytes(&json)).unwrap();
        assert!(text.contains("\"xkrExtra\":\"010203\""));
        assert!(text.contains("\"lockedTransactions\":[{"));
        assert!(text.contains("\"xkrCancelledTransactions\":[{"));
    }
}
