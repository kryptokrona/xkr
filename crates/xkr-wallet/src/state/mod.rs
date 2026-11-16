//! What a wallet knows: keys, subwallets with their outputs, transactions and sync progress
//! (`src/sub_wallets/sub_wallets.cpp` `SubWallets` together with the synchronizer state of
//! `src/wallet_backend/wallet_synchronizer.cpp`).
//!
//! Everything here is plain data with synchronous methods; [`crate::Wallet`] keeps it behind
//! one lock that the sync task and the API calls share, as C++ guards `SubWallets` with
//! `m_mutex`.

pub mod subwallet;
pub mod transaction;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use xkr_consensus::params::{
    CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT, CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3,
    CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4, DIFFICULTY_TARGET, GENESIS_BLOCK_TIMESTAMP,
};
use xkr_crypto::secret_key_to_public_key;
use xkr_types::{Hash, KeyImage, PublicKey, SecretKey};
use xkr_wallet_core::{Address, SpendKeys};
use xkr_wallet_files::backend::UnknownFields;

pub use subwallet::{OwnedInput, SubWallet, UnconfirmedInput};
pub use transaction::{Transaction, TransactionInfo, TransferDestination, TransferKind, TxState};

use crate::error::{ErrorCode, WalletError};
use crate::send::Prepared;
use crate::send::delayed::DelayedStore;
use crate::sync::status::SyncStatus;

/// wallet-backend-js `WalletBackend.processBlocks`: every this many blocks the spent outputs
/// spent more than this many blocks ago are dropped (`SubWallets.pruneSpentInputs`).
pub const PRUNE_SPENT_INPUTS_INTERVAL: u64 = 5000;

/// `utilities::getCurrentTimestampAdjusted`: now minus the largest future-time limit, the
/// earliest timestamp that still covers every block that could be mined from now on.
#[must_use]
pub fn current_timestamp_adjusted(now: u64) -> u64 {
    let adjust = CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT
        .max(CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3)
        .max(CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4);
    now.saturating_sub(adjust)
}

/// `utilities::scanHeightToTimestamp`: an estimate of the timestamp of block `height`, at most
/// [`current_timestamp_adjusted`].
#[must_use]
pub fn scan_height_to_timestamp(height: u64, now: u64) -> u64 {
    if height == 0 {
        return 0;
    }
    let timestamp =
        GENESIS_BLOCK_TIMESTAMP.saturating_add(height.saturating_mul(DIFFICULTY_TARGET));
    timestamp.min(current_timestamp_adjusted(now))
}

/// `utilities::timestampToScanHeight`: an estimate of the height at `timestamp`.
#[must_use]
pub fn timestamp_to_scan_height(timestamp: u64) -> u64 {
    if timestamp <= GENESIS_BLOCK_TIMESTAMP {
        return 0;
    }
    (timestamp - GENESIS_BLOCK_TIMESTAMP) / DIFFICULTY_TARGET
}

/// The current unix time in seconds.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Unlocked and locked balance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Balance {
    /// Spendable now.
    pub unlocked: u64,
    /// Not spendable yet (locked outputs and unconfirmed change).
    pub locked: u64,
}

/// The balance of one address (wallet-api `/balances`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressBalance {
    /// The address.
    pub address: String,
    /// Spendable now.
    pub unlocked: u64,
    /// Not spendable yet.
    pub locked: u64,
}

/// An unspent output offered to input selection, with its owner.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub owner: PublicKey,
    pub spend_secret: SecretKey,
    pub input: OwnedInput,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("owner", &self.owner)
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

impl xkr_wallet_core::select::SpendableInput for Candidate {
    fn amount(&self) -> u64 {
        self.input.amount
    }
    fn unlock_time(&self) -> u64 {
        self.input.unlock_time
    }
}

/// Fields of the wallet file outside the modelled objects (kept on save).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnknownParts {
    /// Top level.
    pub top: UnknownFields,
    /// `subWallets`.
    pub sub_wallets: UnknownFields,
    /// `walletSynchronizer`.
    pub synchronizer: UnknownFields,
}

/// The whole wallet state (`SubWallets` + `WalletSynchronizer` data).
#[derive(Clone)]
pub struct WalletState {
    /// Subwallets in creation order (`m_publicSpendKeys`).
    pub(crate) subwallets: Vec<SubWallet>,
    /// Public spend key to index in `subwallets`.
    index: HashMap<PublicKey, usize>,
    /// Key image to owner, for the unspent and locked outputs (wallets that can spend).
    key_images: HashMap<KeyImage, PublicKey>,
    /// Confirmed transactions.
    pub(crate) transactions: Vec<Transaction>,
    /// Sent transactions not in a block yet (`m_lockedTransactions`).
    pub(crate) locked_transactions: Vec<Transaction>,
    /// Transactions the node forgot (walletd state `CANCELLED`).
    pub(crate) cancelled_transactions: Vec<Transaction>,
    /// The shared private view key.
    pub(crate) view_secret: SecretKey,
    /// The shared public view key.
    pub(crate) view_public: PublicKey,
    /// Whether the wallet has no private spend keys.
    pub(crate) is_view_wallet: bool,
    /// Private keys of sent transactions, in insertion order.
    pub(crate) tx_private_keys: Vec<(Hash, SecretKey)>,
    /// Sync progress.
    pub(crate) sync: SyncStatus,
    /// `WalletSynchronizer::m_startHeight`.
    pub(crate) start_height: u64,
    /// `WalletSynchronizer::m_startTimestamp`.
    pub(crate) start_timestamp: u64,
    /// The spend keys for scanning, shared with the scan task.
    pub(crate) spend_keys: Arc<SpendKeys>,
    /// Bumped whenever the set of spend keys changes.
    pub(crate) keys_version: u64,
    /// Bumped whenever sync must restart from the stored progress (reset, failed batch, node
    /// change); batches fetched for an older generation are dropped.
    pub(crate) generation: u64,
    /// Outputs set aside by delayed transactions (rebuilt from [`Self::delayed`] on load).
    pub(crate) reserved: HashSet<KeyImage>,
    /// walletd's delayed transactions (built and signed, not relayed), in creation order.
    pub(crate) delayed: DelayedStore,
    /// Blocks an output waits after the block holding it before it is spent or counted as
    /// unlocked (walletd `WalletGreen` `transactionSoftLockTime`, 1); 0 for `WalletBackend`.
    pub(crate) spendable_age: u64,
    /// Unmodelled file fields.
    pub(crate) unknown: UnknownParts,
}

impl std::fmt::Debug for WalletState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletState")
            .field("subwallets", &self.subwallets)
            .field("transactions", &self.transactions.len())
            .field("locked_transactions", &self.locked_transactions.len())
            .field("is_view_wallet", &self.is_view_wallet)
            .field("sync_height", &self.sync.height())
            .finish_non_exhaustive()
    }
}

impl WalletState {
    /// An empty state with the given keys; [`Self::push_subwallet`] adds the subwallets.
    pub(crate) fn empty(view_secret: SecretKey, is_view_wallet: bool) -> Result<Self, WalletError> {
        let view_public = secret_key_to_public_key(&view_secret)
            .ok_or_else(|| WalletError::new(ErrorCode::InvalidKeyFormat))?;
        Ok(Self {
            subwallets: Vec::new(),
            index: HashMap::new(),
            key_images: HashMap::new(),
            transactions: Vec::new(),
            locked_transactions: Vec::new(),
            cancelled_transactions: Vec::new(),
            view_secret,
            view_public,
            is_view_wallet,
            tx_private_keys: Vec::new(),
            sync: SyncStatus::default(),
            start_height: 0,
            start_timestamp: 0,
            spend_keys: Arc::new(SpendKeys::new()),
            keys_version: 0,
            generation: 0,
            reserved: HashSet::new(),
            delayed: DelayedStore::default(),
            spendable_age: 0,
            unknown: UnknownParts::default(),
        })
    }

    /// `SubWallets(privateSpendKey, privateViewKey, address, scanHeight, newWallet)`: a wallet
    /// with one spend subwallet; a new wallet scans from now (adjusted), an imported one from
    /// `scan_height`.
    pub(crate) fn new_spend(
        spend_secret: SecretKey,
        view_secret: SecretKey,
        scan_height: u64,
        new_wallet: bool,
        now: u64,
    ) -> Result<Self, WalletError> {
        let spend_public = secret_key_to_public_key(&spend_secret)
            .ok_or_else(|| WalletError::new(ErrorCode::InvalidKeyFormat))?;
        let mut state = Self::empty(view_secret, false)?;
        let address = Address::new(spend_public, state.view_public).to_string();
        let timestamp = if new_wallet {
            current_timestamp_adjusted(now)
        } else {
            0
        };
        state.push_subwallet(SubWallet::new(
            spend_public,
            Some(spend_secret),
            address,
            scan_height,
            timestamp,
            true,
        ));
        state.init_sync_start(now);
        Ok(state)
    }

    /// `SubWallets(privateViewKey, address, scanHeight, newWallet = false)`: a view wallet.
    pub(crate) fn new_view(
        view_secret: SecretKey,
        address: &Address,
        scan_height: u64,
        now: u64,
    ) -> Result<Self, WalletError> {
        let mut state = Self::empty(view_secret, true)?;
        if address.view != state.view_public {
            return Err(WalletError::new(ErrorCode::AddressNotValid)
                .with_detail("the view key does not belong to the address"));
        }
        let text = Address::new(address.spend, address.view).to_string();
        state.push_subwallet(SubWallet::new(
            address.spend,
            None,
            text,
            scan_height,
            0,
            true,
        ));
        state.init_sync_start(now);
        Ok(state)
    }

    /// Sets the synchronizer start from the subwallets (`WalletBackend::init` with
    /// `getMinInitialSyncStart`).
    pub(crate) fn init_sync_start(&mut self, now: u64) {
        let (height, timestamp) = self.min_initial_sync_start(now);
        self.start_height = height;
        self.start_timestamp = timestamp;
    }

    /// Appends a subwallet and updates the indexes.
    pub(crate) fn push_subwallet(&mut self, sub: SubWallet) {
        self.index
            .insert(sub.public_spend_key, self.subwallets.len());
        self.subwallets.push(sub);
        self.keys_changed();
    }

    /// Rebuilds the scan keys and the indexes after the subwallets changed.
    pub(crate) fn keys_changed(&mut self) {
        self.index = self
            .subwallets
            .iter()
            .enumerate()
            .map(|(i, s)| (s.public_spend_key, i))
            .collect();
        self.spend_keys = Arc::new(
            self.subwallets
                .iter()
                .map(|s| (s.public_spend_key, s.private_spend_key))
                .collect(),
        );
        self.keys_version += 1;
        self.rebuild_key_images();
    }

    /// Rebuilds the key image index from the unspent and locked outputs.
    pub(crate) fn rebuild_key_images(&mut self) {
        self.key_images.clear();
        if self.is_view_wallet {
            return;
        }
        for sub in &self.subwallets {
            for input in sub.unspent.iter().chain(&sub.locked) {
                self.key_images
                    .insert(input.key_image, sub.public_spend_key);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // reading

    /// The subwallets in creation order.
    #[must_use]
    pub fn subwallets(&self) -> &[SubWallet] {
        &self.subwallets
    }

    /// The subwallet with public spend key `key`.
    #[must_use]
    pub fn subwallet(&self, key: &PublicKey) -> Option<&SubWallet> {
        self.index.get(key).map(|&i| &self.subwallets[i])
    }

    pub(crate) fn subwallet_mut(&mut self, key: &PublicKey) -> Option<&mut SubWallet> {
        let i = *self.index.get(key)?;
        self.subwallets.get_mut(i)
    }

    /// `SubWallets::getPrimaryAddress`: the primary subwallet (the first one if none is marked).
    ///
    /// # Panics
    ///
    /// If the wallet has no address, which only a walletd container can reach
    /// ([`Self::try_primary`] does not panic).
    #[must_use]
    pub fn primary(&self) -> &SubWallet {
        self.try_primary().expect("the wallet has an address")
    }

    /// The primary subwallet (the first one if none is marked), `None` for a walletd container
    /// whose addresses were all deleted.
    #[must_use]
    pub fn try_primary(&self) -> Option<&SubWallet> {
        self.subwallets
            .iter()
            .find(|s| s.is_primary)
            .or_else(|| self.subwallets.first())
    }

    /// The shared private view key.
    #[must_use]
    pub fn view_secret(&self) -> SecretKey {
        self.view_secret
    }

    /// The shared public view key.
    #[must_use]
    pub fn view_public(&self) -> PublicKey {
        self.view_public
    }

    /// Whether this is a view wallet.
    #[must_use]
    pub fn is_view_wallet(&self) -> bool {
        self.is_view_wallet
    }

    /// Sync progress.
    #[must_use]
    pub fn sync_status(&self) -> &SyncStatus {
        &self.sync
    }

    /// Confirmed transactions.
    #[must_use]
    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Sent transactions not in a block yet.
    #[must_use]
    pub fn locked_transactions(&self) -> &[Transaction] {
        &self.locked_transactions
    }

    /// Sent transactions the node forgot (walletd state `CANCELLED`).
    #[must_use]
    pub fn cancelled_transactions(&self) -> &[Transaction] {
        &self.cancelled_transactions
    }

    /// `SubWallets::getKeyImageOwner`: the subwallet an unspent or locked output with
    /// `key_image` belongs to. Always `None` in a view wallet.
    #[must_use]
    pub fn key_image_owner(&self, key_image: &KeyImage) -> Option<PublicKey> {
        if self.is_view_wallet {
            return None;
        }
        self.key_images.get(key_image).copied()
    }

    /// `SubWallets::getMinInitialSyncStart`: the smallest start height and start timestamp of
    /// the subwallets; only one of them is non-zero (the earlier one) unless one is already 0.
    #[must_use]
    pub fn min_initial_sync_start(&self, now: u64) -> (u64, u64) {
        let min_timestamp = self
            .subwallets
            .iter()
            .map(|s| s.sync_start_timestamp)
            .min()
            .unwrap_or(0);
        let min_height = self
            .subwallets
            .iter()
            .map(|s| s.sync_start_height)
            .min()
            .unwrap_or(0);
        if min_height == 0 || min_timestamp == 0 {
            return (min_height, min_timestamp);
        }
        if scan_height_to_timestamp(min_height, now) < min_timestamp {
            (min_height, 0)
        } else {
            (0, min_timestamp)
        }
    }

    /// Whether an unspent output is younger than the spendable age (walletd
    /// `TransfersContainer::isIncluded`: soft locked while the last processed block index is
    /// below `blockHeight + transactionSpendableAge`).
    fn soft_locked(&self, input: &OwnedInput) -> bool {
        self.spendable_age != 0
            && self.sync.height() < input.block_height.saturating_add(self.spendable_age)
    }

    /// The balance of one subwallet with soft locked outputs counted as locked.
    fn sub_balance(&self, sub: &SubWallet, height: u64, now: u64) -> (u64, u64) {
        let (mut unlocked, mut locked) = sub.balance(height, now);
        if self.spendable_age != 0 {
            let soft: u64 = sub
                .unspent
                .iter()
                .filter(|i| {
                    xkr_wallet_core::rules::is_unlocked(i.unlock_time, height, now)
                        && self.soft_locked(i)
                })
                .map(|i| i.amount)
                .fold(0, u64::saturating_add);
            unlocked = unlocked.saturating_sub(soft);
            locked = locked.saturating_add(soft);
        }
        (unlocked, locked)
    }

    /// `SubWallets::getBalance`: the balance of `keys` (all subwallets for `None`) at `height`
    /// (the daemon's `networkBlockCount`).
    #[must_use]
    pub fn balance(&self, keys: Option<&[PublicKey]>, height: u64, now: u64) -> Balance {
        let mut total = Balance::default();
        let mut add = |sub: &SubWallet| {
            let (u, l) = self.sub_balance(sub, height, now);
            total.unlocked = total.unlocked.saturating_add(u);
            total.locked = total.locked.saturating_add(l);
        };
        match keys {
            None => self.subwallets.iter().for_each(&mut add),
            Some(keys) => keys
                .iter()
                .filter_map(|k| self.subwallet(k))
                .for_each(&mut add),
        }
        total
    }

    /// `SubWallets::getBalances`.
    #[must_use]
    pub fn balances(&self, height: u64, now: u64) -> Vec<AddressBalance> {
        self.subwallets
            .iter()
            .map(|s| {
                let (unlocked, locked) = self.sub_balance(s, height, now);
                AddressBalance {
                    address: s.address.clone(),
                    unlocked,
                    locked,
                }
            })
            .collect()
    }

    /// `SubWallets::getTxPrivateKey`.
    #[must_use]
    pub fn tx_private_key(&self, hash: &Hash) -> Option<SecretKey> {
        self.tx_private_keys
            .iter()
            .find(|(h, _)| h == hash)
            .map(|(_, k)| *k)
    }

    /// A transaction by hash: confirmed, pending or cancelled.
    #[must_use]
    pub fn find_transaction(&self, hash: &Hash) -> Option<TransactionInfo> {
        if let Some(tx) = self.transactions.iter().find(|t| t.hash == *hash) {
            return Some(TransactionInfo {
                transaction: tx.clone(),
                state: TxState::Succeeded,
            });
        }
        if let Some(tx) = self.locked_transactions.iter().find(|t| t.hash == *hash) {
            return Some(TransactionInfo {
                transaction: tx.clone(),
                state: TxState::Succeeded,
            });
        }
        self.cancelled_transactions
            .iter()
            .find(|t| t.hash == *hash)
            .map(|tx| TransactionInfo {
                transaction: tx.clone(),
                state: TxState::Cancelled,
            })
    }

    pub(crate) fn find_transaction_mut(&mut self, hash: &Hash) -> Option<&mut Transaction> {
        self.transactions
            .iter_mut()
            .chain(self.locked_transactions.iter_mut())
            .chain(self.cancelled_transactions.iter_mut())
            .find(|t| t.hash == *hash)
    }

    /// The unspent, unlocked outputs of `keys` (all subwallets for `None`) that no delayed
    /// transaction holds (`SubWallet::getSpendableInputs`). View subwallets have none.
    pub(crate) fn spendable(
        &self,
        keys: Option<&[PublicKey]>,
        height: u64,
        now: u64,
    ) -> Vec<Candidate> {
        let subs: Vec<&SubWallet> = match keys {
            None => self.subwallets.iter().collect(),
            Some(keys) => keys.iter().filter_map(|k| self.subwallet(k)).collect(),
        };
        let mut out = Vec::new();
        for sub in subs {
            let Some(secret) = sub.private_spend_key else {
                continue;
            };
            for input in &sub.unspent {
                if self.reserved.contains(&input.key_image)
                    || !xkr_wallet_core::rules::is_unlocked(input.unlock_time, height, now)
                    || self.soft_locked(input)
                {
                    continue;
                }
                out.push(Candidate {
                    owner: sub.public_spend_key,
                    spend_secret: secret,
                    input: input.clone(),
                });
            }
        }
        out
    }

    // -----------------------------------------------------------------------------------------
    // subwallets

    /// The tracking mode a new address must have (`WalletGreen::getTrackingMode`): any while
    /// the wallet has no address, which then takes the mode of the new address.
    fn check_mode(&mut self, view: bool) -> Result<(), WalletError> {
        if self.subwallets.is_empty() {
            self.is_view_wallet = view;
            return Ok(());
        }
        match (self.is_view_wallet, view) {
            (true, false) => Err(ErrorCode::IllegalViewWalletOperation.into()),
            (false, true) => Err(ErrorCode::IllegalNonViewWalletOperation.into()),
            _ => Ok(()),
        }
    }

    /// After the first address went into a wallet without addresses: sync starts afresh from
    /// that address (`WalletGreen::addWallet` with the synchronizer stopped while empty).
    fn first_address_added(&mut self, was_empty: bool, now: u64) {
        if was_empty {
            self.sync = SyncStatus::default();
            self.init_sync_start(now);
            self.generation += 1;
        }
    }

    /// `SubWallets::addSubWallet`: a new random spend key, scanning from now.
    pub(crate) fn add_subwallet(
        &mut self,
        spend_secret: SecretKey,
        now: u64,
    ) -> Result<String, WalletError> {
        let was_empty = self.subwallets.is_empty();
        self.check_mode(false)?;
        let public = secret_key_to_public_key(&spend_secret)
            .ok_or_else(|| WalletError::new(ErrorCode::InvalidKeyFormat))?;
        if self.index.contains_key(&public) {
            return Err(ErrorCode::SubwalletAlreadyExists.into());
        }
        let address = Address::new(public, self.view_public).to_string();
        let mut sub = SubWallet::new(
            public,
            Some(spend_secret),
            address.clone(),
            0,
            current_timestamp_adjusted(now),
            false,
        );
        sub.creation_timestamp = Some(now);
        sub.is_primary = was_empty;
        self.push_subwallet(sub);
        self.first_address_added(was_empty, now);
        Ok(address)
    }

    /// `SubWallets::importSubWallet`.
    #[cfg(test)]
    pub(crate) fn import_subwallet(
        &mut self,
        spend_secret: SecretKey,
        scan_height: u64,
    ) -> Result<String, WalletError> {
        self.import_subwallets(&[spend_secret], scan_height)
            .map(|mut addresses| addresses.remove(0))
    }

    /// `SubWallets::importSubWallet` for several keys at once (walletd
    /// `WalletGreen::createAddressList`): every key is checked first, so on an error none is
    /// added. Returns the addresses in the order of the keys.
    pub(crate) fn import_subwallets(
        &mut self,
        spend_secrets: &[SecretKey],
        scan_height: u64,
    ) -> Result<Vec<String>, WalletError> {
        let was_empty = self.subwallets.is_empty();
        let was_view = self.is_view_wallet;
        self.check_mode(false)?;
        let mut publics = Vec::with_capacity(spend_secrets.len());
        for secret in spend_secrets {
            let checked = secret_key_to_public_key(secret)
                .ok_or_else(|| WalletError::new(ErrorCode::InvalidKeyFormat))
                .and_then(|public| {
                    if self.index.contains_key(&public) || publics.contains(&public) {
                        Err(ErrorCode::SubwalletAlreadyExists.into())
                    } else {
                        Ok(public)
                    }
                });
            match checked {
                Ok(public) => publics.push(public),
                Err(e) => {
                    self.is_view_wallet = was_view;
                    return Err(e);
                }
            }
        }
        let mut addresses = Vec::with_capacity(publics.len());
        for (i, (secret, public)) in spend_secrets.iter().zip(publics).enumerate() {
            let address = Address::new(public, self.view_public).to_string();
            self.subwallets.push(SubWallet::new(
                public,
                Some(*secret),
                address.clone(),
                scan_height,
                0,
                was_empty && i == 0,
            ));
            addresses.push(address);
        }
        if !addresses.is_empty() {
            // the indexes and scan keys once for the whole list
            self.keys_changed();
            self.first_address_added(was_empty, unix_now());
        }
        Ok(addresses)
    }

    /// `SubWallets::importViewSubWallet`.
    pub(crate) fn import_view_subwallet(
        &mut self,
        public: PublicKey,
        scan_height: u64,
    ) -> Result<String, WalletError> {
        let was_empty = self.subwallets.is_empty();
        let was_view = self.is_view_wallet;
        self.check_mode(true)?;
        let refused = if self.index.contains_key(&public) {
            Some(ErrorCode::SubwalletAlreadyExists)
        } else if !xkr_crypto::check_key(&public) {
            Some(ErrorCode::InvalidKeyFormat)
        } else {
            None
        };
        if let Some(code) = refused {
            self.is_view_wallet = was_view;
            return Err(code.into());
        }
        let address = Address::new(public, self.view_public).to_string();
        self.push_subwallet(SubWallet::new(
            public,
            None,
            address.clone(),
            scan_height,
            0,
            was_empty,
        ));
        self.first_address_added(was_empty, unix_now());
        Ok(address)
    }

    /// `SubWallets::deleteSubWallet`: removes the subwallet and its transfers (a transaction
    /// that only touched it is removed). The primary address cannot be deleted.
    pub(crate) fn delete_subwallet(&mut self, key: &PublicKey) -> Result<(), WalletError> {
        self.remove_subwallet(key, false)
    }

    /// walletd `WalletGreen::deleteAddress`: removes any address, also the first or the only
    /// one. The next address becomes the primary one; without addresses the wallet forgets its
    /// sync progress (C++ clears its block list back to the genesis block) until an address is
    /// added again.
    pub(crate) fn delete_address(&mut self, key: &PublicKey) -> Result<(), WalletError> {
        self.remove_subwallet(key, true)
    }

    fn remove_subwallet(
        &mut self,
        key: &PublicKey,
        allow_primary: bool,
    ) -> Result<(), WalletError> {
        let Some(&i) = self.index.get(key) else {
            return Err(ErrorCode::AddressNotInWallet.into());
        };
        let was_primary = self.subwallets[i].is_primary;
        if was_primary && !allow_primary {
            return Err(ErrorCode::CannotDeletePrimaryAddress.into());
        }
        self.subwallets.remove(i);
        for list in [
            &mut self.transactions,
            &mut self.locked_transactions,
            &mut self.cancelled_transactions,
        ] {
            list.retain_mut(|tx| {
                if !tx.involves(key) {
                    return true;
                }
                if tx.transfers.len() == 1 {
                    return false;
                }
                tx.transfers.retain(|(k, _)| k != key);
                true
            });
        }
        // `deleteFromUncommitedTransactions`: delayed transactions spending its outputs go
        self.delayed.remove_where(&mut self.reserved, |p| {
            p.inputs.iter().any(|(owner, _)| owner == key)
        });
        if was_primary && let Some(first) = self.subwallets.first_mut() {
            first.is_primary = true;
        }
        if self.subwallets.is_empty() {
            self.sync = SyncStatus::default();
            self.start_height = 0;
            self.start_timestamp = 0;
            self.generation += 1;
        }
        self.keys_changed();
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // transactions

    /// `SubWallets::addTransaction`: a confirmed transaction; the pending copy (sent by this
    /// wallet) is removed and what it knew is kept. A transaction already present is replaced.
    pub(crate) fn add_transaction(&mut self, mut tx: Transaction) {
        if let Some(pos) = self
            .locked_transactions
            .iter()
            .position(|t| t.hash == tx.hash)
        {
            let old = self.locked_transactions.remove(pos);
            tx.inherit(&old);
        }
        if let Some(pos) = self
            .cancelled_transactions
            .iter()
            .position(|t| t.hash == tx.hash)
        {
            let old = self.cancelled_transactions.remove(pos);
            tx.inherit(&old);
        }
        if let Some(existing) = self.transactions.iter_mut().find(|t| t.hash == tx.hash) {
            tx.inherit(existing);
            *existing = tx;
        } else {
            self.transactions.push(tx);
        }
    }

    /// `SubWallets::addUnconfirmedTransaction`.
    pub(crate) fn add_unconfirmed(&mut self, tx: Transaction) {
        if !self.locked_transactions.iter().any(|t| t.hash == tx.hash) {
            self.locked_transactions.push(tx);
        }
    }

    /// `SubWallets::storeTransactionInput`.
    ///
    /// An output that a pending sent transaction spends (kept through a [`Self::reset`]) is
    /// stored as locked.
    pub(crate) fn store_input(&mut self, owner: &PublicKey, input: OwnedInput) {
        let is_view = self.is_view_wallet;
        let key_image = input.key_image;
        let spent_by_pending = !is_view
            && self
                .locked_transactions
                .iter()
                .any(|t| t.spent_key_images.contains(&key_image));
        if let Some(sub) = self.subwallet_mut(owner) {
            sub.store_input(input, is_view);
            if spent_by_pending {
                sub.mark_locked(&key_image);
            }
            if !is_view {
                self.key_images.insert(key_image, *owner);
            }
        }
    }

    /// `SubWallets::markInputAsSpent`.
    pub(crate) fn mark_spent(&mut self, owner: &PublicKey, key_image: &KeyImage, height: u64) {
        if let Some(sub) = self.subwallet_mut(owner) {
            sub.mark_spent(key_image, height);
        }
        self.key_images.remove(key_image);
        self.reserved.remove(key_image);
    }

    /// `SubWallets::markInputAsLocked`.
    pub(crate) fn mark_locked(&mut self, owner: &PublicKey, key_image: &KeyImage) {
        if let Some(sub) = self.subwallet_mut(owner) {
            sub.mark_locked(key_image);
        }
        self.reserved.remove(key_image);
    }

    /// `SubWallets::storeUnconfirmedIncomingInput`.
    pub(crate) fn store_unconfirmed_incoming(
        &mut self,
        owner: &PublicKey,
        input: UnconfirmedInput,
    ) {
        if let Some(sub) = self.subwallet_mut(owner) {
            sub.unconfirmed_incoming.push(input);
        }
    }

    /// `SubWallets::storeTxPrivateKey`.
    pub(crate) fn store_tx_private_key(&mut self, hash: Hash, key: SecretKey) {
        if let Some(entry) = self.tx_private_keys.iter_mut().find(|(h, _)| *h == hash) {
            entry.1 = key;
        } else {
            self.tx_private_keys.push((hash, key));
        }
    }

    /// `SubWallets::removeForkedTransactions` and the synchronizer's rewind: everything at or
    /// above `fork_height` is forgotten.
    pub(crate) fn remove_forked(&mut self, fork_height: u64) {
        self.transactions.retain(|t| t.block_height < fork_height);
        for sub in &mut self.subwallets {
            sub.remove_forked(fork_height);
        }
        self.sync.rewind(fork_height);
        self.rebuild_key_images();
    }

    /// wallet-backend-js `SubWallets.pruneSpentInputs`: forgets the spent outputs spent below
    /// `prune_height`. Spent outputs only matter for undoing a reorg, which does not reach that
    /// far back.
    pub(crate) fn prune_spent_inputs(&mut self, prune_height: u64) {
        for sub in &mut self.subwallets {
            sub.prune_spent_inputs(prune_height);
        }
    }

    /// `SubWallets::removeCancelledTransactions`: drops pending transactions the node no longer
    /// knows and makes their inputs spendable again. Returns the cancelled transactions.
    pub(crate) fn remove_cancelled(&mut self, cancelled: &HashSet<Hash>) -> Vec<Transaction> {
        let (gone, kept): (Vec<Transaction>, Vec<Transaction>) =
            std::mem::take(&mut self.locked_transactions)
                .into_iter()
                .partition(|t| cancelled.contains(&t.hash));
        self.locked_transactions = kept;
        if gone.is_empty() {
            return gone;
        }
        let mut key_images: HashSet<KeyImage> = gone
            .iter()
            .flat_map(|t| t.spent_key_images.iter().copied())
            .collect();
        // A pending transaction from a file that does not list its inputs: once no pending
        // transaction is left, every locked output is free again.
        if self.locked_transactions.is_empty() && gone.iter().any(|t| t.spent_key_images.is_empty())
        {
            for sub in &self.subwallets {
                key_images.extend(sub.locked.iter().map(|i| i.key_image));
            }
        }
        for sub in &mut self.subwallets {
            sub.remove_cancelled(cancelled);
            sub.unlock_key_images(&key_images);
        }
        self.rebuild_key_images();
        self.cancelled_transactions.extend(gone.iter().cloned());
        gone
    }

    /// `SubWallets::reset` and `WalletSynchronizer::reset`: forgets the transactions, outputs
    /// and sync progress; every subwallet scans from `scan_height`.
    ///
    /// Sent transactions that are not in a block yet are kept with their private keys and
    /// change, as walletd's `WalletGreen` keeps seeing them in the node's pool after its
    /// reset: the outputs they spend are locked again when the scan finds them
    /// ([`Self::store_input`]), so no new transaction spends them a second time. The node
    /// still decides: one it forgets is cancelled as usual.
    pub(crate) fn reset(&mut self, scan_height: u64) {
        let pending = std::mem::take(&mut self.locked_transactions);
        let pending_keys: Vec<(Hash, SecretKey)> = self
            .tx_private_keys
            .iter()
            .filter(|(h, _)| pending.iter().any(|t| t.hash == *h))
            .copied()
            .collect();
        self.transactions.clear();
        self.cancelled_transactions.clear();
        for sub in &mut self.subwallets {
            let change: Vec<UnconfirmedInput> = sub
                .unconfirmed_incoming
                .iter()
                .filter(|u| pending.iter().any(|t| t.hash == u.parent_transaction_hash))
                .cloned()
                .collect();
            sub.reset(scan_height);
            sub.unconfirmed_incoming = change;
        }
        self.locked_transactions = pending;
        self.tx_private_keys = pending_keys;
        self.sync = SyncStatus::default();
        self.start_height = scan_height;
        self.start_timestamp = 0;
        self.reserved.clear();
        self.delayed.clear();
        self.key_images.clear();
        self.generation += 1;
    }

    /// `SubWallets::convertSyncTimestampToHeight` and the synchronizer's switch from a start
    /// timestamp to the height of the first block received.
    pub(crate) fn convert_sync_timestamp(&mut self, height: u64) {
        if self.start_timestamp == 0 {
            return;
        }
        self.start_timestamp = 0;
        self.start_height = height;
        for sub in &mut self.subwallets {
            sub.convert_sync_timestamp(0, height);
        }
    }

    /// Keeps a delayed transaction and sets its inputs aside.
    pub(crate) fn delayed_insert(&mut self, prepared: Prepared) {
        self.delayed.insert(&mut self.reserved, prepared);
    }

    /// Deletes the delayed transaction with `hash` and frees its inputs; false if there is
    /// none.
    pub(crate) fn delayed_delete(&mut self, hash: &Hash) -> bool {
        self.delayed.delete(&mut self.reserved, hash)
    }

    /// Hashes of the pending sent transactions.
    #[must_use]
    pub fn locked_transaction_hashes(&self) -> Vec<Hash> {
        self.locked_transactions.iter().map(|t| t.hash).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::subwallet::tests::input;

    fn wallet() -> WalletState {
        let keys = xkr_wallet_core::WalletKeys::generate(&mut xkr_crypto::os_rng());
        WalletState::new_spend(keys.spend_secret, keys.view_secret, 0, false, 1_700_000_000)
            .unwrap()
    }

    #[test]
    fn subwallets_and_balances() {
        let mut s = wallet();
        let primary = s.primary().public_spend_key;
        let (_, extra) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        let address = s.add_subwallet(extra, 1_700_000_000).unwrap();
        assert!(s.add_subwallet(extra, 1_700_000_000).is_err());
        let sub_key = xkr_wallet_core::Address::parse(&address).unwrap().spend;
        s.store_input(&primary, input(1, 100, 5));
        s.store_input(&sub_key, input(2, 30, 6));
        assert_eq!(
            s.balance(None, 10, 0),
            Balance {
                unlocked: 130,
                locked: 0
            }
        );
        assert_eq!(s.balance(Some(&[sub_key]), 10, 0).unlocked, 30);
        assert_eq!(s.key_image_owner(&KeyImage([2; 32])), Some(sub_key));
        assert_eq!(s.spendable(None, 10, 0).len(), 2);
        s.reserved.insert(KeyImage([1; 32]));
        assert_eq!(s.spendable(None, 10, 0).len(), 1);
        assert_eq!(
            s.delete_subwallet(&primary).unwrap_err().code(),
            ErrorCode::CannotDeletePrimaryAddress
        );
        s.delete_subwallet(&sub_key).unwrap();
        assert_eq!(s.subwallets().len(), 1);
        assert_eq!(s.key_image_owner(&KeyImage([2; 32])), None);
    }

    #[test]
    fn cancelled_transactions_unlock_their_inputs() {
        let mut s = wallet();
        let primary = s.primary().public_spend_key;
        s.store_input(&primary, input(1, 100, 5));
        s.store_input(&primary, input(2, 50, 5));
        s.mark_locked(&primary, &KeyImage([1; 32]));
        let mut tx = Transaction::new(Hash([0xcc; 32]));
        tx.add_transfer(primary, -100);
        tx.spent_key_images = vec![KeyImage([1; 32])];
        s.add_unconfirmed(tx);
        assert_eq!(s.balance(None, 10, 0).unlocked, 50);
        let gone = s.remove_cancelled(&HashSet::from([Hash([0xcc; 32])]));
        assert_eq!(gone.len(), 1);
        assert_eq!(s.balance(None, 10, 0).unlocked, 150);
        assert_eq!(
            s.find_transaction(&Hash([0xcc; 32])).unwrap().state,
            TxState::Cancelled
        );
    }

    #[test]
    fn pending_sends_outlive_a_reset() {
        let mut s = wallet();
        let primary = s.primary().public_spend_key;
        s.store_input(&primary, input(1, 100, 5));
        s.store_input(&primary, input(2, 50, 5));
        s.mark_locked(&primary, &KeyImage([1; 32]));
        let mut tx = Transaction::new(Hash([0xcc; 32]));
        tx.add_transfer(primary, -100);
        tx.spent_key_images = vec![KeyImage([1; 32])];
        s.add_unconfirmed(tx);
        s.store_tx_private_key(Hash([0xcc; 32]), SecretKey([3; 32]));
        s.store_unconfirmed_incoming(
            &primary,
            UnconfirmedInput {
                amount: 40,
                key: PublicKey([77; 32]),
                parent_transaction_hash: Hash([0xcc; 32]),
                other: UnknownFields::new(),
            },
        );
        s.reset(0);
        assert_eq!(s.locked_transactions().len(), 1);
        assert_eq!(
            s.tx_private_key(&Hash([0xcc; 32])),
            Some(SecretKey([3; 32]))
        );
        assert_eq!(
            s.balance(None, 10, 0),
            Balance {
                unlocked: 0,
                locked: 40
            }
        );
        // the scan finds both outputs again: the one the pending send spends stays locked
        s.store_input(&primary, input(1, 100, 5));
        s.store_input(&primary, input(2, 50, 5));
        assert_eq!(
            s.balance(None, 10, 0),
            Balance {
                unlocked: 50,
                locked: 40
            }
        );
        assert_eq!(s.spendable(None, 10, 0).len(), 1);
    }

    #[test]
    fn import_list_checks_every_key_first() {
        let mut s = wallet();
        let (_, a) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        let (_, b) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        // a repeated key: nothing is added
        assert_eq!(
            s.import_subwallets(&[a, b, a], 5).unwrap_err().code(),
            ErrorCode::SubwalletAlreadyExists
        );
        assert_eq!(s.subwallets().len(), 1);
        let addresses = s.import_subwallets(&[a, b], 5).unwrap();
        assert_eq!(addresses.len(), 2);
        assert_eq!(s.subwallets()[1].address, addresses[0]);
        assert_eq!(s.subwallets()[2].sync_start_height, 5);
        assert!(!s.subwallets()[1].is_primary);
        assert_eq!(
            s.import_subwallets(&[b], 0).unwrap_err().code(),
            ErrorCode::SubwalletAlreadyExists
        );
    }

    #[test]
    fn walletd_deletes_every_address_and_takes_any_mode_after() {
        let mut s = wallet();
        let primary = s.primary().public_spend_key;
        let (_, extra) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        let address = s.add_subwallet(extra, 1_700_000_000).unwrap();
        let second = xkr_wallet_core::Address::parse(&address).unwrap().spend;
        s.store_input(&primary, input(1, 100, 5));
        let mut tx = Transaction::new(Hash([0xdd; 32]));
        tx.add_transfer(primary, 100);
        tx.block_height = 5;
        s.add_transaction(tx);
        s.sync.store_block_hash(Hash([6; 32]), 5);
        // the primary address goes, the next one is primary now
        s.delete_address(&primary).unwrap();
        assert_eq!(s.try_primary().unwrap().public_spend_key, second);
        assert!(s.transactions().is_empty());
        assert_eq!(s.sync_status().height(), 5);
        // the last one goes: no address, no progress
        s.delete_address(&second).unwrap();
        assert!(s.try_primary().is_none());
        assert!(s.sync_status().is_empty());
        assert_eq!(s.balance(None, 10, 0), Balance::default());
        // a tracking address makes it a view wallet
        let (tracked, _) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        s.import_view_subwallet(tracked, 7).unwrap();
        assert!(s.is_view_wallet());
        assert!(s.primary().is_primary);
        assert_eq!((s.start_height, s.start_timestamp), (7, 0));
        let (_, spend) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        assert_eq!(
            s.import_subwallet(spend, 0).unwrap_err().code(),
            ErrorCode::IllegalViewWalletOperation
        );
        // WalletBackend keeps its primary address
        let mut w = wallet();
        let primary = w.primary().public_spend_key;
        assert_eq!(
            w.delete_subwallet(&primary).unwrap_err().code(),
            ErrorCode::CannotDeletePrimaryAddress
        );
    }

    #[test]
    fn sync_start_prefers_the_earlier_of_height_and_timestamp() {
        let mut s = wallet();
        s.subwallets[0].sync_start_height = 1_000_000;
        s.subwallets[0].sync_start_timestamp = 0;
        let (_, extra) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
        let now = 1_700_000_000;
        s.add_subwallet(extra, now).unwrap();
        // each minimum is taken on its own: height 0 (new subwallet) and timestamp 0 (first)
        assert_eq!(s.min_initial_sync_start(now), (0, 0));
        s.subwallets[0].sync_start_height = 500;
        s.subwallets[1].sync_start_height = 700;
        assert_eq!(s.min_initial_sync_start(now), (500, 0));
        assert_eq!(
            timestamp_to_scan_height(scan_height_to_timestamp(1000, now)),
            1000
        );
    }
}
