//! Telling the wallet file formats apart from their first bytes.

use crate::backend::IS_A_WALLET_IDENTIFIER;
use crate::green::container::{
    METADATA_SIZE, PREFIX_SIZE, RECORD_SIZE, SERIALIZATION_VERSION, parse_layout,
};

/// What kind of wallet file some bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A `WalletBackend` file (wallet-api, zedwallet++, wallet-backend-js): starts with
    /// [`IS_A_WALLET_IDENTIFIER`].
    WalletBackend,
    /// A `WalletBackend` file in the string form of wallet-backend-js
    /// `encryptWalletToString` (`{"type":"Buffer","data":[...]}`); decode it with
    /// [`crate::backend::decode_js_buffer_string`].
    WalletBackendJsString,
    /// A `WalletGreen` container (walletd, zedwallet): version byte 6 and a consistent
    /// prefix, metadata and record area.
    WalletGreen,
    /// A wallet file of version 1 to 5, from before the version 6 container: the
    /// `WalletLegacy` file (version 1) or a `WalletSerializerV1` file (versions 2 to 5). Open
    /// it with [`crate::green::legacy::open_legacy`]; C++ converted it to a container on
    /// first open ([`crate::green::legacy::convert_file`]).
    WalletGreenLegacy(u8),
    /// A container of another version: first byte 1-5 that does not frame like
    /// [`FileKind::WalletGreenLegacy`], or another first byte
    /// with an otherwise consistent container layout. C++ refuses both with `WRONG_VERSION`.
    WalletGreenOtherVersion(u8),
    /// None of the above.
    Unknown,
}

/// Detects the format of a wallet file from its contents. Nothing is decrypted.
#[must_use]
pub fn detect(bytes: &[u8]) -> FileKind {
    if bytes.starts_with(&IS_A_WALLET_IDENTIFIER) {
        return FileKind::WalletBackend;
    }
    if is_js_buffer_string(bytes) {
        return FileKind::WalletBackendJsString;
    }
    if let Some(v) = crate::green::legacy::legacy_version(bytes) {
        return FileKind::WalletGreenLegacy(v);
    }
    match bytes.first() {
        Some(&SERIALIZATION_VERSION) if parse_layout(bytes).is_ok() => FileKind::WalletGreen,
        Some(&v)
            if (1..SERIALIZATION_VERSION).contains(&v)
                || (v != SERIALIZATION_VERSION && plausible_container(bytes)) =>
        {
            FileKind::WalletGreenOtherVersion(v)
        }
        _ => FileKind::Unknown,
    }
}

/// The layout checks of [`parse_layout`] without the version byte.
fn plausible_container(bytes: &[u8]) -> bool {
    let header = PREFIX_SIZE + METADATA_SIZE;
    if bytes.len() < header {
        return false;
    }
    let word = |at: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(a)
    };
    let capacity = word(PREFIX_SIZE);
    let size = word(PREFIX_SIZE + 8);
    size <= capacity
        && usize::try_from(capacity)
            .ok()
            .and_then(|c| c.checked_mul(RECORD_SIZE))
            .and_then(|n| n.checked_add(header))
            .is_some_and(|end| end <= bytes.len())
}

fn is_js_buffer_string(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    crate::backend::decode_js_buffer_string(text)
        .is_ok_and(|data| data.starts_with(&IS_A_WALLET_IDENTIFIER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::encode_js_buffer_string;

    #[test]
    fn detects_backend_files() {
        let mut f = IS_A_WALLET_IDENTIFIER.to_vec();
        f.extend_from_slice(&[0u8; 48]);
        assert_eq!(detect(&f), FileKind::WalletBackend);
        assert_eq!(
            detect(encode_js_buffer_string(&f).as_bytes()),
            FileKind::WalletBackendJsString
        );
        assert_eq!(
            detect(encode_js_buffer_string(b"other").as_bytes()),
            FileKind::Unknown
        );
    }

    #[test]
    fn detects_containers() {
        let w = crate::green::GreenWallet {
            view: crate::green::GreenViewKeys {
                public_key: xkr_types::PublicKey::ZERO,
                secret_key: xkr_types::SecretKey::ZERO,
                creation_timestamp: 0,
            },
            records: vec![],
            history: None,
            cache_error: None,
        };
        let mut file = crate::green::write_keys_only(&w, "", &mut xkr_crypto::os_rng()).unwrap();
        assert_eq!(detect(&file), FileKind::WalletGreen);
        file[0] = 5;
        assert_eq!(detect(&file), FileKind::WalletGreenOtherVersion(5));
        // legacy serializer versions are named even when the layout differs
        assert_eq!(detect(&file[..50]), FileKind::WalletGreenOtherVersion(5));
        file[0] = 0x42;
        assert_eq!(detect(&file), FileKind::WalletGreenOtherVersion(0x42));
        assert_eq!(detect(&file[..50]), FileKind::Unknown);
    }

    #[test]
    fn unknown() {
        assert_eq!(detect(b""), FileKind::Unknown);
        assert_eq!(detect(b"[]"), FileKind::Unknown);
        assert_eq!(detect(&[6u8; 10]), FileKind::Unknown);
    }
}
