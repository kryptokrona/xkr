//! One address of a wallet and its outputs (`src/sub_wallets/sub_wallet.cpp` `SubWallet`).

use std::collections::HashSet;

use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};
use xkr_wallet_core::rules::is_unlocked;
use xkr_wallet_core::select::SpendableInput;
use xkr_wallet_files::backend::UnknownFields;

/// An output the wallet owns (`wallet_types::TransactionInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedInput {
    /// Key image (all zero in a view wallet, which cannot compute it).
    pub key_image: KeyImage,
    /// Amount.
    pub amount: u64,
    /// Index of the block holding the transaction.
    pub block_height: u64,
    /// Transaction public key from `extra`.
    pub transaction_public_key: PublicKey,
    /// Index of the output in its transaction.
    pub transaction_index: u64,
    /// Global index among outputs of this amount (fetched only for wallets that can spend).
    pub global_output_index: Option<u64>,
    /// One-time output key.
    pub key: PublicKey,
    /// Block index the output was spent at, 0 if unspent.
    pub spend_height: u64,
    /// Unlock time of the transaction.
    pub unlock_time: u64,
    /// Hash of the transaction holding the output.
    pub parent_transaction_hash: Hash,
    /// Fields of the wallet file not modelled here.
    pub other: UnknownFields,
}

impl SpendableInput for OwnedInput {
    fn amount(&self) -> u64 {
        self.amount
    }
    fn unlock_time(&self) -> u64 {
        self.unlock_time
    }
}

/// Change or a self-payment of a sent transaction that is not in a block yet
/// (`wallet_types::UnconfirmedInput`); it counts as locked balance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconfirmedInput {
    /// Amount.
    pub amount: u64,
    /// One-time output key.
    pub key: PublicKey,
    /// Hash of the sent transaction.
    pub parent_transaction_hash: Hash,
    /// Fields of the wallet file not modelled here.
    pub other: UnknownFields,
}

/// One address (`SubWallet`).
#[derive(Clone, PartialEq, Eq)]
pub struct SubWallet {
    /// Public spend key.
    pub public_spend_key: PublicKey,
    /// Private spend key; `None` in a view wallet.
    pub private_spend_key: Option<SecretKey>,
    /// The address (base58).
    pub address: String,
    /// Timestamp to scan from (0 when a height is used).
    pub sync_start_timestamp: u64,
    /// Height to scan from.
    pub sync_start_height: u64,
    /// Whether this is the primary address.
    pub is_primary: bool,
    /// Outputs that can be spent (once unlocked).
    pub unspent: Vec<OwnedInput>,
    /// Outputs used by a sent transaction that is not in a block yet.
    pub locked: Vec<OwnedInput>,
    /// Spent outputs.
    pub spent: Vec<OwnedInput>,
    /// Change of sent transactions not in a block yet.
    pub unconfirmed_incoming: Vec<UnconfirmedInput>,
    /// The `WalletGreen` creation timestamp of the address, for a walletd container.
    pub creation_timestamp: Option<u64>,
    /// Fields of the wallet file not modelled here.
    pub other: UnknownFields,
}

impl std::fmt::Debug for SubWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubWallet")
            .field("address", &self.address)
            .field("view_only", &self.private_spend_key.is_none())
            .field("is_primary", &self.is_primary)
            .field("unspent", &self.unspent.len())
            .field("locked", &self.locked.len())
            .field("spent", &self.spent.len())
            .finish_non_exhaustive()
    }
}

impl SubWallet {
    /// `SubWallet(publicSpendKey, privateSpendKey, address, scanHeight, scanTimestamp,
    /// isPrimaryAddress)`; `private_spend_key` `None` makes a view subwallet.
    #[must_use]
    pub fn new(
        public_spend_key: PublicKey,
        private_spend_key: Option<SecretKey>,
        address: String,
        scan_height: u64,
        scan_timestamp: u64,
        is_primary: bool,
    ) -> Self {
        Self {
            public_spend_key,
            private_spend_key,
            address,
            sync_start_timestamp: scan_timestamp,
            sync_start_height: scan_height,
            is_primary,
            unspent: Vec::new(),
            locked: Vec::new(),
            spent: Vec::new(),
            unconfirmed_incoming: Vec::new(),
            creation_timestamp: None,
            other: UnknownFields::new(),
        }
    }

    /// `SubWallet::getBalance(currentHeight)`: unspent outputs split into unlocked and locked
    /// (`utilities::isInputUnlocked`), plus unconfirmed change as locked. Outputs used by a
    /// pending sent transaction count in neither.
    #[must_use]
    pub fn balance(&self, height: u64, now: u64) -> (u64, u64) {
        let mut unlocked = 0u64;
        let mut locked = 0u64;
        for input in &self.unspent {
            if is_unlocked(input.unlock_time, height, now) {
                unlocked = unlocked.saturating_add(input.amount);
            } else {
                locked = locked.saturating_add(input.amount);
            }
        }
        for input in &self.unconfirmed_incoming {
            locked = locked.saturating_add(input.amount);
        }
        (unlocked, locked)
    }

    /// `SubWallet::storeTransactionInput`: a new output; unconfirmed change with the same key is
    /// now confirmed and dropped from the unconfirmed list.
    pub(crate) fn store_input(&mut self, input: OwnedInput, is_view_wallet: bool) {
        if !is_view_wallet {
            self.unconfirmed_incoming.retain(|u| u.key != input.key);
        }
        if self.unspent.iter().any(|i| i.key == input.key)
            || self.spent.iter().any(|i| i.key == input.key)
            || self.locked.iter().any(|i| i.key == input.key)
        {
            return;
        }
        self.unspent.push(input);
    }

    /// `SubWallet::markInputAsSpent`: moves the output with `key_image` from the unspent or
    /// locked outputs to the spent ones. False if it is in neither.
    pub(crate) fn mark_spent(&mut self, key_image: &KeyImage, spend_height: u64) -> bool {
        for list in [&mut self.unspent, &mut self.locked] {
            if let Some(pos) = list.iter().position(|i| i.key_image == *key_image) {
                let mut input = list.remove(pos);
                input.spend_height = spend_height;
                self.spent.push(input);
                return true;
            }
        }
        false
    }

    /// `SubWallet::markInputAsLocked`: moves an unspent output to the locked ones.
    pub(crate) fn mark_locked(&mut self, key_image: &KeyImage) -> bool {
        match self.unspent.iter().position(|i| i.key_image == *key_image) {
            Some(pos) => {
                let input = self.unspent.remove(pos);
                self.locked.push(input);
                true
            }
            None => false,
        }
    }

    /// `SubWallet::removeForkedInputs`: forgets outputs received at or above `fork_height`, and
    /// makes outputs spent at or above it unspent again. Outputs locked by a pending sent
    /// transaction stay locked unless they were received above the fork; the pending
    /// transaction is confirmed or cancelled later.
    pub(crate) fn remove_forked(&mut self, fork_height: u64) {
        self.unspent.retain(|i| i.block_height < fork_height);
        self.locked.retain(|i| i.block_height < fork_height);
        let mut kept = Vec::with_capacity(self.spent.len());
        for mut input in std::mem::take(&mut self.spent) {
            if input.block_height >= fork_height {
                continue;
            }
            if input.spend_height >= fork_height {
                input.spend_height = 0;
                self.unspent.push(input);
            } else {
                kept.push(input);
            }
        }
        self.spent = kept;
    }

    /// `SubWallet::removeCancelledTransactions`: outputs locked by a cancelled transaction are
    /// unspent again; its unconfirmed change is dropped.
    pub(crate) fn remove_cancelled(&mut self, cancelled: &HashSet<Hash>) {
        let mut still_locked = Vec::with_capacity(self.locked.len());
        for mut input in std::mem::take(&mut self.locked) {
            if cancelled.contains(&input.parent_transaction_hash) {
                input.spend_height = 0;
                self.unspent.push(input);
            } else {
                still_locked.push(input);
            }
        }
        self.locked = still_locked;
        self.unconfirmed_incoming
            .retain(|u| !cancelled.contains(&u.parent_transaction_hash));
    }

    /// Unlocks the outputs a cancelled transaction used, by key image (the locked outputs
    /// carry the hash of the transaction that created them, not of the one spending them).
    pub(crate) fn unlock_key_images(&mut self, key_images: &HashSet<KeyImage>) {
        let mut still_locked = Vec::with_capacity(self.locked.len());
        for mut input in std::mem::take(&mut self.locked) {
            if key_images.contains(&input.key_image) {
                input.spend_height = 0;
                self.unspent.push(input);
            } else {
                still_locked.push(input);
            }
        }
        self.locked = still_locked;
    }

    /// wallet-backend-js `SubWallet.pruneSpentInputs`: forgets the spent outputs spent below
    /// `prune_height`.
    pub(crate) fn prune_spent_inputs(&mut self, prune_height: u64) {
        let before = self.spent.len();
        self.spent.retain(|i| i.spend_height >= prune_height);
        let pruned = before - self.spent.len();
        if pruned != 0 {
            tracing::debug!("Pruned {pruned} spent inputs");
        }
    }

    /// `SubWallet::reset(scanHeight)`.
    pub(crate) fn reset(&mut self, scan_height: u64) {
        self.sync_start_timestamp = 0;
        self.sync_start_height = scan_height;
        self.locked.clear();
        self.unconfirmed_incoming.clear();
        self.unspent.clear();
        self.spent.clear();
    }

    /// `SubWallet::convertSyncTimestampToHeight`: a subwallet that scans from a timestamp scans
    /// from `height` from now on.
    pub(crate) fn convert_sync_timestamp(&mut self, timestamp: u64, height: u64) {
        if self.sync_start_timestamp != 0 {
            self.sync_start_timestamp = timestamp;
            self.sync_start_height = height;
        }
    }

    /// Whether `key_image` is one of the unspent or locked outputs (`SubWallet::hasKeyImage`).
    #[must_use]
    pub fn has_key_image(&self, key_image: &KeyImage) -> bool {
        self.unspent.iter().any(|i| i.key_image == *key_image)
            || self.locked.iter().any(|i| i.key_image == *key_image)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn input(n: u8, amount: u64, height: u64) -> OwnedInput {
        OwnedInput {
            key_image: KeyImage([n; 32]),
            amount,
            block_height: height,
            transaction_public_key: PublicKey([n; 32]),
            transaction_index: 0,
            global_output_index: Some(u64::from(n)),
            key: PublicKey([n.wrapping_add(100); 32]),
            spend_height: 0,
            unlock_time: 0,
            parent_transaction_hash: Hash([n; 32]),
            other: UnknownFields::new(),
        }
    }

    fn sub() -> SubWallet {
        SubWallet::new(PublicKey([7; 32]), None, "SEKR".into(), 0, 0, true)
    }

    #[test]
    fn balance_spend_lock_and_fork() {
        let mut s = sub();
        s.store_input(input(1, 100, 10), false);
        let mut locked = input(2, 50, 11);
        locked.unlock_time = 1_000;
        s.store_input(locked, false);
        s.store_input(input(1, 100, 10), false); // same output twice
        assert_eq!(s.balance(20, 0), (100, 50));
        assert_eq!(s.balance(999, 0), (150, 0));
        assert!(s.mark_locked(&KeyImage([1; 32])));
        assert_eq!(s.balance(999, 0), (50, 0));
        assert!(s.has_key_image(&KeyImage([1; 32])));
        assert!(s.mark_spent(&KeyImage([1; 32]), 12));
        assert!(!s.has_key_image(&KeyImage([1; 32])));
        assert!(s.mark_spent(&KeyImage([2; 32]), 30));
        assert_eq!(s.balance(999, 0), (0, 0));
        // a fork at 12: the spend at 12 and the spend at 30 are undone
        s.remove_forked(12);
        assert_eq!(s.balance(999, 0), (150, 0));
        assert_eq!(s.spent.len(), 0);
        // a fork at 11 forgets the output received at 11
        s.remove_forked(11);
        assert_eq!(s.balance(999, 0), (100, 0));
    }

    #[test]
    fn spent_inputs_are_pruned_below_the_height() {
        let mut s = sub();
        for n in 1..=3 {
            s.store_input(input(n, 10, 1), false);
        }
        assert!(s.mark_spent(&KeyImage([1; 32]), 100));
        assert!(s.mark_spent(&KeyImage([2; 32]), 5_000));
        s.prune_spent_inputs(5_000);
        assert_eq!(s.spent.len(), 1);
        assert_eq!(s.spent[0].key_image, KeyImage([2; 32]));
        assert_eq!(s.unspent.len(), 1);
        assert_eq!(s.balance(10, 0), (10, 0));
    }

    #[test]
    fn cancelled_transactions_unlock() {
        let mut s = sub();
        s.store_input(input(1, 100, 10), false);
        assert!(s.mark_locked(&KeyImage([1; 32])));
        s.unconfirmed_incoming.push(UnconfirmedInput {
            amount: 40,
            key: PublicKey([55; 32]),
            parent_transaction_hash: Hash([0xaa; 32]),
            other: UnknownFields::new(),
        });
        assert_eq!(s.balance(100, 0), (0, 40));
        s.remove_cancelled(&HashSet::from([Hash([0xaa; 32])]));
        s.unlock_key_images(&HashSet::from([KeyImage([1; 32])]));
        assert_eq!(s.balance(100, 0), (100, 0));
        s.reset(5);
        assert_eq!((s.sync_start_height, s.unspent.len()), (5, 0));
    }
}
