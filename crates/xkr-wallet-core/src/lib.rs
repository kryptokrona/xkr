//! xkr-wallet-core — the wallet half of Kryptokrona, as a small library.
//!
//! Meant to be shared by `xkrd` (block templates, the test wallet) and later by Hugin
//! clients, so it depends only on the core crates (`xkr-types`, `xkr-crypto`,
//! `xkr-serialization`, `xkr-consensus`) and never on storage, networking or async code.
//!
//! | module | C++ / JS source |
//! |---|---|
//! | [`base58`] | `src/common/base58.cpp` (CryptoNote block base58, `encode_addr` / `decode_addr`) |
//! | [`address`] | `cryptonote_basic_impl.cpp` `parseAccountAddressString`, `Currency::parseAccountAddressString` (SEKR + Xkr prefixes), integrated addresses of wallet-backend-js |
//! | [`keys`] | `AccountBase::generate`, `generateViewFromSpend`, deterministic subwallet keys of kryptokrona-crypto |
//! | [`mnemonic`] | `src/mnemonics/mnemonics.cpp`: 25-word seeds of the private spend key |
//! | [`payment_id`] | `validatePaymentID` (`src/errors/validate_parameters.cpp`) |
//! | [`scan`] | wallet-backend-js `WalletSynchronizer` output scanning: derivation, ownership, key images; subwallets and view-only wallets (`WalletSynchronizer::processTransactionOutputs`) |
//! | [`extra`] | `transaction_extra.cpp` tx public key / payment id, kryptokrona-utils nonce data (Hugin) |
//! | [`transfer`] | wallet-backend-js `Transfer.ts`: denominations, ring construction, `generate_ring_signature`; version 2 with view tags and CLSAG (`docs/tx-v2.md`) |
//! | [`rules`] | fee, mixin, transaction version, unlock time and size rules of the C++ wallets (`utilities.cpp`, `validate_parameters.cpp`, `mixins.h`) |
//! | [`select`] | input selection and fusion (`SubWallets::getTransactionInputsForAmount`, `getFusionTransactionInputs`, walletd `pickRandomFusionInputs`) |
//!
//! Everything here is pure (no I/O, no clock: callers pass the height and the time). [`rules`]
//! and [`select`] hold the policy of the C++ wallets; fetching decoys and talking to a daemon is
//! left to the caller (see `crates/xkrd/src/bin/xkr-testwallet.rs` for a minimal user).
#![forbid(unsafe_code)]

pub mod address;
pub mod base58;
pub mod extra;
pub mod keys;
pub mod mnemonic;
pub mod payment_id;
pub mod rules;
pub mod scan;
pub mod select;
pub mod transfer;

pub use address::{ADDRESS_PREFIX, ADDRESS_PREFIX_ALT, Address, AddressError};
pub use keys::{WalletKeys, deterministic_subwallet_keys, is_deterministic, view_key_from_spend};
pub use mnemonic::{MnemonicError, mnemonic_from_secret, secret_from_mnemonic};
pub use payment_id::{PaymentIdError, parse_payment_id};
pub use rules::PolicyError;
pub use scan::{
    FoundOutput, OwnedOutput, ScanStats, SpendKeys, scan_for_subwallets, scan_outputs,
    scan_subwallet_output, scan_tagged_outputs, scan_tagged_outputs_with_stats, scan_transaction,
};
pub use select::{SelectError, SpendableInput};
pub use transfer::{
    BuiltTransaction, Destination, InputSource, TransferError, TransferOptions, TxVersion,
    build_fusion_transaction, build_transaction,
};

/// `CRYPTONOTE_DISPLAY_DECIMAL_POINT`: atomic units per XKR = 10^5.
pub const ATOMIC_UNITS_PER_COIN: u64 = 100_000;

/// Formats atomic units as `XKR` with five decimals (C++ `Currency::formatAmount`).
#[must_use]
pub fn format_amount(amount: u64) -> String {
    format!(
        "{}.{:05}",
        amount / ATOMIC_UNITS_PER_COIN,
        amount % ATOMIC_UNITS_PER_COIN
    )
}

/// Parses `"12.345"` / `"12"` (XKR, at most five decimals) into atomic units.
#[must_use]
pub fn parse_amount(text: &str) -> Option<u64> {
    let text = text.trim();
    let (whole, frac) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text, ""),
    };
    if frac.len() > 5 || (whole.is_empty() && frac.is_empty()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut frac_units: u64 = if frac.is_empty() {
        0
    } else {
        frac.parse().ok()?
    };
    for _ in frac.len()..5 {
        frac_units *= 10;
    }
    whole
        .checked_mul(ATOMIC_UNITS_PER_COIN)?
        .checked_add(frac_units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(format_amount(1_234_567), "12.34567");
        assert_eq!(format_amount(5), "0.00005");
        assert_eq!(parse_amount("12.34567"), Some(1_234_567));
        assert_eq!(parse_amount("12.3"), Some(1_230_000));
        assert_eq!(parse_amount("7"), Some(700_000));
        assert_eq!(parse_amount(".5"), Some(50_000));
        assert_eq!(parse_amount("1.000001"), None);
        assert_eq!(parse_amount("x"), None);
    }
}
