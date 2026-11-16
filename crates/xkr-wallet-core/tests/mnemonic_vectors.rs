//! Mnemonic seeds captured from the C++ tools (`/keys/mnemonic` of C++ wallet-api and the seeds
//! of known keys), in `tests/data/mnemonic.json`. The test is skipped while that file is
//! missing.
//!
//! Accepted shape: a list (or `{"vectors": [...]}`) of objects with the seed under
//! `mnemonicSeed`, `mnemonic` or `seed` and the private spend key under `privateSpendKey` (or `private_spend_key` /
//! `spendKey`); `privateViewKey` and `address`, when present, are checked too.

use serde_json::Value;
use xkr_types::SecretKey;
use xkr_wallet_core::WalletKeys;
use xkr_wallet_core::mnemonic::{mnemonic_from_secret, secret_from_mnemonic};

fn field<'a>(item: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| item[*n].as_str())
}

#[test]
fn captured_cpp_mnemonics() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mnemonic.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("skipped: {} is not present", path.display());
        return;
    };
    let doc: Value = serde_json::from_str(&text).expect("mnemonic.json parses");
    let items = doc
        .as_array()
        .or_else(|| doc["vectors"].as_array())
        .expect("a list of vectors");
    assert!(!items.is_empty(), "mnemonic.json has no vectors");
    for (n, item) in items.iter().enumerate() {
        let seed = field(item, &["mnemonicSeed", "mnemonic", "seed"]).expect("seed");
        let spend: SecretKey = field(item, &["privateSpendKey", "private_spend_key", "spendKey"])
            .expect("private spend key")
            .parse()
            .expect("private spend key hex");
        assert_eq!(secret_from_mnemonic(seed), Ok(spend), "vector {n}");
        let canonical: Vec<&str> = seed.split_whitespace().collect();
        assert_eq!(
            mnemonic_from_secret(&spend),
            canonical.join(" "),
            "vector {n}"
        );
        let keys = WalletKeys::from_spend_secret(&spend);
        if let Some(view) = field(item, &["privateViewKey", "private_view_key", "viewKey"]) {
            let keys = keys.as_ref().expect("canonical spend key");
            assert_eq!(
                keys.view_secret.to_hex(),
                view.to_ascii_lowercase(),
                "vector {n}"
            );
        }
        if let Some(address) = field(item, &["address"]) {
            let keys = keys.as_ref().expect("canonical spend key");
            assert_eq!(keys.address().to_string(), address, "vector {n}");
        }
    }
}

/// The seeds C++ refused (`invalid` in `tests/data/mnemonic.json`) are refused with the same
/// error number.
#[test]
fn captured_cpp_refusals() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mnemonic.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("skipped: {} is not present", path.display());
        return;
    };
    let doc: Value = serde_json::from_str(&text).expect("mnemonic.json parses");
    let Some(items) = doc["invalid"].as_array() else {
        return;
    };
    for item in items {
        let name = item["name"].as_str().unwrap_or("?");
        let seed = item["mnemonicSeed"].as_str().unwrap_or_default();
        let Some(code) = item["response"]["errorCode"].as_u64() else {
            continue;
        };
        let err = secret_from_mnemonic(seed).expect_err(name);
        assert_eq!(u64::from(err.code()), code, "{name}: {err}");
    }
}
