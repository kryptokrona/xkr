//! Format A: the `WalletBackend` file of wallet-api, zedwallet++ (xkrwallet-beta),
//! wallet-backend-js and Hugin.
//!
//! [`envelope`] is the encryption (`src/wallet_backend/wallet_backend.cpp`
//! `WalletBackend::openWallet` / `unsafeSave`), [`json`] the content
//! (`WalletBackend::toJSON` / `fromJSON`). The functions here combine both.

pub mod envelope;
pub mod json;

use std::path::Path;

use rand_core::CryptoRng;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub use envelope::{
    IS_A_WALLET_IDENTIFIER, IS_CORRECT_PASSWORD_IDENTIFIER, PBKDF2_ITERATIONS, SALT_SIZE,
};
pub use json::{
    SubWalletJson, SubWalletsJson, SynchronizationStatusJson, TransactionInputJson,
    TransactionJson, TransferJson, TxPrivateKeyJson, UnconfirmedInputJson, UnknownFields,
    WALLET_FILE_FORMAT_VERSION, WalletBackendJson, WalletSynchronizerJson,
};

use crate::FileError;
use crate::atomic::atomic_write;

/// Decrypts and parses a `WalletBackend` file (C++ `WalletBackend::openWallet` up to
/// `fromJSON`). Errors as in [`envelope::decrypt`] and [`json::parse`].
pub fn open(file: &[u8], password: &str) -> Result<WalletBackendJson, FileError> {
    let plain = envelope::decrypt(file, password)?;
    json::parse(&plain)
}

/// Serialises and encrypts a wallet with a fresh random salt (C++ `WalletBackend::unsafeSave`,
/// `rnd::randomBytes(16, salt)`). Pass `xkr_crypto::os_rng()`.
#[must_use]
pub fn save<R: CryptoRng + ?Sized>(
    wallet: &WalletBackendJson,
    password: &str,
    rng: &mut R,
) -> Vec<u8> {
    let mut salt = [0u8; SALT_SIZE];
    rng.fill_bytes(&mut salt);
    save_with_salt(wallet, password, &salt)
}

/// [`save`] with a given salt. With the salt of the file a wallet was read from
/// ([`envelope::salt_of`]) and unchanged content, a file written by C++ comes out identical.
#[must_use]
pub fn save_with_salt(
    wallet: &WalletBackendJson,
    password: &str,
    salt: &[u8; SALT_SIZE],
) -> Vec<u8> {
    let text = Zeroizing::new(json::to_bytes(wallet));
    envelope::encrypt(&text, password, salt)
}

/// Reads and opens a `WalletBackend` file. A missing file is
/// [`FileError::FilenameNonExistent`], an unreadable one [`FileError::InvalidWalletFilename`].
pub fn open_file(path: &Path, password: &str) -> Result<WalletBackendJson, FileError> {
    let bytes = std::fs::read(path).map_err(FileError::from_open)?;
    open(&bytes, password)
}

/// Saves a wallet to `path` with a fresh random salt, replacing the file in one step
/// ([`crate::atomic_write`]). A write failure is [`FileError::InvalidWalletFilename`]
/// (C++ `unsafeSave`).
pub fn save_file<R: CryptoRng + ?Sized>(
    path: &Path,
    wallet: &WalletBackendJson,
    password: &str,
    rng: &mut R,
) -> Result<(), FileError> {
    let bytes = save(wallet, password, rng);
    atomic_write(path, &bytes).map_err(FileError::InvalidWalletFilename)
}

/// Like [`save_file`], but fails with [`FileError::WalletFileAlreadyExists`] if `path` exists
/// (C++ `checkNewWalletFilename` before `WalletBackend::createWallet` and the imports).
pub fn create_file<R: CryptoRng + ?Sized>(
    path: &Path,
    wallet: &WalletBackendJson,
    password: &str,
    rng: &mut R,
) -> Result<(), FileError> {
    if path.exists() {
        return Err(FileError::WalletFileAlreadyExists);
    }
    save_file(path, wallet, password, rng)
}

/// `JSON.stringify(buffer)` of a Node.js `Buffer`, the form of wallet-backend-js
/// `WalletEncryption.encryptWalletToString` / `decryptWalletFromString`.
#[derive(Serialize, Deserialize)]
struct JsBuffer {
    #[serde(rename = "type")]
    kind: String,
    data: Vec<u8>,
}

/// Decodes the string form of wallet-backend-js `encryptWalletToString`
/// (`{"type":"Buffer","data":[73,102,...]}`) into the file bytes. Anything else is
/// [`FileError::NotAWalletFile`].
pub fn decode_js_buffer_string(text: &str) -> Result<Vec<u8>, FileError> {
    let buffer: JsBuffer = serde_json::from_str(text).map_err(|_| FileError::NotAWalletFile)?;
    if buffer.kind != "Buffer" {
        return Err(FileError::NotAWalletFile);
    }
    Ok(buffer.data)
}

/// The string form of wallet-backend-js `encryptWalletToString` for the given file bytes.
#[must_use]
pub fn encode_js_buffer_string(file: &[u8]) -> String {
    let buffer = JsBuffer {
        kind: "Buffer".to_owned(),
        data: file.to_vec(),
    };
    serde_json::to_string(&buffer).expect("plain struct")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::envelope::{decrypt_with_iterations, encrypt_with_iterations};

    const FAST: u32 = 3;

    const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const K3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const K4: &str = "4444444444444444444444444444444444444444444444444444444444444444";
    const K5: &str = "5555555555555555555555555555555555555555555555555555555555555555";
    const Z: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn input(ki: &str) -> String {
        format!(
            r#"{{"keyImage":"{ki}","amount":1000,"blockHeight":12,"transactionPublicKey":"{K2}","transactionIndex":1,"globalOutputIndex":77,"key":"{K3}","spendHeight":0,"unlockTime":0,"parentTransactionHash":"{K4}"}}"#
        )
    }

    /// A wallet as C++ `WalletBackend::toJSON` writes it: compact, C++ field order.
    pub(crate) fn cpp_json() -> String {
        let tx = format!(
            r#"{{"transfers":[{{"publicKey":"{K1}","amount":-1010}},{{"publicKey":"{K5}","amount":500}}],"hash":"{K4}","fee":10,"blockHeight":13,"timestamp":1600000000,"paymentID":"","unlockTime":0,"isCoinbaseTransaction":false}}"#
        );
        let sub = format!(
            r#"{{"publicSpendKey":"{K1}","privateSpendKey":"{K2}","address":"SEKReXXX","syncStartTimestamp":0,"unspentInputs":[{}],"lockedInputs":[],"spentInputs":[{}],"syncStartHeight":10,"isPrimaryAddress":true,"unconfirmedIncomingAmounts":[{{"amount":5,"key":"{K3}","parentTransactionHash":"{K4}"}}]}}"#,
            input(K5),
            input(K3)
        );
        format!(
            r#"{{"walletFileFormatVersion":0,"subWallets":{{"publicSpendKeys":["{K1}"],"subWallet":[{sub}],"transactions":[{tx}],"lockedTransactions":[],"privateViewKey":"{K5}","isViewWallet":false,"txPrivateKeys":[{{"transactionHash":"{K4}","txPrivateKey":"{K3}"}}]}},"walletSynchronizer":{{"transactionSynchronizerStatus":{{"blockHashCheckpoints":["{K1}"],"lastKnownBlockHashes":["{K2}","{K3}"],"lastKnownBlockHeight":5000}},"startTimestamp":0,"startHeight":10,"privateViewKey":"{K5}"}}}}"#
        )
    }

    /// A view wallet as wallet-backend-js writes it: JS field order, `knownTransactions`,
    /// `privateEphemeral` and `locked`.
    fn js_json() -> String {
        format!(
            r#"{{"walletFileFormatVersion":0,"subWallets":{{"publicSpendKeys":["{K1}"],"subWallet":[{{"unspentInputs":[{{"keyImage":"{K2}","amount":7,"blockHeight":1,"transactionPublicKey":"{K2}","transactionIndex":0,"globalOutputIndex":0,"key":"{K3}","spendHeight":0,"unlockTime":0,"parentTransactionHash":"{K4}","privateEphemeral":"{K5}"}}],"lockedInputs":[],"spentInputs":[],"unconfirmedIncomingAmounts":[{{"amount":3,"key":"{K3}","parentTransactionHash":"{K4}","locked":true}}],"publicSpendKey":"{K1}","privateSpendKey":"{Z}","syncStartTimestamp":0,"syncStartHeight":0,"address":"SEKRe","isPrimaryAddress":true}}],"transactions":[],"lockedTransactions":[],"privateViewKey":"{K5}","isViewWallet":true,"txPrivateKeys":[],"knownTransactions":["{K4}"]}},"walletSynchronizer":{{"privateViewKey":"{K5}","startHeight":0,"startTimestamp":0,"transactionSynchronizerStatus":{{"blockHashCheckpoints":[],"lastKnownBlockHashes":[],"lastKnownBlockHeight":0}}}}}}"#
        )
    }

    #[test]
    fn cpp_json_is_byte_stable() {
        let text = cpp_json();
        let wallet = json::parse(text.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(json::to_bytes(&wallet)).unwrap(), text);

        assert_eq!(wallet.sub_wallets.sub_wallet.len(), 1);
        let sub = &wallet.sub_wallets.sub_wallet[0];
        assert_eq!(sub.public_spend_key.to_hex(), K1);
        assert_eq!(sub.private_spend_key.to_hex(), K2);
        assert_eq!(sub.unspent_inputs[0].global_output_index, 77);
        assert_eq!(
            wallet.sub_wallets.transactions[0].transfers[0].amount,
            -1010
        );
        assert_eq!(
            wallet
                .wallet_synchronizer
                .transaction_synchronizer_status
                .last_known_block_height,
            5000
        );
        assert!(wallet.other.is_empty());
    }

    #[test]
    fn cpp_file_is_byte_stable_with_its_salt() {
        let salt = [0x5au8; 16];
        let file = encrypt_with_iterations(cpp_json().as_bytes(), "pw", &salt, FAST);
        let plain = decrypt_with_iterations(&file, "pw", FAST).unwrap();
        let wallet = json::parse(&plain).unwrap();
        let again = encrypt_with_iterations(&json::to_bytes(&wallet), "pw", &salt, FAST);
        assert_eq!(again, file);
    }

    #[test]
    fn js_only_fields_are_kept() {
        let text = js_json();
        let wallet = json::parse(text.as_bytes()).unwrap();
        assert!(wallet.sub_wallets.is_view_wallet);
        assert!(wallet.sub_wallets.sub_wallet[0].private_spend_key.is_zero());
        assert!(wallet.sub_wallets.other.contains_key("knownTransactions"));
        assert!(
            wallet.sub_wallets.sub_wallet[0].unspent_inputs[0]
                .other
                .contains_key("privateEphemeral")
        );
        assert_eq!(
            wallet.sub_wallets.sub_wallet[0].unconfirmed_incoming_amounts[0]
                .other
                .get("locked"),
            Some(&serde_json::Value::Bool(true))
        );
        // Same JSON value after a round trip (field order is not kept for JS files).
        let original: serde_json::Value = serde_json::from_str(&text).unwrap();
        let back: serde_json::Value = serde_json::from_slice(&json::to_bytes(&wallet)).unwrap();
        assert_eq!(original, back);
    }

    #[test]
    fn unknown_top_level_and_nested_fields_survive() {
        let mut v: serde_json::Value = serde_json::from_str(&cpp_json()).unwrap();
        v["hugin"] = serde_json::json!({"boards": ["a"]});
        v["subWallets"]["transactions"][0]["memo"] = serde_json::json!("x");
        let text = serde_json::to_vec(&v).unwrap();
        let wallet = json::parse(&text).unwrap();
        assert_eq!(wallet.other["hugin"], serde_json::json!({"boards": ["a"]}));
        assert_eq!(wallet.sub_wallets.transactions[0].other["memo"], "x");
        let back: serde_json::Value = serde_json::from_slice(&json::to_bytes(&wallet)).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn json_errors() {
        assert!(matches!(
            json::parse(b""),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            json::parse(b"{not json"),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            json::parse(b"{}"),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            json::parse(br#"{"walletFileFormatVersion":-1}"#),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            json::parse(br#"{"walletFileFormatVersion":1,"anything":true}"#),
            Err(FileError::UnsupportedWalletFileFormatVersion)
        ));
        // version 0 but a field missing
        assert!(matches!(
            json::parse(br#"{"walletFileFormatVersion":0}"#),
            Err(FileError::WalletFileCorrupted(_))
        ));
        // malformed key
        let bad = cpp_json().replacen(K1, "zz", 1);
        assert!(matches!(
            json::parse(bad.as_bytes()),
            Err(FileError::WalletFileCorrupted(_))
        ));
    }

    #[test]
    fn open_and_save_with_real_iterations() {
        let wallet = json::parse(cpp_json().as_bytes()).unwrap();
        let file = save(&wallet, "hunter2", &mut xkr_crypto::os_rng());
        let salt = envelope::salt_of(&file).unwrap();
        assert_eq!(open(&file, "hunter2").unwrap(), wallet);
        assert_eq!(save_with_salt(&wallet, "hunter2", &salt), file);
        assert!(matches!(
            open(&file, "hunter3"),
            Err(FileError::WrongPassword | FileError::WalletFileCorrupted(_))
        ));
    }

    #[test]
    fn files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.wallet");
        assert!(matches!(
            open_file(&path, "pw"),
            Err(FileError::FilenameNonExistent)
        ));
        let wallet = json::parse(cpp_json().as_bytes()).unwrap();
        create_file(&path, &wallet, "pw", &mut xkr_crypto::os_rng()).unwrap();
        assert!(matches!(
            create_file(&path, &wallet, "pw", &mut xkr_crypto::os_rng()),
            Err(FileError::WalletFileAlreadyExists)
        ));
        assert_eq!(open_file(&path, "pw").unwrap(), wallet);
        let missing_dir = dir.path().join("nope").join("w.wallet");
        assert!(matches!(
            save_file(&missing_dir, &wallet, "pw", &mut xkr_crypto::os_rng()),
            Err(FileError::InvalidWalletFilename(_))
        ));
    }

    #[test]
    fn js_buffer_string() {
        let file = vec![73u8, 102, 0, 255];
        let s = encode_js_buffer_string(&file);
        assert_eq!(s, r#"{"type":"Buffer","data":[73,102,0,255]}"#);
        assert_eq!(decode_js_buffer_string(&s).unwrap(), file);
        assert!(matches!(
            decode_js_buffer_string("[1,2]"),
            Err(FileError::NotAWalletFile)
        ));
        assert!(matches!(
            decode_js_buffer_string(r#"{"type":"Uint8Array","data":[1]}"#),
            Err(FileError::NotAWalletFile)
        ));
    }
}
