//! Shared helpers of the fork-14 test-vector tests (`docs/tx-v2.md` §7): where the JSON
//! files live, the `XKR_WRITE_VECTORS=1` gate, stable rendering, the fixed secrets and a
//! scripted RNG that feeds explicit scalars to the `rng` APIs.
//!
//! Included by `crates/xkr-wallet-core/tests/tx_v2_*.rs` and, through `#[path]`, by
//! `crates/xkr-rpc/tests/tx_v2_walletsync/main.rs`.
#![allow(dead_code)] // every test file uses a different subset

use std::path::PathBuf;
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use xkr_crypto::{hash_to_scalar, secret_key_to_public_key};
use xkr_types::{EcScalar, HexError, PublicKey, SecretKey};
use xkr_wallet_core::{ScanStats, WalletKeys};

/// Set to `1` to (re)write the vector files instead of checking them.
pub const WRITE_ENV: &str = "XKR_WRITE_VECTORS";

/// Prefix of every label hashed into a secret ([`secret`]).
pub const SECRET_DOMAIN: &str = "xkr tx-v2 vectors ";

/// Whether this run writes the vector files.
pub fn writing() -> bool {
    std::env::var(WRITE_ENV).is_ok_and(|v| v == "1")
}

/// `docs/test-vectors/<file>` of the repository.
pub fn path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/test-vectors")
        .join(file)
}

/// Sorts the keys of every JSON object (whatever the `preserve_order` feature of `serde_json`
/// says).
pub fn canonical(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = serde_json::Map::new();
            for (key, item) in entries {
                out.insert(key, canonical(item));
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// The file text: sorted keys, two-space indentation, trailing newline.
pub fn render<T: Serialize>(vectors: &T) -> String {
    let value = serde_json::to_value(vectors).expect("vectors serialize");
    let mut text = serde_json::to_string_pretty(&canonical(value)).expect("json renders");
    text.push('\n');
    text
}

fn parse<T: DeserializeOwned>(file: &str, text: &str) -> T {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{file} does not parse: {e}"))
}

/// Body of a generator test: nothing unless `XKR_WRITE_VECTORS=1`; then writes `build()` to
/// `docs/test-vectors/<file>` and returns what was written, parsed, for verification.
pub fn generate<T: Serialize + DeserializeOwned>(
    file: &str,
    build: impl FnOnce() -> T,
) -> Option<T> {
    if !writing() {
        return None;
    }
    let text = render(&build());
    std::fs::write(path(file), &text).unwrap_or_else(|e| panic!("cannot write {file}: {e}"));
    Some(parse(file, &text))
}

/// Body of a verification test: nothing while writing (the generator verifies what it writes);
/// otherwise reads the file, requires it to be byte-identical to a fresh `build()` and returns
/// it parsed.
pub fn load<T: Serialize + DeserializeOwned>(file: &str, build: impl FnOnce() -> T) -> Option<T> {
    if writing() {
        return None;
    }
    let text = std::fs::read_to_string(path(file)).unwrap_or_else(|e| {
        panic!("docs/test-vectors/{file} is missing ({e}); generate it with {WRITE_ENV}=1")
    });
    let fresh = render(&build());
    assert!(
        text == fresh,
        "docs/test-vectors/{file} differs from what the generator produces; regenerate it with \
         {WRITE_ENV}=1 (see docs/tx-v2.md §7)"
    );
    Some(parse(file, &text))
}

/// `H_s("xkr tx-v2 vectors " || label)`: every secret scalar of the vectors.
pub fn secret(label: &str) -> SecretKey {
    let mut data = SECRET_DOMAIN.as_bytes().to_vec();
    data.extend_from_slice(label.as_bytes());
    SecretKey(hash_to_scalar(&data).to_bytes())
}

/// [`secret`] as a public scalar (signing randomness).
pub fn scalar(label: &str) -> EcScalar {
    EcScalar(secret(label).0)
}

/// `secret * G`.
pub fn public(secret: &SecretKey) -> PublicKey {
    secret_key_to_public_key(secret).expect("reduced scalars are canonical")
}

/// Parses a hex field into one of the byte newtypes.
pub fn from_hex<T: FromStr<Err = HexError>>(text: &str) -> T {
    text.parse()
        .unwrap_or_else(|e| panic!("bad hex field {text:?}: {e}"))
}

/// One byte as two lowercase hex digits (view tags).
pub fn byte_hex(byte: u8) -> String {
    hex::encode([byte])
}

/// A wallet with every key, as the vector files list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Wallet {
    /// Name the cases refer to.
    pub name: String,
    /// Private spend key `b`.
    pub spend_secret: String,
    /// Private view key `a`.
    pub view_secret: String,
    /// `B = b*G`.
    pub spend_public: String,
    /// `A = a*G`.
    pub view_public: String,
    /// Standard `SEKR…` address.
    pub address: String,
}

impl Wallet {
    /// The JSON form of `keys`.
    pub fn new(name: &str, keys: &WalletKeys) -> Self {
        Self {
            name: name.to_string(),
            spend_secret: keys.spend_secret.to_hex(),
            view_secret: keys.view_secret.to_hex(),
            spend_public: keys.spend_public.to_hex(),
            view_public: keys.view_public.to_hex(),
            address: keys.address().to_string(),
        }
    }

    /// The keys from the secrets; checks the public keys and the address against them.
    pub fn keys(&self) -> WalletKeys {
        let spend: SecretKey = from_hex(&self.spend_secret);
        let view: SecretKey = from_hex(&self.view_secret);
        let keys = WalletKeys::from_secrets(&spend, &view).expect("canonical wallet secrets");
        assert_eq!(
            keys.spend_public.to_hex(),
            self.spend_public,
            "{}",
            self.name
        );
        assert_eq!(keys.view_public.to_hex(), self.view_public, "{}", self.name);
        assert_eq!(keys.address().to_string(), self.address, "{}", self.name);
        keys
    }
}

/// [`ScanStats`] in the files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Stats {
    /// Outputs looked at.
    pub outputs: u64,
    /// Outputs with a view tag.
    pub tagged: u64,
    /// Tagged outputs skipped because the tag did not match.
    pub skipped_by_tag: u64,
    /// Outputs that needed `derive_public_key`.
    pub full_checks: u64,
}

impl From<ScanStats> for Stats {
    fn from(s: ScanStats) -> Self {
        Self {
            outputs: s.outputs,
            tagged: s.tagged,
            skipped_by_tag: s.skipped_by_tag,
            full_checks: s.full_checks,
        }
    }
}

/// An RNG that feeds explicit scalars to `random_scalar` (`xkr_crypto::ops`): each draw is
/// 64 bytes `s || 0^32`, whose wide reduction is `s` itself for a canonical `s`. Every random
/// value of `build_transaction`, `generate_keys`, `generate_ring_signature` and `clsag_sign` is
/// such a draw, so this supplies their randomness in draw order. Anything else panics.
#[derive(Debug)]
pub struct ScriptedRng {
    draws: Vec<[u8; 32]>,
    next: usize,
}

impl ScriptedRng {
    /// Feeds `draws` (canonical scalars) in order.
    pub fn new(draws: &[EcScalar]) -> Self {
        Self {
            draws: draws.iter().map(|s| s.0).collect(),
            next: 0,
        }
    }

    /// Feeds hex scalars.
    pub fn from_hex(draws: &[String]) -> Self {
        let scalars: Vec<EcScalar> = draws.iter().map(|d| from_hex(d.as_str())).collect();
        Self::new(&scalars)
    }

    /// Panics unless every scalar was drawn.
    pub fn assert_consumed(&self) {
        assert_eq!(
            self.next,
            self.draws.len(),
            "not every scripted draw was used"
        );
    }
}

impl rand_core::RngCore for ScriptedRng {
    fn next_u32(&mut self) -> u32 {
        panic!("ScriptedRng only serves random_scalar draws")
    }

    fn next_u64(&mut self) -> u64 {
        panic!("ScriptedRng only serves random_scalar draws")
    }

    fn fill_bytes(&mut self, dst: &mut [u8]) {
        assert_eq!(dst.len(), 64, "ScriptedRng only serves random_scalar draws");
        let Some(s) = self.draws.get(self.next) else {
            panic!("ScriptedRng exhausted after {} draws", self.next);
        };
        dst[..32].copy_from_slice(s);
        dst[32..].fill(0);
        self.next += 1;
    }
}

impl rand_core::CryptoRng for ScriptedRng {}
