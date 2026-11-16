//! Whole-wallet methods: `save`, `export`, `reset`, `getViewKey`, `getMnemonicSeed`,
//! `getStatus`, `getAddresses`, `getFeeInfo` / `getNodeFeeInfo`, `validateAddress`,
//! `createIntegratedAddress` (`src/wallet_service/wallet_service.cpp` `WalletService::*` of the
//! same names).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_wallet::persist::green::cache::{
    SAVE_KEYS_AND_TRANSACTIONS, WALLET_UNCONFIRMED_TRANSACTION_HEIGHT, transaction_state,
};
use xkr_wallet::persist::green::{GreenCachedKey, GreenHistory, GreenTransaction, GreenTransfer};
use xkr_wallet::{ErrorCode, TransactionInfo, TxState};

use super::MethodResult;
use crate::errors::{AppError, ServiceCode, WalletCode};
use crate::model::{self, uint};
use crate::params::Params;
use crate::service::Service;

/// `WalletService::saveWalletNoThrow`.
pub async fn save<T: Transport + 'static>(service: &Service<T>, params: &Value) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    service.wallet().save().await?;
    tracing::info!("Wallet is saved");
    Ok(json!({}))
}

/// `boost::filesystem::path(walletFile).parent_path() / fileName`: the file name is appended
/// to the wallet's directory, even when it starts with a separator.
#[must_use]
pub fn export_path(wallet: &Path, file_name: &str) -> PathBuf {
    let Some(dir) = wallet.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return PathBuf::from(file_name);
    };
    let mut path = dir.as_os_str().to_owned();
    if !file_name.starts_with(std::path::is_separator) {
        path.push(std::path::MAIN_SEPARATOR_STR);
    }
    path.push(file_name);
    PathBuf::from(path)
}

/// What `WalletGreen::saveWalletCache` writes of the wallet's history, up to the transfers
/// (`WalletSerializerV2::saveKeyListAndBalances`, `saveTransactions`, `saveTransfers`): the
/// addresses, then the confirmed, pending and cancelled transactions with their walletd
/// transfers. Delayed transactions are left out, as C++ leaves them out below `SAVE_ALL`.
pub async fn export_history<T: Transport + 'static>(
    service: &Service<T>,
) -> Result<GreenHistory, AppError> {
    let wallet = service.wallet();
    let with_state = |list: Vec<xkr_wallet::Transaction>, state: TxState| {
        list.into_iter()
            .map(move |transaction| TransactionInfo { transaction, state })
    };
    let mut infos: Vec<TransactionInfo> =
        with_state(wallet.transactions(None, None)?, TxState::Succeeded).collect();
    infos.extend(with_state(wallet.unconfirmed(None)?, TxState::Succeeded));
    infos.extend(with_state(wallet.cancelled(None)?, TxState::Cancelled));
    let records = service.wallet_records(&infos).await;
    let mut transactions = Vec::with_capacity(infos.len());
    let mut transfers = Vec::new();
    for (id, (info, (list, extra))) in infos.iter().zip(records).enumerate() {
        let tx = &info.transaction;
        let state = match info.state {
            TxState::Cancelled => transaction_state::CANCELLED,
            _ => transaction_state::SUCCEEDED,
        };
        let block_height = if model::is_pending(info) {
            WALLET_UNCONFIRMED_TRANSACTION_HEIGHT
        } else {
            u32::try_from(tx.block_height).unwrap_or(WALLET_UNCONFIRMED_TRANSACTION_HEIGHT)
        };
        transactions.push(GreenTransaction {
            state,
            timestamp: tx.timestamp,
            block_height,
            hash: tx.hash,
            total_amount: tx.total_amount(),
            fee: tx.fee,
            creation_time: tx.timestamp,
            unlock_time: tx.unlock_time,
            extra,
            is_base: tx.is_coinbase,
        });
        transfers.extend(list.into_iter().map(|t| GreenTransfer {
            transaction_id: id as u64,
            address: t.address,
            amount: t.amount,
            transfer_type: t.kind,
        }));
    }
    Ok(GreenHistory {
        save_level: SAVE_KEYS_AND_TRANSACTIONS,
        cached_keys: service
            .own_addresses()
            .into_iter()
            .map(|(spend_public_key, _)| GreenCachedKey {
                spend_public_key,
                balances: None,
            })
            .collect(),
        transactions,
        transfers,
    })
}

/// `WalletService::exportWallet` (`WalletGreen::exportWallet`): a new container next to the
/// wallet file, encrypted with the wallet's password, with the keys and the history of
/// [`export_history`] as its cache; C++ walletd reads it and rescans the outputs. An existing
/// file is not overwritten (`EEXIST`).
pub async fn export<T: Transport + 'static>(service: &Service<T>, params: &Value) -> MethodResult {
    let file_name = Params::new(params)?.required_string("fileName")?;
    let _ready = service.serialize().await;
    let path = export_path(&service.wallet().path(), &file_name);
    tracing::info!("Exporting wallet to {}", path.display());
    // `ContainerStorage(path, CREATE)` refuses an existing file
    if let Err(e) = OpenOptions::new().write(true).create_new(true).open(&path) {
        return Err(AppError::io(&e).into());
    }
    let history = match export_history(service).await {
        Ok(history) => history,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e.into());
        }
    };
    if let Err(e) = service
        .wallet()
        .export_container_with_history(&path, Some(history))
        .await
    {
        let _ = std::fs::remove_file(&path);
        tracing::warn!("Error while exporting wallet: {e}");
        return Err(AppError::wallet(WalletCode::InternalWalletError).into());
    }
    Ok(json!({}))
}

/// `WalletService::resetWallet` (`WalletGreen::reset`): forget the transactions and outputs and
/// scan again from `scanHeight` (0 if not given).
pub async fn reset<T: Transport + 'static>(service: &Service<T>, params: &Value) -> MethodResult {
    let scan_height = Params::new(params)?.u64("scanHeight")?.unwrap_or(0);
    let _ready = service.serialize().await;
    tracing::info!("Resetting wallet");
    service.wallet().reset(scan_height).await?;
    service.clear_facts();
    tracing::info!("Wallet has been reset");
    Ok(json!({}))
}

/// `WalletService::getViewKey`.
pub async fn get_view_key<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    Ok(json!({ "viewSecretKey": service.wallet().keys().to_hex() }))
}

/// `WalletService::getMnemonicSeed`: only for an address whose spend key derives the wallet's
/// view key (`KEYS_NOT_DETERMINISTIC` otherwise, also for tracking addresses).
pub async fn get_mnemonic_seed<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let address = Params::new(params)?.required_string("address")?;
    let _ready = service.serialize().await;
    let key = service
        .own_key(&address)?
        .ok_or(AppError::wallet(WalletCode::ObjectNotFound))?;
    if service.is_tracking() {
        return Err(AppError::service(ServiceCode::KeysNotDeterministic).into());
    }
    let text = service
        .own_address_text(&key)
        .ok_or(AppError::wallet(WalletCode::ObjectNotFound))?;
    match service.wallet().mnemonic(&text) {
        Ok(seed) => Ok(json!({ "mnemonicSeed": seed })),
        Err(e) if e.code() == ErrorCode::IllegalViewWalletOperation => {
            Err(AppError::service(ServiceCode::KeysNotDeterministic).into())
        }
        Err(e) => Err(e.into()),
    }
}

/// `WalletService::getStatus`: the wallet's block count and last block hash, the node's
/// network and local block counts and its peers.
pub async fn get_status<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    let daemon = service.wallet().daemon_status();
    let info = daemon.info.as_ref();
    let known = info.map_or(0, |i| i.network_height);
    let local = info.map_or(0, |i| i.height);
    let peers = info.map_or(0, xkr_daemon_client::Info::peer_count);
    let block_count = service.block_count();
    let last = service.block_hash(block_count - 1).await?;
    Ok(json!({
        "blockCount": block_count as u32,
        "knownBlockCount": known as u32,
        "localDaemonBlockCount": uint(local),
        "lastBlockHash": last.to_hex(),
        "peerCount": peers as u32,
    }))
}

/// `WalletService::getAddresses`, in container order.
pub async fn get_addresses<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    Ok(json!({ "addresses": service.wallet().addresses() }))
}

/// `WalletService::getFeeInfo`: the node's fee address and amount (`uint32_t`).
pub async fn get_fee_info<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    let node = service.wallet().node();
    Ok(json!({ "address": node.fee_address, "amount": node.fee_amount as u32 }))
}

/// `WalletService::validateAddress`: whether `address` is a valid standard address.
pub async fn validate_address<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let address = Params::new(params)?.required_string("address")?;
    let _ready = service.serialize().await;
    Ok(json!({ "isValid": model::standard_address(&address).is_some() }))
}

/// `WalletService::createIntegratedAddress`.
pub async fn create_integrated_address<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let address = p.required_string("address")?;
    let payment_id = p.required_string("paymentId")?;
    let _ready = service.serialize().await;
    let parsed =
        model::standard_address(&address).ok_or(AppError::wallet(WalletCode::BadAddress))?;
    model::validate_payment_id(&payment_id)?;
    Ok(json!({ "integratedAddress": model::integrated_address(&parsed, &payment_id) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_goes_next_to_the_wallet() {
        let sep = std::path::MAIN_SEPARATOR_STR;
        assert_eq!(
            export_path(Path::new("dir/w.wallet"), "x.wallet"),
            PathBuf::from(format!("dir{sep}x.wallet"))
        );
        assert_eq!(
            export_path(Path::new("w.wallet"), "x.wallet"),
            PathBuf::from("x.wallet")
        );
        assert_eq!(
            export_path(Path::new("/a/w.wallet"), "/x.wallet"),
            PathBuf::from("/a/x.wallet")
        );
    }
}
