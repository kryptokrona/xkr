//! Format B: walletd and zedwallet containers (`src/wallet/wallet_green.cpp`).
//!
//! The container keeps the keys; the engine's own state lives next to it in
//! `<container>.xkrstate`, a format A file encrypted with the container password. The container
//! itself is rewritten (keys only, empty cache, as `xkr_wallet_files::green::write_keys_only`
//! writes it) only when the keys change, so the C++ tools can still open it and rescan.
//!
//! A container opened for the first time (no sidecar, or a sidecar for other keys) starts from
//! its keys and creation timestamps and takes the transaction history of the C++ cache
//! (`WalletGreen` transactions and transfers): outgoing destinations and `extra` cannot be
//! recovered by a rescan. The rescan then fills in outputs and replaces the imported records
//! with what it finds, keeping their destinations and `extra`.

use std::path::{Path, PathBuf};

use xkr_types::{Hash, PublicKey, payment_id};
use xkr_wallet_core::Address;
use xkr_wallet_files::green::cache::{WALLET_UNCONFIRMED_TRANSACTION_HEIGHT, transaction_state};
use xkr_wallet_files::green::{GreenRecord, GreenViewKeys, GreenWallet};

/// The cache types and constants of a walletd container, for the history that walletd
/// `export` writes ([`crate::Wallet::export_container_with_history`]).
pub use xkr_wallet_files::green::cache;
pub use xkr_wallet_files::green::{GreenCachedKey, GreenHistory, GreenTransaction, GreenTransfer};

use crate::error::{ErrorCode, WalletError};
use crate::state::{
    SubWallet, Transaction, TransferDestination, TransferKind, WalletState,
    scan_height_to_timestamp,
};
use crate::validate::spend_key_of;

/// Appended to the container path for the engine's state file.
pub const SIDECAR_SUFFIX: &str = ".xkrstate";

/// `<container>.xkrstate`.
#[must_use]
pub fn sidecar_path(container: &Path) -> PathBuf {
    let mut name = container.as_os_str().to_owned();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// A fresh state from the container's keys (`WalletGreen::load` up to the keys): the first
/// record is the primary address; every address scans from its creation timestamp. The cache
/// history is imported when there is one. A container may have no address (walletd
/// `deleteAddress` of the last one).
pub(crate) fn state_from_green(green: &GreenWallet, now: u64) -> Result<WalletState, WalletError> {
    let is_view = green.is_tracking();
    let mut state = WalletState::empty(green.view.secret_key, is_view)?;
    if state.view_public != green.view.public_key {
        return Err(ErrorCode::WrongPassword.into());
    }
    state.subwallets = green
        .records
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let address = Address::new(r.spend_public_key, green.view.public_key).to_string();
            let mut sub = SubWallet::new(
                r.spend_public_key,
                r.spend_secret_key,
                address,
                0,
                r.creation_timestamp,
                i == 0,
            );
            sub.creation_timestamp = Some(r.creation_timestamp);
            sub
        })
        .collect();
    state.keys_changed();
    state.init_sync_start(now);
    if let Some(history) = &green.history {
        import_history(&mut state, history);
    }
    Ok(state)
}

/// Whether a state loaded from the sidecar belongs to `green` (same view key, same addresses).
pub(crate) fn matches(state: &WalletState, green: &GreenWallet) -> bool {
    if state.view_secret != green.view.secret_key || state.subwallets.len() != green.records.len() {
        return false;
    }
    green.records.iter().all(|r| {
        state
            .subwallet(&r.spend_public_key)
            .is_some_and(|s| s.private_spend_key == r.spend_secret_key)
    })
}

/// Takes the confirmed transactions of a walletd cache into `state`
/// (`WalletSerializerV2::loadTransactions`, `loadTransfers`): amounts per own address become
/// the transfers, every transfer of the cache is kept as a destination, and the `extra` and
/// payment id come from the cached `extra`.
pub(crate) fn import_history(state: &mut WalletState, history: &GreenHistory) {
    let Some(primary) = state.try_primary().map(|s| s.public_spend_key) else {
        return;
    };
    for (id, cached) in history.transactions.iter().enumerate() {
        if cached.state != transaction_state::SUCCEEDED
            || cached.block_height == WALLET_UNCONFIRMED_TRANSACTION_HEIGHT
        {
            continue;
        }
        let mut tx = Transaction::new(cached.hash);
        tx.fee = cached.fee;
        tx.block_height = u64::from(cached.block_height);
        tx.timestamp = cached.timestamp;
        tx.unlock_time = cached.unlock_time;
        tx.is_coinbase = cached.is_base;
        tx.payment_id = payment_id(&cached.extra)
            .as_ref()
            .map_or_default(Hash::to_hex);
        tx.extra = Some(cached.extra.clone());
        for transfer in history
            .transfers
            .iter()
            .filter(|t| t.transaction_id == id as u64)
        {
            let own: Option<PublicKey> =
                spend_key_of(&transfer.address).filter(|k| state.subwallet(k).is_some());
            if let Some(key) = own {
                tx.add_transfer(key, transfer.amount);
            }
            tx.destinations.push(TransferDestination {
                address: transfer.address.clone(),
                amount: transfer.amount,
                kind: TransferKind::from_number(transfer.transfer_type),
            });
        }
        if tx.transfers.is_empty() {
            tx.add_transfer(primary, cached.total_amount);
        }
        state.add_transaction(tx);
    }
}

/// The container for `state`: the view keys with `view_timestamp`, and one record per
/// subwallet, the primary first, with its creation timestamp (or an estimate from its scan
/// start).
pub(crate) fn green_from_state(state: &WalletState, view_timestamp: u64, now: u64) -> GreenWallet {
    let mut subs: Vec<&SubWallet> = state.subwallets.iter().collect();
    if let Some(pos) = subs.iter().position(|s| s.is_primary) {
        let primary = subs.remove(pos);
        subs.insert(0, primary);
    }
    let records = subs
        .into_iter()
        .map(|s| GreenRecord {
            spend_public_key: s.public_spend_key,
            spend_secret_key: s.private_spend_key,
            creation_timestamp: s.creation_timestamp.unwrap_or_else(|| {
                if s.sync_start_timestamp != 0 {
                    s.sync_start_timestamp
                } else {
                    scan_height_to_timestamp(s.sync_start_height, now)
                }
            }),
        })
        .collect();
    GreenWallet {
        view: GreenViewKeys {
            public_key: state.view_public,
            secret_key: state.view_secret,
            creation_timestamp: view_timestamp,
        },
        records,
        history: None,
        cache_error: None,
    }
}

#[cfg(test)]
mod tests {
    use xkr_wallet_files::green::{GreenTransaction, GreenTransfer};

    use super::*;

    fn container(n: usize) -> GreenWallet {
        let base = xkr_wallet_core::WalletKeys::generate(&mut xkr_crypto::os_rng());
        GreenWallet {
            view: GreenViewKeys {
                public_key: base.view_public,
                secret_key: base.view_secret,
                creation_timestamp: 1_600_000_000,
            },
            records: (0..n)
                .map(|i| {
                    let (p, s) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
                    GreenRecord {
                        spend_public_key: p,
                        spend_secret_key: Some(s),
                        creation_timestamp: 1_600_000_000 + i as u64,
                    }
                })
                .collect(),
            history: None,
            cache_error: None,
        }
    }

    #[test]
    fn container_keys_become_subwallets_and_back() {
        let green = container(3);
        let state = state_from_green(&green, 1_700_000_000).unwrap();
        assert_eq!(state.subwallets().len(), 3);
        assert!(state.subwallets()[0].is_primary);
        assert_eq!(state.start_timestamp, 1_600_000_000);
        assert!(matches(&state, &green));
        let back = green_from_state(&state, 1_600_000_000, 1_700_000_000);
        assert_eq!(back, green);
        assert!(!matches(&state, &container(3)));
    }

    #[test]
    fn history_is_imported() {
        let mut green = container(2);
        let own = Address::new(green.records[1].spend_public_key, green.view.public_key);
        let mut extra = vec![0x01];
        extra.extend_from_slice(&[9u8; 32]);
        extra.extend_from_slice(&[0x02, 33, 0x00]);
        extra.extend_from_slice(&[0xab; 32]);
        green.history = Some(GreenHistory {
            save_level: 1,
            cached_keys: Vec::new(),
            transactions: vec![
                GreenTransaction {
                    state: transaction_state::SUCCEEDED,
                    timestamp: 1_650_000_000,
                    block_height: 77,
                    hash: Hash([1; 32]),
                    total_amount: -1_010,
                    fee: 10,
                    creation_time: 0,
                    unlock_time: 0,
                    extra: extra.clone(),
                    is_base: false,
                },
                GreenTransaction {
                    state: transaction_state::CANCELLED,
                    hash: Hash([2; 32]),
                    ..GreenTransaction::default()
                },
            ],
            transfers: vec![
                GreenTransfer {
                    transaction_id: 0,
                    address: "SEKRsomeoneelse".into(),
                    amount: 1_000,
                    transfer_type: 0,
                },
                GreenTransfer {
                    transaction_id: 0,
                    address: own.to_string(),
                    amount: -1_010,
                    transfer_type: 0,
                },
            ],
        });
        let state = state_from_green(&green, 1_700_000_000).unwrap();
        assert_eq!(state.transactions().len(), 1);
        let tx = &state.transactions()[0];
        assert_eq!(tx.transfers, vec![(own.spend, -1_010)]);
        assert_eq!(tx.destinations.len(), 2);
        assert_eq!(tx.payment_id, "ab".repeat(32));
        assert_eq!(tx.block_height, 77);
        assert_eq!(tx.extra.as_deref(), Some(&extra[..]));
        assert_eq!(
            sidecar_path(Path::new("/x/wallet.bin")),
            PathBuf::from("/x/wallet.bin.xkrstate")
        );
    }
}
