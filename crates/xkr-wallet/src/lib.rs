//! xkr-wallet — the wallet engine behind xkr-wallet-api, xkr-walletd and the terminal wallet.
//!
//! It plays the part of the C++ `WalletBackend` (`src/wallet_backend/*`, `src/sub_wallets/*`)
//! and of the parts of walletd's `WalletGreen` that the walletd RPC needs, on top of
//! `xkr-wallet-core` (keys, scanning, rules, selection, transaction building),
//! `xkr-wallet-files` (the two C++ wallet formats) and `xkr-daemon-client` (the node).
//!
//! | module | C++ / JS |
//! |---|---|
//! | [`wallet`] | `WalletBackend`: create, open, import, save, reset, swap node, subwallets, balances, transactions, keys, status, sends |
//! | [`state`] | `SubWallets`, `SubWallet`, `wallet_types::Transaction` |
//! | [`sync`] | `WalletSynchronizer`, `SynchronizationStatus`; fetch/process tasks as wallet-backend-js |
//! | [`send`] | `transfer.cpp` (`send_transaction::*`), `validate_parameters.cpp`; walletd fusion with a threshold and delayed transactions |
//! | [`autosave`] | saving: write order, crash safety, the wallet saving by itself while open |
//! | [`persist`] | `WalletBackend::openWallet` / `unsafeSave` (format A); walletd containers with the engine state in `<container>.xkrstate` (format B) |
//! | [`events`] | `EventHandler` |
//! | [`error`] | `src/errors/errors.h` / `errors.cpp` |
//! | [`validate`] | `src/errors/validate_parameters.cpp` address, hash and payment id checks |
//!
//! Transactions are version 1 or 2 as the daemon's `/info` `tx_versions` allows; no fork
//! height is compiled in. Random values come from `xkr_crypto::os_rng()`.
#![forbid(unsafe_code)]

pub mod autosave;
pub mod error;
pub mod events;
pub mod persist;
pub mod send;
pub mod state;
pub mod sync;
pub mod validate;
pub mod wallet;

pub use autosave::AutosaveConfig;
pub use error::{ErrorCode, WalletError};
pub use events::WalletEvent;
pub use persist::FileFormat;
pub use persist::green::sidecar_path;
pub use send::SendRequest;
pub use send::fusion::FusionRequest;
pub use state::{
    AddressBalance, Balance, OwnedInput, SubWallet, Transaction, TransactionInfo,
    TransferDestination, TransferKind, TxState, UnconfirmedInput, WalletState,
};
pub use sync::status::SyncStatus;
pub use sync::{DaemonStatus, SyncConfig};
pub use validate::{create_integrated_address, validate_address, validate_hash};
pub use wallet::{
    LOCAL_PROBE, Node, NodeInfo, SWARM_HOST, Wallet, WalletConfig, WalletStatus, local_node_answers,
};
pub use xkr_wallet_core::select::FusionEstimate;
