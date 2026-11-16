//! Addresses (`ApiDispatcher::createAddress`, `importAddress`, `importViewAddress`,
//! `deleteAddress`, `getAddresses`, `getPrimaryAddress`, `createIntegratedAddress`).

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_types::{PublicKey, SecretKey};
use xkr_wallet::Wallet;
use xkr_wallet::validate::{create_integrated_address, validate_address};

use super::{Answer, plain_not_in_wallet};
use crate::params;
use crate::respond::Reply;

/// POST `/addresses/create`: a subwallet with a random spend key; 201
/// `{address, privateSpendKey}`.
pub async fn create<T: Transport + 'static>(wallet: &Wallet<T>) -> Answer {
    let (address, secret) = wallet.add_subwallet().await?;
    Ok(Reply::json(
        201,
        &json!({ "address": address, "privateSpendKey": secret.to_hex() }),
    ))
}

/// POST `/addresses/import`: `privateSpendKey`, `scanHeight` (default 0); 201 `{address}`.
pub async fn import<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let scan_height = params::opt_u64(body, "scanHeight")?.unwrap_or(0);
    let secret: SecretKey = params::key(body, "privateSpendKey")?;
    let address = wallet.import_subwallet(secret, scan_height).await?;
    Ok(Reply::json(201, &json!({ "address": address })))
}

/// POST `/addresses/import/view`: `publicSpendKey`, `scanHeight` (default 0); 201 `{address}`.
pub async fn import_view<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let scan_height = params::opt_u64(body, "scanHeight")?.unwrap_or(0);
    let public: PublicKey = params::key(body, "publicSpendKey")?;
    let address = wallet.import_view_subwallet(public, scan_height).await?;
    Ok(Reply::json(201, &json!({ "address": address })))
}

/// DELETE `/addresses/{address}`: 200, or `ADDRESS_NOT_IN_WALLET` /
/// `CANNOT_DELETE_PRIMARY_ADDRESS`.
pub async fn delete<T: Transport + 'static>(wallet: &Wallet<T>, address: &str) -> Answer {
    validate_address(address, false)?;
    wallet
        .delete_subwallet(address)
        .await
        .map_err(plain_not_in_wallet)?;
    Ok(Reply::empty(200))
}

/// GET `/addresses`: `{addresses: [...]}`.
#[must_use]
pub fn list<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    Reply::json(200, &json!({ "addresses": wallet.addresses() }))
}

/// GET `/addresses/primary`: `{address}`.
#[must_use]
pub fn primary<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    Reply::json(200, &json!({ "address": wallet.primary_address() }))
}

/// GET `/addresses/{address}/{paymentId}`: `{integratedAddress}` (`SEKR` prefix whatever the
/// prefix of the address given).
pub fn integrated(address: &str, payment_id: &str) -> Answer {
    let integrated = create_integrated_address(address, payment_id)?;
    Ok(Reply::json(
        200,
        &json!({ "integratedAddress": integrated }),
    ))
}
