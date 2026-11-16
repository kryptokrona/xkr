//! Wallet transactions (`include/wallet_types.h` `wallet_types::Transaction`) and what walletd
//! adds to them (`include/iwallet.h` `WalletTransactionState`, `WalletTransferType`).

use xkr_types::{Hash, KeyImage, PublicKey};
use xkr_wallet_files::backend::UnknownFields;

/// `iwallet.h` `WalletTransactionState`, as walletd reports it in `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TxState {
    /// `SUCCEEDED` (0): in a block, or sent and waiting in the pool.
    Succeeded = 0,
    /// `FAILED` (1).
    Failed = 1,
    /// `CANCELLED` (2): the node forgot it; its inputs are spendable again.
    Cancelled = 2,
    /// `CREATED` (3): built but not sent (a delayed transaction).
    Created = 3,
    /// `DELETED` (4).
    Deleted = 4,
}

impl TxState {
    /// The walletd number.
    #[must_use]
    pub const fn number(self) -> u8 {
        self as u8
    }
}

/// `iwallet.h` `WalletTransferType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TransferKind {
    /// `USUAL` (0).
    Usual = 0,
    /// `DONATION` (1).
    Donation = 1,
    /// `CHANGE` (2).
    Change = 2,
}

impl TransferKind {
    /// The walletd number.
    #[must_use]
    pub const fn number(self) -> u8 {
        self as u8
    }

    /// The kind with walletd number `n` (unknown numbers are [`TransferKind::Usual`]).
    #[must_use]
    pub const fn from_number(n: u8) -> Self {
        match n {
            1 => Self::Donation,
            2 => Self::Change,
            _ => Self::Usual,
        }
    }
}

/// A destination of a transaction this wallet sent (or one imported from a walletd cache):
/// walletd lists them in `transfers`; the C++ `WalletBackend` file has no place for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferDestination {
    /// Destination address (base58).
    pub address: String,
    /// Amount sent to it (negative amounts come from walletd caches).
    pub amount: i64,
    /// Usual, donation or change.
    pub kind: TransferKind,
}

/// A transaction that touches the wallet (`wallet_types::Transaction`).
///
/// `transfers` holds the net amount per own subwallet (public spend key), negative for money
/// that left it. Unconfirmed sent transactions have `block_height` 0 and `timestamp` 0, as in
/// C++ (`storeSentTransaction`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Net amount per own subwallet, in the order first seen.
    pub transfers: Vec<(PublicKey, i64)>,
    /// Transaction hash.
    pub hash: Hash,
    /// Fee.
    pub fee: u64,
    /// Block index (0 while unconfirmed).
    pub block_height: u64,
    /// Block timestamp (0 while unconfirmed).
    pub timestamp: u64,
    /// Payment id as 64 hex characters, empty if none.
    pub payment_id: String,
    /// Unlock time.
    pub unlock_time: u64,
    /// Whether this is a coinbase transaction.
    pub is_coinbase: bool,
    /// The full `extra` field, once known (sent by this wallet, imported, or fetched for
    /// walletd).
    pub extra: Option<Vec<u8>>,
    /// Destinations of a transaction this wallet sent.
    pub destinations: Vec<TransferDestination>,
    /// Key images of the own outputs a transaction this wallet sent spends; they are unlocked
    /// again if the node forgets the transaction.
    pub spent_key_images: Vec<KeyImage>,
    /// Fields of the wallet file that are not modelled here (kept on save).
    pub other: UnknownFields,
}

impl Transaction {
    /// An empty record for `hash`.
    #[must_use]
    pub fn new(hash: Hash) -> Self {
        Self {
            transfers: Vec::new(),
            hash,
            fee: 0,
            block_height: 0,
            timestamp: 0,
            payment_id: String::new(),
            unlock_time: 0,
            is_coinbase: false,
            extra: None,
            destinations: Vec::new(),
            spent_key_images: Vec::new(),
            other: UnknownFields::new(),
        }
    }

    /// Sum of the transfers (`wallet_types::Transaction::totalAmount`).
    #[must_use]
    pub fn total_amount(&self) -> i64 {
        self.transfers.iter().map(|(_, a)| *a).sum()
    }

    /// The net amount for subwallet `key` (0 if it is not involved).
    #[must_use]
    pub fn amount_for(&self, key: &PublicKey) -> i64 {
        self.transfers
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, a)| *a)
            .sum()
    }

    /// Whether subwallet `key` takes part.
    #[must_use]
    pub fn involves(&self, key: &PublicKey) -> bool {
        self.transfers.iter().any(|(k, _)| k == key)
    }

    /// `wallet_types::Transaction::isFusionTransaction`: no fee and not a coinbase.
    #[must_use]
    pub fn is_fusion(&self) -> bool {
        self.fee == 0 && !self.is_coinbase
    }

    /// Whether it is waiting for a block (sent by this wallet, `blockHeight` 0).
    #[must_use]
    pub fn is_unconfirmed(&self) -> bool {
        self.block_height == 0 && self.timestamp == 0 && !self.is_coinbase
    }

    /// Adds `amount` to the transfer of `key` (`transfers[key] += amount`).
    pub(crate) fn add_transfer(&mut self, key: PublicKey, amount: i64) {
        if let Some(entry) = self.transfers.iter_mut().find(|(k, _)| *k == key) {
            entry.1 += amount;
        } else {
            self.transfers.push((key, amount));
        }
    }

    /// Takes over what the wallet knew about the same transaction before it was confirmed or
    /// rescanned: `extra`, destinations and unmodelled fields.
    pub(crate) fn inherit(&mut self, old: &Self) {
        if self.extra.is_none() {
            self.extra.clone_from(&old.extra);
        }
        if self.destinations.is_empty() {
            self.destinations.clone_from(&old.destinations);
        }
        if self.spent_key_images.is_empty() {
            self.spent_key_images.clone_from(&old.spent_key_images);
        }
        for (k, v) in &old.other {
            self.other.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

/// A transaction with its walletd state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionInfo {
    /// The transaction.
    pub transaction: Transaction,
    /// Its state.
    pub state: TxState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_accumulate_per_key() {
        let a = PublicKey([1; 32]);
        let b = PublicKey([2; 32]);
        let mut tx = Transaction::new(Hash([9; 32]));
        tx.add_transfer(a, 100);
        tx.add_transfer(b, 5);
        tx.add_transfer(a, -30);
        assert_eq!(tx.transfers, vec![(a, 70), (b, 5)]);
        assert_eq!(tx.total_amount(), 75);
        assert_eq!(tx.amount_for(&a), 70);
        assert!(tx.involves(&b));
        assert!(tx.is_fusion());
        assert!(tx.is_unconfirmed());
        let mut old = Transaction::new(tx.hash);
        old.extra = Some(vec![1, 2]);
        tx.inherit(&old);
        assert_eq!(tx.extra, Some(vec![1, 2]));
    }
}
