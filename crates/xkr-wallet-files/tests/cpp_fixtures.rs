//! Wallet files made by the C++ tools, captured by `testnet/fixtures/capture-wallet-files.sh`
//! (wallet-api, kryptokrona-service) and `capture-cli.sh` (xkrwallet-beta = zedwallet++,
//! xkrwallet = zedwallet). A test skips only while its directory is missing or holds no wallet
//! file; otherwise every file is checked strictly.
//!
//! Layout under `tests/data/`:
//!
//! ```text
//! backend/<file>.wallet, green/<file>.wallet   the wallet file
//! <file>.wallet.json                           sidecar: {file, format: "A" | "B", tool, password,
//!                                              description, expected: <dump taken from the tool
//!                                              right before it saved the file>, ...}
//! addressbook/<tool>-addressBook.json          .addressBook.json of xkrwallet-beta ("beta-")
//!                                              and xkrwallet ("xkrwallet-")
//! ```
//!
//! The `expected` dump depends on `tool`:
//! * `wallet-api`: `/addresses`, `/addresses/primary`, `/keys`, `/keys/<address>`, balances,
//!   `/transactions`, `/status`;
//! * `kryptokrona-service`: `getAddresses`, `getViewKey`, `getSpendKeys`, `getBalance` (per
//!   address and total), `getStatus`, `getTransactions`;
//! * `beta`, `xkrwallet`: what the terminal wallet printed: `address`, `privateViewKey`, and
//!   when present `privateSpendKey`, `publicSpendKey`, `mnemonicSeed`, `viewOnly`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use xkr_types::{PublicKey, SecretKey};
use xkr_wallet_core::Address;
use xkr_wallet_files::backend::{SubWalletJson, WalletBackendJson};
use xkr_wallet_files::{
    AddressBook, AddressBookForm, FileKind, GreenWallet, backend, detect, green,
};

fn data_dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(sub)
}

/// A wallet file and its sidecar.
struct Case {
    path: PathBuf,
    sidecar: Value,
}

impl Case {
    fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn password(&self) -> &str {
        self.sidecar["password"]
            .as_str()
            .expect("sidecar has a password")
    }

    fn tool(&self) -> &str {
        self.sidecar["tool"].as_str().expect("sidecar has a tool")
    }

    fn expected(&self) -> &Value {
        &self.sidecar["expected"]
    }
}

/// Every `*.wallet` in `tests/data/<sub>` with its `<file>.json` sidecar, or `None` to skip.
/// A wallet without a sidecar, or a sidecar without its wallet, fails the test.
fn cases(sub: &str, format: &str) -> Option<Vec<Case>> {
    let dir = data_dir(sub);
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("skipped: {} not present", dir.display());
        return None;
    };
    let mut names: Vec<String> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in &names {
        if let Some(wallet) = name.strip_suffix(".json") {
            if wallet.ends_with(".wallet") {
                assert!(
                    dir.join(wallet).exists(),
                    "{sub}/{name}: sidecar without {wallet}"
                );
            }
            continue;
        }
        if !name.ends_with(".wallet") {
            continue;
        }
        let sidecar_path = dir.join(format!("{name}.json"));
        let text = fs::read(&sidecar_path)
            .unwrap_or_else(|e| panic!("{sub}/{name}: no sidecar {name}.json ({e})"));
        let sidecar: Value = serde_json::from_slice(&text)
            .unwrap_or_else(|e| panic!("{sub}/{name}.json: not JSON ({e})"));
        assert_eq!(
            sidecar["file"].as_str(),
            Some(name.as_str()),
            "{sub}/{name}.json: file"
        );
        assert_eq!(
            sidecar["format"].as_str(),
            Some(format),
            "{sub}/{name}.json: format"
        );
        out.push(Case {
            path: dir.join(name),
            sidecar,
        });
    }
    if out.is_empty() {
        eprintln!("skipped: {} has no wallet files", dir.display());
        return None;
    }
    Some(out)
}

fn str_of<'a>(v: &'a Value, key: &str, what: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: `{key}` missing in the dump"))
}

fn u64_of(v: &Value, key: &str, what: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{what}: `{key}` missing in the dump"))
}

fn address_of(spend: PublicKey, view_secret: &SecretKey) -> String {
    let view = xkr_crypto::secret_key_to_public_key(view_secret).expect("valid view key");
    Address::new(spend, view).encode()
}

/// C++ `utilities::isInputUnlocked` (`CRYPTONOTE_MAX_BLOCK_NUMBER` 500 000 000,
/// `CRYPTONOTE_LOCKED_TX_ALLOWED_DELTA_BLOCKS` 1, `..._DELTA_SECONDS` 90).
fn input_unlocked(unlock_time: u64, height: u64) -> bool {
    if unlock_time == 0 {
        return true;
    }
    if unlock_time >= 500_000_000 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        return now + 90 >= unlock_time;
    }
    height + 1 >= unlock_time
}

/// C++ `SubWallet::getBalance`: `(unlocked, locked)` at `height` (the daemon's network block
/// count, as `WalletBackend::getBalance` passes it).
fn backend_balance(sub: &SubWalletJson, height: u64) -> (u64, u64) {
    let mut unlocked = 0;
    let mut locked = 0;
    for input in &sub.unspent_inputs {
        if input_unlocked(input.unlock_time, height) {
            unlocked += input.amount;
        } else {
            locked += input.amount;
        }
    }
    for u in &sub.unconfirmed_incoming_amounts {
        locked += u.amount;
    }
    (unlocked, locked)
}

fn primary(wallet: &WalletBackendJson, what: &str) -> SubWalletJson {
    let mut primaries = wallet
        .sub_wallets
        .sub_wallet
        .iter()
        .filter(|s| s.is_primary_address);
    let p = primaries
        .next()
        .unwrap_or_else(|| panic!("{what}: no primary subwallet"));
    assert!(primaries.next().is_none(), "{what}: two primary subwallets");
    p.clone()
}

// ------------------------------------------------------------------ format A

#[test]
fn backend_files() {
    let Some(cases) = cases("backend", "A") else {
        return;
    };
    for case in cases {
        let what = format!("backend/{}", case.name());
        let bytes = fs::read(&case.path).unwrap();
        assert_eq!(detect(&bytes), FileKind::WalletBackend, "{what}");
        let password = case.password();
        let wallet = backend::open(&bytes, password).unwrap_or_else(|e| panic!("{what}: {e}"));

        // Shared invariants.
        let subs = &wallet.sub_wallets;
        assert_eq!(
            wallet.wallet_synchronizer.private_view_key, subs.private_view_key,
            "{what}"
        );
        let listed: BTreeSet<PublicKey> = subs.public_spend_keys.iter().copied().collect();
        let stored: BTreeSet<PublicKey> =
            subs.sub_wallet.iter().map(|s| s.public_spend_key).collect();
        assert_eq!(listed, stored, "{what}: publicSpendKeys vs subWallet");
        for s in &subs.sub_wallet {
            assert_eq!(
                s.address,
                address_of(s.public_spend_key, &subs.private_view_key),
                "{what}"
            );
            if subs.is_view_wallet {
                assert!(
                    s.private_spend_key.is_zero(),
                    "{what}: view wallet with a spend key"
                );
            } else {
                assert_eq!(
                    xkr_crypto::secret_key_to_public_key(&s.private_spend_key),
                    Some(s.public_spend_key),
                    "{what}: spend key pair"
                );
            }
        }

        match case.tool() {
            "wallet-api" => check_wallet_api_dump(&wallet, case.expected(), &what),
            "beta" => check_beta_dump(&wallet, case.expected(), &what),
            other => panic!("{what}: unknown tool {other}"),
        }

        // C++ files re-encrypt byte for byte with their own salt.
        let salt = backend::envelope::salt_of(&bytes).unwrap();
        assert!(
            backend::save_with_salt(&wallet, password, &salt) == bytes,
            "{what}: re-encrypting with the file's salt does not give the same bytes"
        );

        // The summary shows no private key.
        let text = xkr_wallet_files::summarize(&bytes, password)
            .unwrap()
            .to_string();
        assert!(
            !text.contains(&subs.private_view_key.to_hex()),
            "{what}: summary shows a key"
        );
        for s in &subs.sub_wallet {
            if !s.private_spend_key.is_zero() {
                assert!(
                    !text.contains(&s.private_spend_key.to_hex()),
                    "{what}: summary shows a key"
                );
            }
        }
    }
}

fn check_wallet_api_dump(wallet: &WalletBackendJson, exp: &Value, what: &str) {
    let subs = &wallet.sub_wallets;
    let by_address: BTreeMap<&str, &SubWalletJson> = subs
        .sub_wallet
        .iter()
        .map(|s| (s.address.as_str(), s))
        .collect();

    // addresses, primary, view key, status
    let want: BTreeSet<&str> = exp["addresses"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `addresses` missing"))
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(
        by_address.keys().copied().collect::<BTreeSet<_>>(),
        want,
        "{what}: addresses"
    );
    assert_eq!(
        primary(wallet, what).address,
        str_of(exp, "primaryAddress", what),
        "{what}: primary"
    );
    assert_eq!(
        subs.private_view_key.to_hex(),
        str_of(exp, "privateViewKey", what),
        "{what}: view key"
    );
    let status = &exp["status"];
    assert_eq!(
        Some(subs.is_view_wallet),
        status["isViewWallet"].as_bool(),
        "{what}: isViewWallet"
    );
    assert_eq!(
        subs.sub_wallet.len() as u64,
        u64_of(status, "subWalletCount", what),
        "{what}"
    );
    let height = u64_of(status, "networkBlockCount", what);

    // keys and balance per subwallet
    let mut total = (0u64, 0u64);
    for e in exp["subWallets"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `subWallets` missing"))
    {
        let address = str_of(e, "address", what);
        let sub = by_address[address];
        let keys = &e["keys"];
        if let Some(secret) = keys["privateSpendKey"].as_str() {
            assert_eq!(
                sub.private_spend_key.to_hex(),
                secret,
                "{what}: {address} spend key"
            );
            assert_eq!(
                sub.public_spend_key.to_hex(),
                str_of(keys, "publicSpendKey", what),
                "{what}"
            );
        } else {
            assert!(
                subs.is_view_wallet,
                "{what}: {address}: no keys in the dump but not a view wallet"
            );
        }
        let (unlocked, locked) = backend_balance(sub, height);
        assert_eq!(
            unlocked,
            u64_of(&e["balance"], "unlocked", what),
            "{what}: {address} unlocked"
        );
        assert_eq!(
            locked,
            u64_of(&e["balance"], "locked", what),
            "{what}: {address} locked"
        );
        total = (total.0 + unlocked, total.1 + locked);
    }
    assert_eq!(
        total.0,
        u64_of(&exp["balance"], "unlocked", what),
        "{what}: total unlocked"
    );
    assert_eq!(
        total.1,
        u64_of(&exp["balance"], "locked", what),
        "{what}: total locked"
    );

    // transactions (wallet-api /transactions lists the confirmed ones)
    let txs = exp["transactions"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `transactions` missing"));
    assert_eq!(
        subs.transactions.len(),
        txs.len(),
        "{what}: transaction count"
    );
    let address_of_key: BTreeMap<PublicKey, &str> = subs
        .sub_wallet
        .iter()
        .map(|s| (s.public_spend_key, s.address.as_str()))
        .collect();
    for t in txs {
        let hash = str_of(t, "hash", what);
        let tx = subs
            .transactions
            .iter()
            .find(|x| x.hash.to_hex() == hash)
            .unwrap_or_else(|| panic!("{what}: transaction {hash} not in the file"));
        assert_eq!(tx.fee, u64_of(t, "fee", what), "{what}: {hash} fee");
        assert_eq!(
            tx.block_height,
            u64_of(t, "blockHeight", what),
            "{what}: {hash} height"
        );
        assert_eq!(
            tx.timestamp,
            u64_of(t, "timestamp", what),
            "{what}: {hash} timestamp"
        );
        assert_eq!(
            tx.unlock_time,
            u64_of(t, "unlockTime", what),
            "{what}: {hash} unlockTime"
        );
        assert_eq!(
            tx.payment_id,
            str_of(t, "paymentID", what),
            "{what}: {hash} paymentID"
        );
        assert_eq!(
            Some(tx.is_coinbase_transaction),
            t["isCoinbaseTransaction"].as_bool(),
            "{what}: {hash} coinbase"
        );
        let got: BTreeMap<&str, i64> = tx
            .transfers
            .iter()
            .map(|x| (address_of_key[&x.public_key], x.amount))
            .collect();
        let want: BTreeMap<&str, i64> = t["transfers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["address"].as_str().unwrap(),
                    x["amount"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, want, "{what}: {hash} transfers");
    }
}

fn check_beta_dump(wallet: &WalletBackendJson, exp: &Value, what: &str) {
    let subs = &wallet.sub_wallets;
    let p = primary(wallet, what);
    assert_eq!(p.address, str_of(exp, "address", what), "{what}: address");
    if let Some(view) = exp["privateViewKey"].as_str() {
        assert_eq!(subs.private_view_key.to_hex(), view, "{what}: view key");
    }
    if let Some(secret) = exp["privateSpendKey"].as_str() {
        assert_eq!(p.private_spend_key.to_hex(), secret, "{what}: spend key");
    }
    if let Some(public) = exp["publicSpendKey"].as_str() {
        assert_eq!(
            p.public_spend_key.to_hex(),
            public,
            "{what}: public spend key"
        );
    }
    if let Some(seed) = exp["mnemonicSeed"].as_str() {
        let secret = xkr_wallet_core::mnemonic::secret_from_mnemonic(seed).unwrap();
        assert_eq!(p.private_spend_key, secret, "{what}: seed");
    }
    let view_only = exp["viewOnly"].as_bool().unwrap_or(false);
    assert_eq!(subs.is_view_wallet, view_only, "{what}: view only");
}

// ------------------------------------------------------------------ format B

#[test]
fn green_files() {
    let Some(cases) = cases("green", "B") else {
        return;
    };
    let mut failures = Vec::new();
    for case in cases {
        let what = format!("green/{}", case.name());
        let bytes = fs::read(&case.path).unwrap();
        assert_eq!(detect(&bytes), FileKind::WalletGreen, "{what}");
        let password = case.password();
        let wallet = match green::open(&bytes, password) {
            Ok(w) => w,
            Err(e) => {
                // Report every container that does not open, with what each check found.
                failures.push(format!(
                    "{what}: {e}\n{}{}",
                    green::diagnose(&bytes, password),
                    password_variants_that_open(&bytes, password)
                ));
                continue;
            }
        };
        assert!(
            wallet.cache_error.is_none(),
            "{what}: cache not readable: {:?}",
            wallet.cache_error
        );

        match case.tool() {
            "kryptokrona-service" => check_walletd_dump(&wallet, case.expected(), &what),
            "xkrwallet" => check_xkrwallet_dump(&wallet, case.expected(), &what),
            other => panic!("{what}: unknown tool {other}"),
        }

        // A keys-only rewrite opens with the same keys and no history.
        let rewritten =
            green::write_keys_only(&wallet, password, &mut xkr_crypto::os_rng()).unwrap();
        let back = green::open(&rewritten, password).unwrap();
        assert_eq!(back.view, wallet.view, "{what}");
        assert_eq!(back.records, wallet.records, "{what}");
        assert!(back.history.is_none(), "{what}");

        // A wrong password is refused.
        let wrong = format!("{password}x");
        assert!(
            green::open(&bytes, &wrong).is_err(),
            "{what}: wrong password accepted"
        );

        // The summary shows no private key.
        let text = xkr_wallet_files::summarize(&bytes, password)
            .unwrap()
            .to_string();
        assert!(
            !text.contains(&wallet.view.secret_key.to_hex()),
            "{what}: summary shows a key"
        );
        for r in &wallet.records {
            if let Some(s) = r.spend_secret_key {
                assert!(!text.contains(&s.to_hex()), "{what}: summary shows a key");
            }
        }
    }
    assert!(
        failures.is_empty(),
        "containers that do not open:\n{}",
        failures.join("\n")
    );
}

/// Tries simple variants of `password` on a container that does not open with it and says
/// which, if any, opens it.
fn password_variants_that_open(bytes: &[u8], password: &str) -> String {
    let mut variants = vec![
        format!("{password}\n"),
        format!("{password}\r\n"),
        format!("{password}\0"),
        format!(" {password}"),
        format!("{password} "),
        password.trim().to_owned(),
        password.to_uppercase(),
        String::new(),
    ];
    for n in [8, 16] {
        if password.chars().count() > n {
            variants.push(password.chars().take(n).collect());
        }
    }
    let found: Vec<String> = variants
        .iter()
        .filter(|v| v.as_str() != password && green::open(bytes, v).is_ok())
        .map(|v| format!("{v:?}"))
        .collect();
    if found.is_empty() {
        "no simple password variant opens it\n".to_owned()
    } else {
        format!("opens with: {}\n", found.join(", "))
    }
}

/// `cn_slow_hash_v0` (the container key) against values from the C++ code, from
/// `tests/data/crypto/cn-slow-hash-v0.json`: `[{"input": "<text>", "hash": "<hex>"}, ...]`.
/// Without the file it prints the Rust values for the fixture passwords, to compare by hand.
#[test]
fn container_key_derivation_matches_cpp() {
    let path = data_dir("crypto").join("cn-slow-hash-v0.json");
    let Ok(text) = fs::read(&path) else {
        for p in [
            "",
            "walletd-pass",
            "cli-pass",
            "cli-pass-2",
            "view-pass",
            "compat-walletd-pass",
        ] {
            eprintln!(
                "cn_slow_hash_v0({p:?}) = {}",
                xkr_crypto::cn_slow_hash_v0(p.as_bytes())
            );
        }
        eprintln!("skipped: {} not present", path.display());
        return;
    };
    let vectors: Vec<Value> = serde_json::from_slice(&text).unwrap();
    let mut wrong = Vec::new();
    for v in &vectors {
        let input = v["input"].as_str().unwrap();
        let want = v["hash"].as_str().unwrap();
        let got = xkr_crypto::cn_slow_hash_v0(input.as_bytes()).to_hex();
        if got != want {
            wrong.push(format!("{input:?}: C++ {want}, Rust {got}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "cn_slow_hash_v0 differs:\n{}",
        wrong.join("\n")
    );
}

fn green_addresses(wallet: &GreenWallet) -> Vec<String> {
    wallet
        .records
        .iter()
        .map(|r| address_of(r.spend_public_key, &wallet.view.secret_key))
        .collect()
}

fn check_walletd_dump(wallet: &GreenWallet, exp: &Value, what: &str) {
    assert_eq!(
        wallet.view.secret_key.to_hex(),
        str_of(exp, "viewSecretKey", what),
        "{what}: view key"
    );
    let addresses = green_addresses(wallet);
    let want: Vec<&str> = exp["addresses"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `addresses` missing"))
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(addresses, want, "{what}: addresses (container order)");

    // Balances cached at SAVE_ALL, per address.
    let cached: BTreeMap<PublicKey, (u64, u64)> = wallet
        .history
        .iter()
        .flat_map(|h| h.cached_keys.iter())
        .filter_map(|k| k.balances.map(|b| (k.spend_public_key, b)))
        .collect();

    let spend_keys = exp["spendKeys"]
        .as_array()
        .unwrap_or_else(|| panic!("{what}: `spendKeys` missing"));
    assert_eq!(spend_keys.len(), wallet.records.len(), "{what}: spendKeys");
    let mut total = (0u64, 0u64);
    let mut any_balance = false;
    for e in spend_keys {
        let address = str_of(e, "address", what);
        let i = addresses.iter().position(|a| a == address).unwrap();
        let record = &wallet.records[i];
        let keys = &e["keys"];
        assert_eq!(
            record.spend_public_key.to_hex(),
            str_of(keys, "spendPublicKey", what),
            "{what}"
        );
        let dumped = str_of(keys, "spendSecretKey", what);
        if let Some(s) = record.spend_secret_key {
            assert_eq!(s.to_hex(), dumped, "{what}: {address} spend key");
        } else {
            // A tracking address has no spend key; the dumped value is not one of its keys.
            let dumped: SecretKey = dumped.parse().unwrap();
            assert_ne!(
                xkr_crypto::secret_key_to_public_key(&dumped),
                Some(record.spend_public_key),
                "{what}: {address}: tracking record but the dump has its spend key"
            );
        }
        if let Some(b) = e["balance"].as_object() {
            any_balance = true;
            let got = cached
                .get(&record.spend_public_key)
                .unwrap_or_else(|| panic!("{what}: {address}: no cached balance in the file"));
            let want = (
                b["availableBalance"].as_u64().unwrap(),
                b["lockedAmount"].as_u64().unwrap(),
            );
            assert_eq!(*got, want, "{what}: {address} balance (available, locked)");
            total = (total.0 + got.0, total.1 + got.1);
        }
    }
    if any_balance {
        let b = &exp["balance"];
        assert_eq!(
            total.0,
            u64_of(b, "availableBalance", what),
            "{what}: total available"
        );
        assert_eq!(
            total.1,
            u64_of(b, "lockedAmount", what),
            "{what}: total locked"
        );
    }

    // Transactions, where the dump lists them.
    if let Some(txs) = exp["transactions"].as_array().filter(|t| !t.is_empty()) {
        let history = wallet
            .history
            .as_ref()
            .unwrap_or_else(|| panic!("{what}: no history"));
        for t in txs {
            let hash = str_of(t, "transactionHash", what);
            assert!(
                history.transactions.iter().any(|x| x.hash.to_hex() == hash),
                "{what}: transaction {hash} not in the cache"
            );
        }
    }
}

fn check_xkrwallet_dump(wallet: &GreenWallet, exp: &Value, what: &str) {
    let first = wallet
        .records
        .first()
        .unwrap_or_else(|| panic!("{what}: no address record"));
    assert_eq!(
        green_addresses(wallet)[0],
        str_of(exp, "address", what),
        "{what}: address"
    );
    if let Some(view) = exp["privateViewKey"].as_str() {
        assert_eq!(wallet.view.secret_key.to_hex(), view, "{what}: view key");
    }
    if let Some(secret) = exp["privateSpendKey"].as_str() {
        assert_eq!(
            first.spend_secret_key.map(|k| k.to_hex()).as_deref(),
            Some(secret),
            "{what}"
        );
    }
    if let Some(public) = exp["publicSpendKey"].as_str() {
        assert_eq!(
            first.spend_public_key.to_hex(),
            public,
            "{what}: public spend key"
        );
    }
    if let Some(seed) = exp["mnemonicSeed"].as_str() {
        let secret = xkr_wallet_core::mnemonic::secret_from_mnemonic(seed).unwrap();
        assert_eq!(first.spend_secret_key, Some(secret), "{what}: seed");
    }
    let view_only = exp["viewOnly"].as_bool().unwrap_or(false);
    assert_eq!(wallet.is_tracking(), view_only, "{what}: view only");
}

// ------------------------------------------------------------------ address books

#[test]
fn address_books() {
    let dir = data_dir("addressbook");
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("skipped: {} not present", dir.display());
        return;
    };
    let mut seen = 0;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = fs::read(&path).unwrap();
        let book = AddressBook::from_json(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let form = if name.starts_with("beta-") {
            AddressBookForm::ZedwalletPlusPlus
        } else if name.starts_with("xkrwallet-") {
            AddressBookForm::Zedwallet
        } else {
            panic!("{name}: unknown tool prefix");
        };
        assert_eq!(book.form, form, "{name}");
        assert!(!book.entries.is_empty(), "{name}: no entries");
        assert!(
            book.to_json() == bytes,
            "{name}: not written back byte for byte"
        );
        seen += 1;
    }
    if seen == 0 {
        eprintln!("skipped: {} has no address books", dir.display());
    }
}
