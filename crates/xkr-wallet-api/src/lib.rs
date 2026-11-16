//! xkr-wallet-api — a drop-in for the C++ `wallet-api` REST service (`src/wallet_api/*` in the
//! C++ repository) on the [`xkr_wallet`] engine.
//!
//! The binary takes the C++ flags with the same defaults ([`cli`]), serves the same routes with
//! the same path patterns, status codes, headers and JSON bodies ([`server`], [`handlers`],
//! [`respond`], [`json`]), checks `X-API-KEY` ([`auth`]), answers `OPTIONS` and adds the CORS
//! header ([`cors`]), and reads `exit` / `quit` from standard input to save and stop
//! ([`console`]).
//!
//! | module | C++ |
//! |---|---|
//! | [`cli`] | `parse_arguments.cpp` `parseArguments` |
//! | [`server`] | `api_dispatcher.cpp` `ApiDispatcher::ApiDispatcher`, `middleware`; `constants.h` |
//! | [`auth`] | `ApiDispatcher::checkAuthenticated` |
//! | [`cors`] | `ApiDispatcher::handleOptions` |
//! | [`handlers`] | the route functions of `ApiDispatcher` |
//! | [`params`] | `tryGetJsonValue`, `getDefaultWalletParams` |
//! | [`respond`] | `json::dump(4) + "\n"`, `{errorCode, errorMessage}` |
//! | [`json`] | `to_json(Transaction)`, `publicKeysToAddresses` |
//! | [`console`] | `wallet_api.cpp` `main` |
//!
//! One flag has no C++ counterpart: `--network mainnet|testnet` picks the network rules and the
//! default daemon port (11898 or 11899), which the C++ tools fix at build time.
#![forbid(unsafe_code)]

pub mod auth;
pub mod cli;
pub mod console;
pub mod cors;
pub mod handlers;
pub mod json;
pub mod params;
pub mod respond;
pub mod server;

pub use server::{ApiConfig, App, HttpNodes, NodeFactory, router};
