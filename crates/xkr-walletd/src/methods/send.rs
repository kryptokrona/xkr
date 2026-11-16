//! Sending: `sendTransaction`, `createDelayedTransaction`, `getDelayedTransactionHashes`,
//! `deleteDelayedTransaction`, `sendDelayedTransaction`, `sendFusionTransaction`,
//! `estimateFusion` (`src/wallet_service/wallet_service.cpp` `WalletService::*` and
//! `src/wallet/wallet_green.cpp` `WalletGreen::transfer`, `makeTransaction`,
//! `validateTransactionParameters`, `prepareTransaction`, `commitTransaction`,
//! `rollbackUncommitedTransaction`, `createFusionTransaction`, `estimate`).
//!
//! The parameter checks run here in the C++ order with the C++ codes; the engine then selects
//! inputs, fetches ring members, signs and relays.

use serde_json::{Value, json};
use xkr_consensus::reward::{default_fusion_dust_threshold, mixin_limits};
use xkr_daemon_client::Transport;
use xkr_types::Hash;
use xkr_wallet::{FusionRequest, SendRequest};
use xkr_wallet_core::rules;

use super::MethodResult;
use crate::errors::{AppError, ServiceCode, WalletCode};
use crate::model;
use crate::params::{Invalid, Order, Params};
use crate::service::Service;

/// The parameters of `sendTransaction` and `createDelayedTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendParams {
    /// `addresses`: where the inputs come from (all addresses if empty).
    pub sources: Vec<String>,
    /// `transfers`.
    pub transfers: Vec<Order>,
    /// `changeAddress` (empty if not given).
    pub change_address: String,
    /// `fee`.
    pub fee: u64,
    /// `anonymity`, the network default when not given.
    pub anonymity: u64,
    /// `extra` as hex (empty if not given).
    pub extra: String,
    /// `paymentId` (empty if not given).
    pub payment_id: String,
    /// `unlockTime`.
    pub unlock_time: u64,
}

impl SendParams {
    /// `SendTransaction::Request::serialize`: `transfers` and `fee` are required; `extra` and
    /// `paymentId` exclude each other; `anonymity` defaults to `default_mixin`.
    pub fn read(params: &Value, default_mixin: u64) -> Result<Self, Invalid> {
        let p = Params::new(params)?;
        let sources = p.strings("addresses")?.unwrap_or_default();
        let transfers = p.orders("transfers")?.ok_or(Invalid)?;
        let change_address = p.string("changeAddress")?.unwrap_or_default();
        let fee = p.u64("fee")?.ok_or(Invalid)?;
        let anonymity = p.u64("anonymity")?.unwrap_or(default_mixin);
        let extra = p.string("extra")?;
        let payment_id = p.string("paymentId")?;
        if extra.is_some() && payment_id.is_some() {
            return Err(Invalid);
        }
        let unlock_time = p.u64("unlockTime")?.unwrap_or(0);
        Ok(Self {
            sources,
            transfers,
            change_address,
            fee,
            anonymity,
            extra: extra.unwrap_or_default(),
            payment_id: payment_id.unwrap_or_default(),
            unlock_time,
        })
    }
}

/// `WalletService::getDefaultMixin`.
fn default_mixin<T: Transport + 'static>(service: &Service<T>) -> u64 {
    rules::default_mixin(service.network_height())
}

/// `WalletGreen::validateSourceAddresses` after `validateAddresses`: every source is an own
/// address (`BAD_ADDRESS`).
fn check_sources<T: Transport + 'static>(
    service: &Service<T>,
    sources: &[String],
) -> Result<(), AppError> {
    for source in sources {
        if service.own_key(source)?.is_none() {
            return Err(WalletCode::BadAddress.into());
        }
    }
    Ok(())
}

/// `WalletGreen::validateChangeDestination` and `getChangeDestination`: the given address
/// (which must be an own one), the only address, or the only source.
fn change_destination<T: Transport + 'static>(
    service: &Service<T>,
    sources: &[String],
    given: &str,
    fusion: bool,
) -> Result<String, AppError> {
    let addresses = service.wallet().addresses();
    if given.is_empty() {
        if sources.len() > 1 || (sources.is_empty() && addresses.len() > 1) {
            let code = if fusion {
                WalletCode::DestinationAddressRequired
            } else {
                WalletCode::ChangeAddressRequired
            };
            return Err(code.into());
        }
        return sources
            .first()
            .cloned()
            .or_else(|| addresses.first().cloned())
            .ok_or_else(|| WalletCode::InternalWalletError.into());
    }
    if model::standard_address(given).is_none() {
        return Err(WalletCode::BadAddress.into());
    }
    if service.own_key(given)?.is_none() {
        let code = if fusion {
            WalletCode::DestinationAddressNotFound
        } else {
            WalletCode::ChangeAddressNotFound
        };
        return Err(code.into());
    }
    Ok(given.to_owned())
}

/// `WalletService::sendTransaction` / `createDelayedTransaction` up to `WalletGreen::transfer`
/// / `makeTransaction`: integrated addresses, address checks, mixin limits (send only), the
/// payment id or raw `extra`, then `validateTransactionParameters` and `countNeededMoney`.
fn prepare<T: Transport + 'static>(
    service: &Service<T>,
    p: &SendParams,
    delayed: bool,
) -> Result<SendRequest, AppError> {
    // integrated payment ids are compared with the given one in upper case, as C++ does
    let mut payment_id = p.payment_id.to_ascii_uppercase();
    let mut integrated: Vec<String> = Vec::new();
    let mut transfers = p.transfers.clone();
    for t in &mut transfers {
        if model::standard_address(&t.address).is_none() {
            let (address, id) = model::decode_integrated_address(&t.address)?;
            if !payment_id.is_empty() && payment_id != id {
                return Err(WalletCode::ConflictingPaymentIds.into());
            }
            t.address = address;
            integrated.push(id);
        }
    }
    if let Some(first) = integrated.first() {
        if integrated.iter().any(|id| id != first) {
            return Err(WalletCode::ConflictingPaymentIds.into());
        }
        payment_id.clone_from(first);
    }
    model::validate_addresses(&p.sources)?;
    model::validate_addresses(&transfers.iter().map(|t| &t.address).collect::<Vec<_>>())?;
    if !p.change_address.is_empty() {
        model::validate_addresses(&[&p.change_address])?;
    }
    if !delayed {
        // `Mixins::validate(anonymity, node.getLastKnownBlockHeight())`
        let limits = mixin_limits(service.network_height());
        if p.anonymity < limits.min {
            return Err(WalletCode::MixinBelowThreshold.into());
        }
        if p.anonymity > limits.max {
            return Err(WalletCode::MixinAboveThreshold.into());
        }
    }
    let (payment_id, extra) = if payment_id.is_empty() {
        let extra = hex::decode(&p.extra).map_err(|_| {
            AppError::wallet(if delayed {
                WalletCode::InternalWalletError
            } else {
                WalletCode::BadTransactionExtra
            })
        })?;
        (String::new(), extra)
    } else {
        // `createTxExtraWithPaymentId`
        let id = model::hex32(&payment_id).ok_or(AppError::wallet(WalletCode::BadPaymentId))?;
        (Hash(id).to_hex(), Vec::new())
    };
    // `TransactionParameters::mixIn` is a `uint16_t`
    let mixin = u64::from(p.anonymity as u16);

    // WalletGreen
    if service.is_tracking() {
        return Err(WalletCode::TrackingMode.into());
    }
    if transfers.is_empty() {
        return Err(WalletCode::ZeroDestination.into());
    }
    if p.fee < rules::minimum_fee() {
        return Err(WalletCode::FeeTooSmall.into());
    }
    check_sources(service, &p.sources)?;
    let change = change_destination(service, &p.sources, &p.change_address, false)?;
    if transfers.iter().any(|t| t.amount >= i64::MAX as u64) {
        return Err(WalletCode::WrongAmount.into());
    }
    let mut needed: u64 = 0;
    for t in &transfers {
        if t.amount == 0 {
            return Err(WalletCode::ZeroDestination.into());
        }
        needed = needed
            .checked_add(t.amount)
            .ok_or(AppError::wallet(WalletCode::SumOverflow))?;
    }
    needed
        .checked_add(p.fee)
        .ok_or(AppError::wallet(WalletCode::SumOverflow))?;

    Ok(SendRequest {
        destinations: transfers
            .into_iter()
            .map(|t| (t.address, t.amount))
            .collect(),
        mixin: Some(mixin),
        fee: Some(p.fee),
        payment_id,
        source_addresses: p.sources.clone(),
        change_address: Some(change),
        unlock_time: p.unlock_time,
        extra,
    })
}

/// `WalletService::sendTransaction`.
pub async fn send_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = SendParams::read(params, default_mixin(service))?;
    let _ready = service.serialize().await;
    let request = prepare(service, &p, false)?;
    let hash = service.wallet().send_advanced(request).await?;
    tracing::debug!("Transaction {hash} has been sent");
    Ok(json!({ "transactionHash": hash.to_hex() }))
}

/// `WalletService::createDelayedTransaction`: built and signed, kept until sent or deleted.
pub async fn create_delayed_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = SendParams::read(params, default_mixin(service))?;
    let _ready = service.serialize().await;
    let request = prepare(service, &p, true)?;
    let hash = service.wallet().delayed_create(request).await?;
    tracing::debug!("Delayed transaction {hash} has been created");
    Ok(json!({ "transactionHash": hash.to_hex() }))
}

/// `WalletService::getDelayedTransactionHashes`.
pub async fn get_delayed_transaction_hashes<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    Params::new(params)?;
    let _ready = service.serialize().await;
    if service.is_tracking() {
        return Err(AppError::wallet(WalletCode::TrackingMode).into());
    }
    let hashes: Vec<String> = service
        .wallet()
        .delayed_list()
        .iter()
        .map(Hash::to_hex)
        .collect();
    Ok(json!({ "transactionHashes": hashes }))
}

/// The delayed transaction a request names: the hash must parse, and walletd's index of
/// transaction ids is keyed by the lower-case hex (`transactionIdIndex`). `Ok(Some)` for a
/// delayed transaction; `Err(not_delayed)` for another wallet transaction.
fn delayed_hash<T: Transport + 'static>(
    service: &Service<T>,
    text: &str,
    not_delayed: WalletCode,
) -> Result<Hash, AppError> {
    let hash = model::parse_hash(text)?;
    if hash.to_hex() != text {
        return Err(ServiceCode::ObjectNotFound.into());
    }
    if service.wallet().delayed_list().contains(&hash) {
        return Ok(hash);
    }
    if service.wallet().transaction(&hash).is_none() {
        return Err(ServiceCode::ObjectNotFound.into());
    }
    if service.is_tracking() {
        return Err(WalletCode::TrackingMode.into());
    }
    Err(not_delayed.into())
}

/// `WalletService::deleteDelayedTransaction` (`rollbackUncommitedTransaction`).
pub async fn delete_delayed_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let text = Params::new(params)?.required_string("transactionHash")?;
    let _ready = service.serialize().await;
    let hash = delayed_hash(service, &text, WalletCode::TxCancelImpossible)?;
    service.wallet().delayed_delete(&hash)?;
    tracing::debug!("Delayed transaction {text} has been canceled");
    Ok(json!({}))
}

/// `WalletService::sendDelayedTransaction` (`commitTransaction`).
pub async fn send_delayed_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let text = Params::new(params)?.required_string("transactionHash")?;
    let _ready = service.serialize().await;
    let hash = delayed_hash(service, &text, WalletCode::TxTransferImpossible)?;
    service.wallet().delayed_send(&hash).await?;
    tracing::debug!("Delayed transaction {text} has been sent");
    Ok(json!({}))
}

/// `WalletService::sendFusionTransaction` (`WalletGreen::createFusionTransaction`): outputs
/// below `threshold` of one power-of-ten group into a few larger ones, without fee.
pub async fn send_fusion_transaction<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let threshold = p.u64("threshold")?.ok_or(Invalid)?;
    let anonymity = p
        .u64("anonymity")?
        .unwrap_or_else(|| default_mixin(service));
    let sources = p.strings("addresses")?.unwrap_or_default();
    let destination = p.string("destinationAddress")?.unwrap_or_default();
    let _ready = service.serialize().await;
    model::validate_addresses(&sources)?;
    if !destination.is_empty() {
        model::validate_addresses(&[&destination])?;
    }
    if service.is_tracking() {
        return Err(AppError::wallet(WalletCode::TrackingMode).into());
    }
    check_sources(service, &sources)?;
    let destination = change_destination(service, &sources, &destination, true)?;
    let height = u32::try_from(service.network_height()).unwrap_or(u32::MAX);
    if threshold <= default_fusion_dust_threshold(height) {
        tracing::warn!("Fusion transaction threshold is too small: {threshold}");
        return Err(AppError::wallet(WalletCode::InternalWalletError).into());
    }
    // the service passes `uint32_t`, `createFusionTransaction` takes `uint16_t`
    let mixin = u64::from(anonymity as u16);
    let hash = service
        .wallet()
        .fusion_advanced(FusionRequest {
            mixin: Some(mixin),
            source_addresses: sources,
            destination: Some(destination),
            threshold: Some(threshold),
        })
        .await?;
    tracing::debug!("Fusion transaction {hash} has been sent");
    Ok(json!({ "transactionHash": hash.to_hex() }))
}

/// `WalletService::estimateFusion` (`WalletGreen::estimate`).
pub async fn estimate_fusion<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let threshold = p.u64("threshold")?.ok_or(Invalid)?;
    let sources = p.strings("addresses")?.unwrap_or_default();
    let _ready = service.serialize().await;
    model::validate_addresses(&sources)?;
    check_sources(service, &sources)?;
    let estimate = service.wallet().estimate_fusion(threshold, &sources)?;
    Ok(json!({
        "fusionReadyCount": estimate.fusion_ready_count as u32,
        "totalOutputCount": estimate.total_output_count as u32,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_parameters_follow_the_message_rules() {
        let base = json!({ "transfers": [{"address": "a", "amount": 1}], "fee": 10 });
        let p = SendParams::read(&base, 3).unwrap();
        assert_eq!((p.anonymity, p.fee, p.unlock_time), (3, 10, 0));
        let mut v = base.clone();
        v["anonymity"] = json!(0);
        assert_eq!(SendParams::read(&v, 3).unwrap().anonymity, 0);
        let mut v = base.clone();
        v["extra"] = json!("00");
        v["paymentId"] = json!("ab".repeat(32));
        assert_eq!(SendParams::read(&v, 3), Err(Invalid));
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("fee");
        assert_eq!(SendParams::read(&v, 3), Err(Invalid));
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("transfers");
        assert_eq!(SendParams::read(&v, 3), Err(Invalid));
        let mut v = base;
        v["transfers"] = json!([{"amount": 1}]);
        assert_eq!(SendParams::read(&v, 3), Err(Invalid));
    }
}
