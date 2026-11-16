//! Vector A of `docs/tx-v2.md` §7, `docs/test-vectors/tx-v2-view-tags.json`: the 70
//! Monero `derive_view_tag` vectors plus XKR wallets, transaction keys, derivations (sender and
//! recipient side) and view tags at the varint length boundaries.
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_view_tags` writes the file;
//! without the variable the file is checked against the Rust implementation.

mod vectors;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use vectors::{Wallet, byte_hex, from_hex, public, secret};
use xkr_crypto::{generate_key_derivation, secret_key_to_public_key, view_tag};
use xkr_serialization::varint::write_varint;
use xkr_types::{KeyDerivation, SecretKey};
use xkr_wallet_core::WalletKeys;

const FILE: &str = "tx-v2-view-tags.json";
const DESCRIPTION: &str = "Fork-14 view tags (docs/tx-v2.md §3): \
    view_tag(D, i) = cn_fast_hash(\"view_tag\" || D || varint(i))[0]. Secrets are \
    H_s(\"xkr tx-v2 vectors \" || label). Generated and checked by \
    crates/xkr-wallet-core/tests/tx_v2_view_tags.rs.";
const MONERO: &str = include_str!("data/view_tag_vectors.txt");
/// Output indexes around the one-, two- and three-byte varint boundaries.
const INDEXES: [u64; 9] = [0, 1, 2, 127, 128, 255, 256, 16383, 16384];

#[derive(Debug, Serialize, Deserialize)]
struct ViewTagVectors {
    description: String,
    /// The 8 salt bytes `"view_tag"`.
    salt: String,
    monero: Vec<MoneroVector>,
    wallets: Vec<Wallet>,
    transactions: Vec<TxVector>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MoneroVector {
    derivation: String,
    index: u64,
    view_tag: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TxVector {
    /// Recipient wallet (name in `wallets`).
    wallet: String,
    /// Transaction secret key `r`.
    tx_secret_key: String,
    /// `R = r*G` (in `extra`).
    tx_public_key: String,
    /// `D = 8*r*A` = `8*a*R`.
    derivation: String,
    tags: Vec<TagVector>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TagVector {
    index: u64,
    /// `varint(index)` as hashed.
    index_varint: String,
    view_tag: String,
}

fn wallets() -> Vec<(&'static str, WalletKeys)> {
    // A deterministic wallet (view key from the spend key) and one with an independent view key.
    let first = WalletKeys::from_spend_secret(&secret("view tags wallet 1 spend"))
        .expect("reduced spend secret");
    let second = WalletKeys::from_secrets(
        &secret("view tags wallet 2 spend"),
        &secret("view tags wallet 2 view"),
    )
    .expect("reduced secrets");
    vec![("wallet_1", first), ("wallet_2", second)]
}

fn varint_hex(index: u64) -> String {
    let mut out = Vec::new();
    write_varint(&mut out, index);
    hex::encode(out)
}

fn build() -> ViewTagVectors {
    let monero = MONERO
        .lines()
        .filter(|l| l.starts_with("derive_view_tag "))
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            MoneroVector {
                derivation: f[1].to_string(),
                index: f[2].parse().expect("index"),
                view_tag: f[3].to_string(),
            }
        })
        .collect();
    let wallets = wallets();
    let mut transactions = Vec::new();
    for (name, keys) in &wallets {
        for k in 1..=3 {
            let r = secret(&format!("view tags tx key {k}"));
            let d = generate_key_derivation(&keys.view_public, &r).expect("valid view key");
            let tags = INDEXES
                .iter()
                .map(|&index| TagVector {
                    index,
                    index_varint: varint_hex(index),
                    view_tag: byte_hex(view_tag(&d, index)),
                })
                .collect();
            transactions.push(TxVector {
                wallet: (*name).to_string(),
                tx_secret_key: r.to_hex(),
                tx_public_key: public(&r).to_hex(),
                derivation: d.to_hex(),
                tags,
            });
        }
    }
    ViewTagVectors {
        description: DESCRIPTION.to_string(),
        salt: hex::encode(b"view_tag"),
        monero,
        wallets: wallets.iter().map(|(n, k)| Wallet::new(n, k)).collect(),
        transactions,
    }
}

fn verify(v: &ViewTagVectors) {
    assert_eq!(v.salt, "766965775f746167");
    assert_eq!(v.monero.len(), 70);
    for m in &v.monero {
        let d: KeyDerivation = from_hex(&m.derivation);
        let got = byte_hex(view_tag(&d, m.index));
        assert_eq!(got, m.view_tag, "{} {}", m.derivation, m.index);
    }
    let wallets: HashMap<&str, WalletKeys> = v
        .wallets
        .iter()
        .map(|w| (w.name.as_str(), w.keys()))
        .collect();
    assert_eq!(wallets.len(), 2);
    assert_eq!(v.transactions.len(), 6);
    for t in &v.transactions {
        let keys = &wallets[t.wallet.as_str()];
        let r: SecretKey = from_hex(&t.tx_secret_key);
        let tx_public = secret_key_to_public_key(&r).expect("canonical r");
        assert_eq!(tx_public.to_hex(), t.tx_public_key);
        // The sender's 8*r*A and the recipient's 8*a*R agree.
        let sender = generate_key_derivation(&keys.view_public, &r).expect("view key");
        let recipient = generate_key_derivation(&tx_public, &keys.view_secret).expect("R");
        assert_eq!(sender.to_hex(), t.derivation);
        assert_eq!(recipient, sender);
        let indexes: Vec<u64> = t.tags.iter().map(|g| g.index).collect();
        assert_eq!(indexes, INDEXES);
        for g in &t.tags {
            assert_eq!(varint_hex(g.index), g.index_varint);
            assert_eq!(
                byte_hex(view_tag(&sender, g.index)),
                g.view_tag,
                "{}",
                g.index
            );
        }
    }
}

#[test]
fn write_view_tag_vectors() {
    if let Some(v) = vectors::generate(FILE, build) {
        verify(&v);
    }
}

#[test]
fn view_tag_vectors() {
    if let Some(v) = vectors::load(FILE, build) {
        verify(&v);
    }
}
