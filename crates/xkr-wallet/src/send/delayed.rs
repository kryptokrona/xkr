//! Prepared ("delayed") transactions of walletd (`src/wallet/wallet_green.cpp`
//! `WalletGreen::makeTransaction`, `commitTransaction`, `rollbackUncommitedTransaction`;
//! `src/wallet_service/wallet_service.cpp` `createDelayedTransaction`,
//! `getDelayedTransactionHashes`, `sendDelayedTransaction`, `deleteDelayedTransaction`).
//!
//! A delayed transaction is built and signed but not relayed. Its inputs are set aside so no
//! other transaction uses them; they are not locked in the wallet file. The list is part of
//! the wallet state and saved with it (walletd keeps it in its cache, `m_uncommitedTransactions`
//! of `WalletSerializerV2`).

use std::collections::HashSet;

use xkr_types::{Hash, KeyImage};

use super::Prepared;
use crate::state::{Transaction, WalletState};

/// The delayed transactions of a wallet, in creation order.
#[derive(Debug, Clone, Default)]
pub(crate) struct DelayedStore {
    items: Vec<Prepared>,
}

impl DelayedStore {
    /// Keeps `prepared` and sets its inputs aside.
    pub(crate) fn insert(&mut self, reserved: &mut HashSet<KeyImage>, prepared: Prepared) {
        for (_, input) in &prepared.inputs {
            reserved.insert(input.key_image);
        }
        self.items.push(prepared);
    }

    /// The delayed transactions, in creation order.
    pub(crate) fn items(&self) -> &[Prepared] {
        &self.items
    }

    /// The hashes, in creation order.
    pub(crate) fn hashes(&self) -> Vec<Hash> {
        self.items.iter().map(|p| p.built.hash).collect()
    }

    /// The wallet record of a delayed transaction.
    pub(crate) fn record(&self, hash: &Hash) -> Option<Transaction> {
        self.get(hash).map(Prepared::record)
    }

    /// The prepared transaction with `hash`.
    pub(crate) fn get(&self, hash: &Hash) -> Option<&Prepared> {
        self.items.iter().find(|p| p.built.hash == *hash)
    }

    /// Takes the prepared transaction with `hash` out of the list; its inputs stay set aside.
    pub(crate) fn take(&mut self, hash: &Hash) -> Option<Prepared> {
        let pos = self.items.iter().position(|p| p.built.hash == *hash)?;
        Some(self.items.remove(pos))
    }

    /// Puts back a transaction that could not be sent.
    pub(crate) fn restore(&mut self, prepared: Prepared) {
        self.items.push(prepared);
    }

    /// Deletes the transaction with `hash` and frees its inputs.
    pub(crate) fn delete(&mut self, reserved: &mut HashSet<KeyImage>, hash: &Hash) -> bool {
        match self.take(hash) {
            Some(prepared) => {
                for (_, input) in &prepared.inputs {
                    reserved.remove(&input.key_image);
                }
                true
            }
            None => false,
        }
    }

    /// Deletes the transactions `remove` picks and frees their inputs.
    pub(crate) fn remove_where(
        &mut self,
        reserved: &mut HashSet<KeyImage>,
        remove: impl Fn(&Prepared) -> bool,
    ) {
        self.items.retain(|p| {
            if !remove(p) {
                return true;
            }
            for (_, input) in &p.inputs {
                reserved.remove(&input.key_image);
            }
            false
        });
    }

    /// Forgets everything (after a reset, the inputs no longer exist).
    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    /// Whether every input of `prepared` is still an unspent output of the wallet.
    pub(crate) fn inputs_unspent(state: &WalletState, prepared: &Prepared) -> bool {
        prepared.inputs.iter().all(|(owner, input)| {
            state
                .subwallet(owner)
                .is_some_and(|s| s.unspent.iter().any(|u| u.key_image == input.key_image))
        })
    }
}
