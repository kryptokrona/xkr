//! A printable summary of a wallet file with public data only: format, public keys,
//! addresses, whether spend keys are present and consistent, counts and sync positions.
//! No private key, seed or `extra` content is ever part of it, so it can be shown in logs and
//! bug reports.

use core::fmt;
use std::path::Path;

use xkr_types::PublicKey;

use crate::FileError;
use crate::backend::{self, WalletBackendJson};
use crate::detect::{FileKind, detect};
use crate::green::{self, GreenWallet};

/// One address of a [`WalletSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressSummary {
    /// The address as stored (`WalletBackend` files only; containers store keys only).
    pub address: Option<String>,
    /// Public spend key.
    pub spend_public_key: PublicKey,
    /// True if the file holds a private spend key for this address.
    pub has_spend_secret: bool,
    /// `Some(true)` if the private spend key gives the public spend key, `Some(false)` if not,
    /// `None` without a private spend key.
    pub spend_key_matches: Option<bool>,
    /// Creation (container) or sync start (`WalletBackend`) timestamp.
    pub start_timestamp: u64,
    /// Sync start height (`WalletBackend` only).
    pub start_height: Option<u64>,
    /// Number and sum of unspent outputs (`WalletBackend` only).
    pub unspent: Option<(usize, u64)>,
}

/// Public facts about an opened wallet file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletSummary {
    /// The detected format.
    pub kind: FileKind,
    /// Public view key, derived from the private view key.
    pub view_public_key: Option<PublicKey>,
    /// True if the private view key is a valid scalar whose public key is the stored one
    /// (containers) or derivable at all (`WalletBackend`).
    pub view_key_valid: bool,
    /// `WalletBackend`: the view key in `walletSynchronizer` equals the one in `subWallets`.
    pub view_keys_agree: Option<bool>,
    /// True for a view-only (tracking) wallet.
    pub view_only: bool,
    /// The addresses.
    pub addresses: Vec<AddressSummary>,
    /// Confirmed (or cached) transactions.
    pub transaction_count: usize,
    /// `WalletBackend`: unconfirmed sent transactions.
    pub locked_transaction_count: Option<usize>,
    /// Container: number of cached transfers.
    pub transfer_count: Option<usize>,
    /// `WalletBackend`: last synced block height.
    pub sync_height: Option<u64>,
    /// Container cache: save level, or why it was not readable.
    pub cache_note: Option<String>,
    /// Names (not values) of JSON fields kept but not modelled, by object path.
    pub unknown_fields: Vec<String>,
}

/// Opens `bytes` with `password` in whatever format [`detect`] finds and summarises it.
///
/// Errors: [`FileError::NotAWalletFile`] for an unknown format, [`FileError::WrongVersion`]
/// for a container of another version, otherwise the error of the format's `open`.
pub fn summarize(bytes: &[u8], password: &str) -> Result<WalletSummary, FileError> {
    let kind = detect(bytes);
    match kind {
        FileKind::WalletBackend => Ok(from_backend(kind, &backend::open(bytes, password)?)),
        FileKind::WalletBackendJsString => {
            let text = std::str::from_utf8(bytes).map_err(|_| FileError::NotAWalletFile)?;
            let file = backend::decode_js_buffer_string(text)?;
            Ok(from_backend(kind, &backend::open(&file, password)?))
        }
        FileKind::WalletGreen => Ok(from_green(kind, &green::open(bytes, password)?)),
        FileKind::WalletGreenOtherVersion(_) => Err(FileError::WrongVersion),
        FileKind::Unknown => Err(FileError::NotAWalletFile),
    }
}

/// [`summarize`] for a file on disk; a missing file is [`FileError::FilenameNonExistent`].
pub fn summarize_file(path: &Path, password: &str) -> Result<WalletSummary, FileError> {
    let bytes = std::fs::read(path).map_err(FileError::from_open)?;
    summarize(&bytes, password)
}

fn from_backend(kind: FileKind, w: &WalletBackendJson) -> WalletSummary {
    let subs = &w.sub_wallets;
    let view_public_key = xkr_crypto::secret_key_to_public_key(&subs.private_view_key);
    let addresses = subs
        .sub_wallet
        .iter()
        .map(|s| {
            let has_secret = !s.private_spend_key.is_zero();
            AddressSummary {
                address: Some(s.address.clone()),
                spend_public_key: s.public_spend_key,
                has_spend_secret: has_secret,
                spend_key_matches: has_secret.then(|| {
                    xkr_crypto::secret_key_to_public_key(&s.private_spend_key)
                        == Some(s.public_spend_key)
                }),
                start_timestamp: s.sync_start_timestamp,
                start_height: Some(s.sync_start_height),
                unspent: Some((
                    s.unspent_inputs.len(),
                    s.unspent_inputs
                        .iter()
                        .fold(0u64, |a, i| a.saturating_add(i.amount)),
                )),
            }
        })
        .collect();

    let mut unknown_fields = Vec::new();
    let mut note = |path: &str, map: &backend::UnknownFields| {
        for k in map.keys() {
            let entry = format!("{path}.{k}");
            if !unknown_fields.contains(&entry) {
                unknown_fields.push(entry);
            }
        }
    };
    note("", &w.other);
    note("subWallets", &subs.other);
    note("walletSynchronizer", &w.wallet_synchronizer.other);
    for s in &subs.sub_wallet {
        note("subWallet[]", &s.other);
        for i in s
            .unspent_inputs
            .iter()
            .chain(&s.locked_inputs)
            .chain(&s.spent_inputs)
        {
            note("subWallet[].input", &i.other);
        }
        for u in &s.unconfirmed_incoming_amounts {
            note("subWallet[].unconfirmedIncomingAmounts[]", &u.other);
        }
    }
    for t in subs.transactions.iter().chain(&subs.locked_transactions) {
        note("transactions[]", &t.other);
    }

    WalletSummary {
        kind,
        view_public_key,
        view_key_valid: view_public_key.is_some(),
        view_keys_agree: Some(w.wallet_synchronizer.private_view_key == subs.private_view_key),
        view_only: subs.is_view_wallet,
        addresses,
        transaction_count: subs.transactions.len(),
        locked_transaction_count: Some(subs.locked_transactions.len()),
        transfer_count: None,
        sync_height: Some(
            w.wallet_synchronizer
                .transaction_synchronizer_status
                .last_known_block_height,
        ),
        cache_note: None,
        unknown_fields,
    }
}

fn from_green(kind: FileKind, w: &GreenWallet) -> WalletSummary {
    // `green::open` has already checked every key pair.
    let addresses = w
        .records
        .iter()
        .map(|r| AddressSummary {
            address: None,
            spend_public_key: r.spend_public_key,
            has_spend_secret: r.spend_secret_key.is_some(),
            spend_key_matches: r.spend_secret_key.map(|_| true),
            start_timestamp: r.creation_timestamp,
            start_height: None,
            unspent: None,
        })
        .collect();
    let cache_note = match (&w.history, &w.cache_error) {
        (Some(h), _) => Some(format!(
            "save level {} ({} cached keys)",
            h.save_level,
            h.cached_keys.len()
        )),
        (None, Some(e)) => Some(format!("cache not readable: {e}")),
        (None, None) => Some("no cache".to_owned()),
    };
    WalletSummary {
        kind,
        view_public_key: Some(w.view.public_key),
        view_key_valid: true,
        view_keys_agree: None,
        view_only: w.is_tracking(),
        addresses,
        transaction_count: w.history.as_ref().map_or(0, |h| h.transactions.len()),
        locked_transaction_count: None,
        transfer_count: w.history.as_ref().map(|h| h.transfers.len()),
        sync_height: None,
        cache_note,
        unknown_fields: Vec::new(),
    }
}

impl fmt::Display for WalletSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "format: {:?}", self.kind)?;
        match &self.view_public_key {
            Some(k) => writeln!(f, "view public key: {k}")?,
            None => writeln!(
                f,
                "view public key: <private view key is not a valid scalar>"
            )?,
        }
        if let Some(agree) = self.view_keys_agree {
            writeln!(f, "view keys agree: {agree}")?;
        }
        writeln!(f, "view only: {}", self.view_only)?;
        writeln!(f, "addresses: {}", self.addresses.len())?;
        for (i, a) in self.addresses.iter().enumerate() {
            write!(f, "  [{i}] spend public key {}", a.spend_public_key)?;
            if let Some(addr) = &a.address {
                write!(f, ", address {addr}")?;
            }
            write!(
                f,
                ", spend secret: {}",
                if a.has_spend_secret {
                    "present"
                } else {
                    "none"
                }
            )?;
            if let Some(m) = a.spend_key_matches {
                write!(f, " (matches: {m})")?;
            }
            write!(f, ", start timestamp {}", a.start_timestamp)?;
            if let Some(h) = a.start_height {
                write!(f, ", start height {h}")?;
            }
            if let Some((n, sum)) = a.unspent {
                write!(f, ", unspent {n} outputs / {sum} atomic units")?;
            }
            writeln!(f)?;
        }
        writeln!(f, "transactions: {}", self.transaction_count)?;
        if let Some(n) = self.locked_transaction_count {
            writeln!(f, "unconfirmed sent transactions: {n}")?;
        }
        if let Some(n) = self.transfer_count {
            writeln!(f, "transfers: {n}")?;
        }
        if let Some(h) = self.sync_height {
            writeln!(f, "synced to height: {h}")?;
        }
        if let Some(c) = &self.cache_note {
            writeln!(f, "cache: {c}")?;
        }
        if !self.unknown_fields.is_empty() {
            writeln!(f, "kept unknown fields: {}", self.unknown_fields.join(", "))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_summary_has_no_secrets() {
        let wallet = backend::json::parse(backend::tests::cpp_json().as_bytes()).unwrap();
        let file = backend::save(&wallet, "pw", &mut xkr_crypto::os_rng());
        let s = summarize(&file, "pw").unwrap();
        assert_eq!(s.kind, FileKind::WalletBackend);
        assert_eq!(s.addresses.len(), 1);
        assert_eq!(s.addresses[0].unspent, Some((1, 1000)));
        // the sample keys are not real key pairs
        assert_eq!(s.addresses[0].spend_key_matches, Some(false));
        let text = s.to_string();
        let secrets = [
            wallet.sub_wallets.private_view_key.to_hex(),
            wallet.sub_wallets.sub_wallet[0].private_spend_key.to_hex(),
            wallet.sub_wallets.tx_private_keys[0]
                .tx_private_key
                .to_hex(),
        ];
        for secret in secrets {
            assert!(
                !text.contains(&secret),
                "summary shows a private key:\n{text}"
            );
            assert!(!format!("{s:?}").contains(&secret));
        }
    }

    #[test]
    fn green_summary_has_no_secrets() {
        let secret = xkr_types::SecretKey([5; 32]);
        let public = xkr_crypto::secret_key_to_public_key(&secret).unwrap();
        let w = GreenWallet {
            view: green::GreenViewKeys {
                public_key: public,
                secret_key: secret,
                creation_timestamp: 1,
            },
            records: vec![green::GreenRecord {
                spend_public_key: public,
                spend_secret_key: Some(secret),
                creation_timestamp: 2,
            }],
            history: None,
            cache_error: None,
        };
        let file = green::write_keys_only(&w, "", &mut xkr_crypto::os_rng()).unwrap();
        let s = summarize(&file, "").unwrap();
        assert_eq!(s.kind, FileKind::WalletGreen);
        assert_eq!(s.cache_note.as_deref(), Some("no cache"));
        let text = s.to_string();
        assert!(!text.contains(&secret.to_hex()));
        assert!(text.contains(&public.to_hex()));
    }

    #[test]
    fn unopenable_files_give_clear_errors() {
        assert!(matches!(
            summarize(b"hello", ""),
            Err(FileError::NotAWalletFile)
        ));
        assert!(matches!(
            summarize(&[3u8; 4], ""),
            Err(FileError::WrongVersion)
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            summarize_file(&dir.path().join("none"), ""),
            Err(FileError::FilenameNonExistent)
        ));
    }
}
