//! The 27 walletd methods (`src/wallet_service/payment_service_json_rpc_server.cpp`
//! `PaymentServiceJsonRpcServer` handler table; `getNodeFeeInfo` is a second name of
//! `getFeeInfo`).
//!
//! Each method first reads its parameters as the C++ message's `serialize` does
//! (`payment_service_json_rpc_messages.cpp`; failures are `-32600`), then runs alone
//! (`WalletService` takes `readyEvent` for every call) with the checks of `WalletService` and
//! `WalletGreen` in their C++ order.

pub mod addresses;
pub mod send;
pub mod transactions;
pub mod wallet;

use serde_json::Value;
use xkr_daemon_client::Transport;

use crate::rpc::Failure;
use crate::service::Service;

/// What a method answers.
pub type MethodResult = Result<Value, Failure>;

/// The method names, as registered in `PaymentServiceJsonRpcServer`.
pub const METHODS: [&str; 28] = [
    "save",
    "export",
    "reset",
    "createAddress",
    "createAddressList",
    "deleteAddress",
    "getSpendKeys",
    "getBalance",
    "getBlockHashes",
    "getTransactionHashes",
    "getTransactions",
    "getUnconfirmedTransactionHashes",
    "getTransaction",
    "sendTransaction",
    "createDelayedTransaction",
    "getDelayedTransactionHashes",
    "deleteDelayedTransaction",
    "sendDelayedTransaction",
    "getViewKey",
    "getMnemonicSeed",
    "getStatus",
    "getAddresses",
    "sendFusionTransaction",
    "estimateFusion",
    "createIntegratedAddress",
    "getFeeInfo",
    "getNodeFeeInfo",
    "validateAddress",
];

/// Whether `name` is a walletd method.
#[must_use]
pub fn is_method(name: &str) -> bool {
    METHODS.contains(&name)
}

/// Runs method `name` (one of [`METHODS`]) with `params`.
pub async fn call<T: Transport + 'static>(
    service: &Service<T>,
    name: &str,
    params: &Value,
) -> MethodResult {
    match name {
        "save" => wallet::save(service, params).await,
        "export" => wallet::export(service, params).await,
        "reset" => wallet::reset(service, params).await,
        "getViewKey" => wallet::get_view_key(service, params).await,
        "getMnemonicSeed" => wallet::get_mnemonic_seed(service, params).await,
        "getStatus" => wallet::get_status(service, params).await,
        "getAddresses" => wallet::get_addresses(service, params).await,
        "getFeeInfo" | "getNodeFeeInfo" => wallet::get_fee_info(service, params).await,
        "validateAddress" => wallet::validate_address(service, params).await,
        "createIntegratedAddress" => wallet::create_integrated_address(service, params).await,
        "createAddress" => addresses::create_address(service, params).await,
        "createAddressList" => addresses::create_address_list(service, params).await,
        "deleteAddress" => addresses::delete_address(service, params).await,
        "getSpendKeys" => addresses::get_spend_keys(service, params).await,
        "getBalance" => addresses::get_balance(service, params).await,
        "getBlockHashes" => transactions::get_block_hashes(service, params).await,
        "getTransactionHashes" => transactions::get_transaction_hashes(service, params).await,
        "getTransactions" => transactions::get_transactions(service, params).await,
        "getUnconfirmedTransactionHashes" => {
            transactions::get_unconfirmed_transaction_hashes(service, params).await
        }
        "getTransaction" => transactions::get_transaction(service, params).await,
        "sendTransaction" => send::send_transaction(service, params).await,
        "createDelayedTransaction" => send::create_delayed_transaction(service, params).await,
        "getDelayedTransactionHashes" => {
            send::get_delayed_transaction_hashes(service, params).await
        }
        "deleteDelayedTransaction" => send::delete_delayed_transaction(service, params).await,
        "sendDelayedTransaction" => send::send_delayed_transaction(service, params).await,
        "sendFusionTransaction" => send::send_fusion_transaction(service, params).await,
        "estimateFusion" => send::estimate_fusion(service, params).await,
        _ => Err(Failure::InvalidRequest),
    }
}
