//! xkr-crypto-ffi: one implementation of the crypto primitives the Kryptokrona JavaScript stack
//! calls (kryptokrona-crypto's `Crypto` API and the Hugin mobile app's native module), built on
//! the crates the node itself runs (`xkr-crypto`, `xkr-wallet-core`).
//!
//! It replaces the two C++ copies inside the Hugin app (`ios/crypto`, `android/.../jni`) with
//! the same code on iOS and Android, adds the transaction-version-2 primitives (view tags,
//! CLSAG) and backs the Node-API addon of kryptokrona-crypto 2.x. Build and compatibility
//! checks: `crates/xkr-crypto-ffi/README.md`.
//!
//! | module | what |
//! |---|---|
//! | [`api`] | the primitives, hex strings in and out (`checkKey` .. `clsagVerify`, plus the rest of kryptokrona-crypto's `Crypto` methods the Node addon needs) |
//! | [`scan`] | `processBlockOutputs` (wallet output scan, JSON in and out) |
//! | [`pow`] | `findPowShare` (Hugin message share search) |
//! | [`ffi`] | the C ABI of `include/xkr_crypto_ffi.h` for the React Native C++ `TurboModule` |
//! | [`error`] | error type and C status codes |
//!
//! The napi-rs Node addon calls [`api`], [`scan`] and [`pow`] directly (crate type `rlib`); the
//! C ABI exists for C++ callers only (crate type `staticlib`).

pub mod api;
pub mod error;
pub mod ffi;
pub mod pow;
pub mod scan;

pub use error::{CryptoError, CryptoResult};
