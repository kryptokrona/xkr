//! Keys (`ApiDispatcher::getPrivateViewKey`, `getSpendKeys`, `getMnemonicSeed`).

use serde_json::json;
use xkr_daemon_client::Transport;
use xkr_wallet::Wallet;
use xkr_wallet::validate::validate_address;

use super::{Answer, plain_not_in_wallet};
use crate::respond::Reply;

/// GET `/keys`: `{privateViewKey}`.
#[must_use]
pub fn view_key<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    Reply::json(200, &json!({ "privateViewKey": wallet.keys().to_hex() }))
}

/// GET `/keys/{address}`: `{publicSpendKey, privateSpendKey}`.
pub fn spend_keys<T: Transport + 'static>(wallet: &Wallet<T>, address: &str) -> Answer {
    validate_address(address, false)?;
    let (public, secret) = wallet.spend_keys(address).map_err(plain_not_in_wallet)?;
    Ok(Reply::json(
        200,
        &json!({ "publicSpendKey": public.to_hex(), "privateSpendKey": secret.to_hex() }),
    ))
}

/// GET `/keys/mnemonic/{address}`: `{mnemonicSeed}`, or `KEYS_NOT_DETERMINISTIC` when the
/// address's spend key does not give the wallet's view key.
pub fn mnemonic<T: Transport + 'static>(wallet: &Wallet<T>, address: &str) -> Answer {
    validate_address(address, false)?;
    let seed = wallet.mnemonic(address).map_err(plain_not_in_wallet)?;
    Ok(Reply::json(200, &json!({ "mnemonicSeed": seed })))
}
