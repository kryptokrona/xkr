//! xkr-walletd — a drop-in for the C++ `kryptokrona-service` (walletd) on the xkr-wallet engine.
//!
//! Same command line and config file, the same JSON-RPC on `POST /json_rpc` (envelope, ids,
//! error codes, method parameters and result shapes), and the same walletd container
//! (format B), with the engine's own state in `<container>.xkrstate`.
//!
//! | module | C++ |
//! |---|---|
//! | [`cli`] | `src/wallet_service/main.cpp`, `payment_gate_service.cpp` |
//! | [`config`] | `wallet_service_configuration.cpp`, `configuration_manager.cpp` |
//! | [`container`] | `wallet_service.cpp` `generateNewWallet` |
//! | [`rpc`] | `src/json_rpc_server/json_rpc_server.cpp`, `payment_service_json_rpc_server.cpp` |
//! | [`methods`] | `wallet_service.cpp` `WalletService`, `src/wallet/wallet_green.cpp` |
//! | [`params`] | `payment_service_json_rpc_messages.cpp`, `json_input_value_serializer.cpp` |
//! | [`model`] | `TransactionRpcInfo`, `WalletGreen::updateTransactionTransfers` |
//! | [`errors`] | `wallet_errors.h`, `wallet_service_error_category.h`, `node_errors.h` |
//! | [`service`] | `WalletService` state |
#![forbid(unsafe_code)]

pub mod cli;
pub mod config;
pub mod container;
pub mod errors;
pub mod methods;
pub mod model;
pub mod params;
pub mod rpc;
pub mod service;

pub use rpc::router;
pub use service::{RpcSettings, Service};
