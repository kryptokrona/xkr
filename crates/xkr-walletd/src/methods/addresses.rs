//! Address methods: `createAddress`, `createAddressList`, `deleteAddress`, `getSpendKeys`,
//! `getBalance` (`src/wallet_service/wallet_service.cpp` and `src/wallet/wallet_green.cpp`
//! `WalletGreen::createAddress`, `createAddressList`, `addWallet`, `deleteAddress`,
//! `getAddressSpendKey`, `getActualBalance`, `getPendingBalance`).
//!
//! A container is either spending (every address has a secret key) or tracking (none has);
//! adding the other kind is `WRONG_PARAMETERS`. A container whose addresses were all deleted
//! takes either kind. An address created as new scans from the next block; an imported one
//! from `scanHeight`, rescanning the wallet when it is below what the wallet has processed.

use std::collections::HashSet;

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_types::{PublicKey, SecretKey};

use super::MethodResult;
use crate::errors::{AppError, ServiceCode, WalletCode};
use crate::model::{self, uint};
use crate::params::{Invalid, Params};
use crate::service::Service;

/// `crypto::secret_key_to_public_key`, `KEY_GENERATION_ERROR` if the key is not reduced.
fn public_of(secret: &SecretKey) -> Result<PublicKey, AppError> {
    xkr_crypto::secret_key_to_public_key(secret)
        .ok_or_else(|| AppError::wallet(WalletCode::KeyGenerationError))
}

/// `WalletGreen::addWallet`'s checks before the engine adds the address: the tracking mode of
/// the container (`NONE` without addresses accepts either) and an existing address.
fn check_new<T: Transport + 'static>(
    service: &Service<T>,
    public: &PublicKey,
    tracking: bool,
) -> Result<(), AppError> {
    if service.has_addresses() && service.is_tracking() != tracking {
        return Err(WalletCode::WrongParameters.into());
    }
    if service.own_addresses().iter().any(|(k, _)| k == public) {
        return Err(WalletCode::AddressAlreadyExists.into());
    }
    Ok(())
}

/// The scan height of an address: the next block for a new one, `scan_height` otherwise.
fn start_height<T: Transport + 'static>(
    service: &Service<T>,
    new_address: bool,
    scan_height: u64,
) -> u64 {
    if new_address {
        service.block_count()
    } else {
        scan_height
    }
}

/// `newAddress` and `scanHeight` may not both be given (`CreateAddress::Request::serialize`).
fn new_address_and_height(p: Params<'_>) -> Result<(bool, u64), Invalid> {
    let new_address = p.bool("newAddress")?;
    let scan_height = p.u64("scanHeight")?;
    if new_address.is_some() && scan_height.is_some() {
        return Err(Invalid);
    }
    Ok((new_address.unwrap_or(false), scan_height.unwrap_or(0)))
}

/// `WalletService::createAddress` / `createTrackingAddress`: a random address, an address from
/// `spendSecretKey`, or a tracking address from `spendPublicKey` (not both).
pub async fn create_address<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let secret = p.string("spendSecretKey")?;
    let public = p.string("spendPublicKey")?;
    let (new_address, scan_height) = new_address_and_height(p)?;
    if secret.is_some() && public.is_some() {
        return Err(Invalid.into());
    }
    let secret = secret.unwrap_or_default();
    let public = public.unwrap_or_default();
    let _ready = service.serialize().await;
    let wallet = service.wallet();
    let address = if secret.is_empty() && public.is_empty() {
        // `WalletGreen::createAddress()`: a random key, scanning from now
        if service.is_tracking() {
            return Err(AppError::wallet(WalletCode::WrongParameters).into());
        }
        wallet.add_subwallet().await?.0
    } else if secret.is_empty() {
        let key = model::parse_public_key(&public)?;
        if !xkr_crypto::check_key(&key) {
            return Err(AppError::wallet(WalletCode::WrongParameters).into());
        }
        check_new(service, &key, true)?;
        // `createTrackingAddress` always creates the address as new
        wallet
            .import_view_subwallet(key, start_height(service, true, scan_height))
            .await?
    } else {
        let key = model::parse_secret_key(&secret)?;
        let public = public_of(&key)?;
        check_new(service, &public, false)?;
        wallet
            .import_subwallet(key, start_height(service, new_address, scan_height))
            .await?
    };
    tracing::debug!("Created address {address}");
    Ok(json!({ "address": address }))
}

/// `WalletService::createAddressList`: one address per secret key, in order, imported together
/// with one rescan for the whole list (`WalletGreen::doCreateAddressList`). A repeated key text
/// is `DUPLICATE_KEY`, a malformed one `WRONG_KEY_FORMAT`, a key that is not reduced
/// `KEY_GENERATION_ERROR`, an address that is already there `ADDRESS_ALREADY_EXISTS`; every
/// key is checked first and nothing is added then.
pub async fn create_address_list<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let p = Params::new(params)?;
    let texts = p.strings("spendSecretKeys")?.ok_or(Invalid)?;
    let (new_address, scan_height) = new_address_and_height(p)?;
    let _ready = service.serialize().await;
    let mut unique: HashSet<&str> = HashSet::with_capacity(texts.len());
    let mut keys = Vec::with_capacity(texts.len());
    for text in &texts {
        if !unique.insert(text.as_str()) {
            return Err(AppError::service(ServiceCode::DuplicateKey).into());
        }
        keys.push(model::parse_secret_key(text)?);
    }
    let publics = keys.iter().map(public_of).collect::<Result<Vec<_>, _>>()?;
    for (i, public) in publics.iter().enumerate() {
        check_new(service, public, false)?;
        if publics[..i].contains(public) {
            return Err(AppError::wallet(WalletCode::AddressAlreadyExists).into());
        }
    }
    let height = start_height(service, new_address, scan_height);
    let addresses = service.wallet().import_subwallets(&keys, height).await?;
    tracing::debug!("Created {} addresses", addresses.len());
    Ok(json!({ "addresses": addresses }))
}

/// The own address `address` names: `BAD_ADDRESS` if invalid, `missing` if not in the wallet.
fn own_address<T: Transport + 'static>(
    service: &Service<T>,
    address: &str,
    missing: WalletCode,
) -> Result<(PublicKey, String), AppError> {
    let key = service.own_key(address)?.ok_or(AppError::wallet(missing))?;
    let text = service
        .own_address_text(&key)
        .ok_or(AppError::wallet(missing))?;
    Ok((key, text))
}

/// `WalletService::deleteAddress` (`WalletGreen::deleteAddress`): any address, also the first
/// or the only one.
pub async fn delete_address<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let address = Params::new(params)?.required_string("address")?;
    let _ready = service.serialize().await;
    let (_, text) = own_address(service, &address, WalletCode::ObjectNotFound)?;
    service.wallet().delete_address(&text).await?;
    tracing::debug!("Address {address} successfully deleted");
    Ok(json!({}))
}

/// `WalletService::getSpendkeys`: the public and secret spend key (zeros for a tracking
/// address).
pub async fn get_spend_keys<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let address = Params::new(params)?.required_string("address")?;
    let _ready = service.serialize().await;
    let (key, text) = own_address(service, &address, WalletCode::ObjectNotFound)?;
    let secret = if service.is_tracking() {
        SecretKey::ZERO
    } else {
        service.wallet().spend_keys(&text)?.1
    };
    Ok(json!({ "spendPublicKey": key.to_hex(), "spendSecretKey": secret.to_hex() }))
}

/// `WalletService::getBalance`: of one address (`WALLET_NOT_FOUND` if not in the wallet) or
/// of all (`address` missing or empty).
pub async fn get_balance<T: Transport + 'static>(
    service: &Service<T>,
    params: &Value,
) -> MethodResult {
    let address = Params::new(params)?.string("address")?.unwrap_or_default();
    let _ready = service.serialize().await;
    let balance = if address.is_empty() {
        service.wallet().balance(None)?
    } else {
        let (_, text) = own_address(service, &address, WalletCode::WalletNotFound)?;
        service.wallet().balance(Some(&text))?
    };
    Ok(json!({
        "availableBalance": uint(balance.unlocked),
        "lockedAmount": uint(balance.locked),
    }))
}
