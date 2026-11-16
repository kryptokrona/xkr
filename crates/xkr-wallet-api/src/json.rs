//! JSON of wallet transactions (`src/wallet_backend/json_serialization.cpp`
//! `to_json(json &, const Transaction &)`) and `ApiDispatcher::publicKeysToAddresses`.
//!
//! A transaction is `{transfers, hash, fee, blockHeight, timestamp, paymentID, unlockTime,
//! isCoinbaseTransaction}`. Its `transfers` carry `{address, amount}` on every route except
//! `/transactions/hash/{hash}`, which keeps `{publicKey, amount}` as C++ does. A transfer of a
//! deleted subwallet has the address `""` (`SubWallets::getAddress` fails).

use std::collections::HashMap;

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_types::PublicKey;
use xkr_wallet::validate::spend_key_of;
use xkr_wallet::{Transaction, Wallet};

/// The addresses of the wallet's subwallets by public spend key (`SubWallets::getAddress`).
#[derive(Debug, Clone, Default)]
pub struct AddressNames(HashMap<PublicKey, String>);

impl AddressNames {
    /// The subwallets of `wallet`.
    #[must_use]
    pub fn of<T: Transport + 'static>(wallet: &Wallet<T>) -> Self {
        Self::from_addresses(wallet.addresses())
    }

    /// From a list of the wallet's addresses.
    #[must_use]
    pub fn from_addresses(addresses: Vec<String>) -> Self {
        Self(
            addresses
                .into_iter()
                .filter_map(|a| spend_key_of(&a).map(|k| (k, a)))
                .collect(),
        )
    }

    /// The address of subwallet `key`, `""` if there is none.
    #[must_use]
    pub fn name(&self, key: &PublicKey) -> &str {
        self.0.get(key).map_or("", String::as_str)
    }

    /// Whether a transfer of `tx` belongs to the subwallet whose address is exactly `address`
    /// (the C++ filters compare the address strings).
    #[must_use]
    pub fn touches(&self, tx: &Transaction, address: &str) -> bool {
        tx.transfers.iter().any(|(k, _)| self.name(k) == address)
    }
}

/// How a transaction's transfers name their subwallet.
#[derive(Debug, Clone, Copy)]
pub enum Transfers<'a> {
    /// `{address, amount}` (`publicKeysToAddresses`).
    Addresses(&'a AddressNames),
    /// `{publicKey, amount}` (`/transactions/hash/{hash}`).
    PublicKeys,
}

/// One transaction.
#[must_use]
pub fn transaction(tx: &Transaction, transfers: Transfers<'_>) -> Value {
    let transfers: Vec<Value> = tx
        .transfers
        .iter()
        .map(|(key, amount)| match transfers {
            Transfers::Addresses(names) => json!({ "address": names.name(key), "amount": amount }),
            Transfers::PublicKeys => json!({ "publicKey": key.to_hex(), "amount": amount }),
        })
        .collect();
    json!({
        "transfers": transfers,
        "hash": tx.hash.to_hex(),
        "fee": tx.fee,
        "blockHeight": tx.block_height,
        "timestamp": tx.timestamp,
        "paymentID": tx.payment_id,
        "unlockTime": tx.unlock_time,
        "isCoinbaseTransaction": tx.is_coinbase,
    })
}

/// `{"transactions": [...]}` with addresses.
#[must_use]
pub fn transactions<'a>(
    txs: impl IntoIterator<Item = &'a Transaction>,
    names: &AddressNames,
) -> Value {
    let list: Vec<Value> = txs
        .into_iter()
        .map(|tx| transaction(tx, Transfers::Addresses(names)))
        .collect();
    json!({ "transactions": list })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_types::Hash;

    #[test]
    fn transfers_with_addresses_or_keys() {
        let key = PublicKey([7; 32]);
        let mut names = AddressNames::default();
        names.0.insert(key, "SEKRaddress".to_owned());
        let mut tx = Transaction::new(Hash([1; 32]));
        tx.transfers = vec![(key, -110), (PublicKey([8; 32]), 5)];
        tx.fee = 10;
        tx.block_height = 7;
        tx.payment_id = "ab".repeat(32);
        let v = transaction(&tx, Transfers::Addresses(&names));
        assert_eq!(
            v["transfers"],
            json!([{"address": "SEKRaddress", "amount": -110}, {"address": "", "amount": 5}])
        );
        assert_eq!(v["hash"], json!("01".repeat(32)));
        assert_eq!(v["blockHeight"], json!(7));
        assert_eq!(v["isCoinbaseTransaction"], json!(false));
        let v = transaction(&tx, Transfers::PublicKeys);
        assert_eq!(
            v["transfers"][0],
            json!({"publicKey": "07".repeat(32), "amount": -110})
        );
        assert!(names.touches(&tx, "SEKRaddress"));
        assert!(!names.touches(&tx, "SEKRother"));
        assert_eq!(
            crate::respond::dump(&transactions(std::iter::empty(), &names)),
            "{\n    \"transactions\": []\n}\n"
        );
    }
}
