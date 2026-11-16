//! xkr-wallet-files: the wallet files of the Kryptokrona C++ and JavaScript wallets, read and
//! written byte-compatibly, so users keep their files and passwords.
//!
//! | Module | Format | C++ / JS source |
//! |---|---|---|
//! | [`backend`] | `WalletBackend` file (wallet-api, zedwallet++ / xkrwallet-beta, wallet-backend-js, Hugin): AES-encrypted JSON | `src/wallet_backend/wallet_backend.cpp` `openWallet` / `unsafeSave` / `toJSON`; `lib/WalletEncryption.ts`, `lib/JsonSerialization.ts` |
//! | [`green`] | `WalletGreen` container (walletd / kryptokrona-service, zedwallet / xkrwallet): chacha8-encrypted records and cache | `src/wallet/wallet_green.cpp`, `wallet_serialization_v2.cpp`, `src/common/file_mapped_vector.h`, `src/crypto/chacha8.cpp` |
//! | [`address_book`] | `.addressBook.json` of zedwallet++ and zedwallet | `src/zedwallet++/address_book.cpp`, `src/zedwallet/address_book.cpp` |
//! | [`detect`] | which of the above a file is | |
//! | [`summary`] | public facts about an opened file, for logs and checks (no private keys) | |
//! | [`FileError`] | errors with the C++ codes and texts | `src/errors/errors.h`, `src/wallet/wallet_errors.h` |
//!
//! Everything works on byte slices; the `*_file` helpers read files and write them through
//! [`atomic_write`] (temporary file, `fsync`, rename). Random values (salts, IVs) come from the
//! caller's RNG, `xkr_crypto::os_rng()` in production.
#![forbid(unsafe_code)]

pub mod address_book;
mod atomic;
pub mod backend;
pub mod detect;
mod error;
pub mod green;
pub mod summary;

pub use address_book::{AddressBook, AddressBookEntry, AddressBookForm};
#[cfg(feature = "fault-injection")]
pub use atomic::fault;
pub use atomic::{atomic_write, remove_leftover_temps};
pub use backend::WalletBackendJson;
pub use detect::{FileKind, detect};
pub use error::FileError;
pub use green::{GreenRecord, GreenViewKeys, GreenWallet};
pub use summary::{AddressSummary, WalletSummary, summarize, summarize_file};
