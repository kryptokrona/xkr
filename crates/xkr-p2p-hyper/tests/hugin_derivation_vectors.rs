//! Hugin topic/`mid` derivation vectors in `docs/test-vectors/hugin-relay.json`, key
//! `hugin_derivation` (the domain-separated derivation of hugin-p2p 3.0): for the three
//! hugin-node topics the key, topic key, base public key and topic, and for one fixed DHT key
//! pair (seed = 32 bytes of `0x07`) the `mid` each topic's base key signs for it
//! (`crates/xkr-p2p-hyper/src/topic.rs`, "Hugin topics"). The bridge checks the same values
//! with libsodium and its own Keccak (`bridge/test/mid.test.js`), and so can `hugin-p2p`.
//!
//! The rest of `hugin-relay.json` comes from `bridge/test/hugin-vectors.js` (Hugin's own
//! packages); this test owns only the `hugin_derivation` key, which is kept as the last key of
//! the file (`hugin-vectors.js` carries it over when it regenerates the other sections).
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-p2p-hyper --test hugin_derivation_vectors`
//! (re)writes the section and checks what it wrote; without the variable the test checks the
//! file.

use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use xkr_crypto::{EdwardsPoint, Scalar};
use xkr_p2p_hyper::HuginTopic;
use xkr_p2p_hyper::topic::{self, HUGIN_MID_DOMAIN, HUGIN_TOPIC_DOMAIN, hugin_mid_message};

const FILE: &str = "hugin-relay.json";
const KEY: &str = "hugin_derivation";
/// How the `hugin_derivation` key starts in the file (two-space indentation, last key).
const MARKER: &str = ",\n  \"hugin_derivation\": ";
const WRITE_ENV: &str = "XKR_WRITE_VECTORS";

/// The private-node vector uses this XKR address as its key.
const PRIVATE_ADDRESS: &str = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";

/// Seed of the fixed DHT key pair the `mid`s are made for.
const DHT_SEED: [u8; 32] = [0x07; 32];

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/test-vectors")
        .join(FILE)
}

fn writing() -> bool {
    std::env::var(WRITE_ENV).is_ok_and(|v| v == "1")
}

/// RFC 8032 ed25519 signature from a 32-byte seed, what libsodium's `crypto_sign_detached` gives
/// for the secret key of `crypto_sign_seed_keypair(seed)`.
fn ed25519_sign(seed: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let h = Sha512::digest(seed);
    let mut clamped = [0u8; 32];
    clamped.copy_from_slice(&h[..32]);
    clamped[0] &= 0xf8;
    clamped[31] &= 0x7f;
    clamped[31] |= 0x40;
    let a = Scalar::from_bytes_mod_order(clamped);
    let public = EdwardsPoint::mul_base(&a).compress().to_bytes();
    assert_eq!(public, topic::ed25519_public_from_seed(seed));

    let mut wide = [0u8; 64];
    let nonce_hash = Sha512::new()
        .chain_update(&h[32..])
        .chain_update(msg)
        .finalize();
    wide.copy_from_slice(&nonce_hash[..]);
    let r = Scalar::from_bytes_mod_order_wide(&wide);
    let big_r = EdwardsPoint::mul_base(&r).compress().to_bytes();
    let challenge = Sha512::new()
        .chain_update(big_r)
        .chain_update(public)
        .chain_update(msg)
        .finalize();
    wide.copy_from_slice(&challenge[..]);
    let k = Scalar::from_bytes_mod_order_wide(&wide);
    let s = r + k * a;
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(s.as_bytes());
    sig
}

fn topic_entry(name: &str, t: &HuginTopic, dht_public_key: &[u8; 32]) -> Value {
    let message = hugin_mid_message(dht_public_key);
    json!({
        "name": name,
        "key": t.key,
        "topic_key": hex::encode(t.topic_key),
        "base_public_key": hex::encode(t.base_public_key),
        "topic": hex::encode(t.topic),
        "mid_message": hex::encode(message),
        "mid": hex::encode(ed25519_sign(&t.topic_key, &message)),
    })
}

fn build() -> Value {
    let dht_public_key = topic::ed25519_public_from_seed(&DHT_SEED);
    json!({
        "description": "Hugin topic and mid derivation of hugin-p2p 3.0 \
            (crates/xkr-p2p-hyper/src/topic.rs), written by \
            crates/xkr-p2p-hyper/tests/hugin_derivation_vectors.rs. topic = \
            cn_fast_hash(topic_domain || base_public_key); mid = ed25519 detached signature by \
            the base secret key (seed topic_key) over mid_domain || dht.public_key.",
        "topic_domain": HUGIN_TOPIC_DOMAIN,
        "mid_domain": HUGIN_MID_DOMAIN,
        "dht": {
            "seed": hex::encode(DHT_SEED),
            "public_key": hex::encode(dht_public_key),
        },
        "topics": [
            topic_entry("node mesh", &HuginTopic::node_mesh(), &dht_public_key),
            topic_entry("public nodes", &HuginTopic::public_nodes(), &dht_public_key),
            topic_entry(
                "private node",
                &HuginTopic::private_node(PRIVATE_ADDRESS),
                &dht_public_key,
            ),
        ],
    })
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("docs/test-vectors/{FILE}: {e}"))
}

/// Every top-level entry except `hugin_derivation`.
fn others(v: &Value) -> Vec<(String, Value)> {
    v.as_object()
        .expect("top-level object")
        .iter()
        .filter(|(k, _)| k.as_str() != KEY)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `text` with its `hugin_derivation` entry replaced by (or extended with) `section`, as the last
/// key. Only that entry is touched, so the sections `hugin-vectors.js` writes keep their bytes.
fn splice(text: &str, section: &Value) -> String {
    let head = if let Some(i) = text.rfind(MARKER) {
        &text[..i]
    } else {
        text.trim_end()
            .strip_suffix('}')
            .expect("the file ends with the closing brace of an object")
            .trim_end()
    };
    let pretty = serde_json::to_string_pretty(section)
        .expect("section renders")
        .replace('\n', "\n  ");
    format!("{head}{MARKER}{pretty}\n}}\n")
}

#[test]
fn hugin_derivation_vectors() {
    let file = path();
    let text = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("cannot read docs/test-vectors/{FILE}: {e}"));
    let section = build();
    if writing() {
        let before = parse(&text);
        let new_text = splice(&text, &section);
        std::fs::write(&file, &new_text)
            .unwrap_or_else(|e| panic!("cannot write docs/test-vectors/{FILE}: {e}"));
        let after = parse(&new_text);
        assert_eq!(
            others(&after),
            others(&before),
            "other sections must not change"
        );
        assert_eq!(after[KEY], section);
        assert!(new_text.ends_with("\n}\n"));
    } else {
        let v = parse(&text);
        assert!(
            v[KEY] == section,
            "docs/test-vectors/{FILE} has no current `{KEY}` section; regenerate it with \
             {WRITE_ENV}=1 cargo test -p xkr-p2p-hyper --test hugin_derivation_vectors"
        );
    }
}

/// The values in the section are the rule spelled out, not just what the code gives.
#[test]
fn hugin_derivation_section_follows_the_spec() {
    let v = build();
    assert_eq!(v["topic_domain"], "hugin/topic");
    assert_eq!(v["mid_domain"], "hugin/mid");
    let dht = topic::ed25519_public_from_seed(&DHT_SEED);
    assert_eq!(v["dht"]["public_key"], hex::encode(dht));
    let topics = v["topics"].as_array().expect("topics");
    assert_eq!(topics.len(), 3);
    for t in topics {
        let base = hex::decode(t["base_public_key"].as_str().expect("base")).expect("hex");
        let mut data = b"hugin/topic".to_vec();
        data.extend_from_slice(&base);
        assert_eq!(
            t["topic"],
            hex::encode(xkr_crypto::cn_fast_hash(&data).0),
            "{}",
            t["name"]
        );
        let mut message = b"hugin/mid".to_vec();
        message.extend_from_slice(&dht);
        assert_eq!(t["mid_message"], hex::encode(&message));
        // the topic base key pair is the seed `topic_key`
        let seed: [u8; 32] = hex::decode(t["topic_key"].as_str().expect("seed"))
            .expect("hex")
            .try_into()
            .expect("32 bytes");
        assert_eq!(
            hex::encode(topic::ed25519_public_from_seed(&seed)),
            t["base_public_key"]
        );
    }
}

#[test]
fn splice_keeps_other_sections_and_replaces_the_section() {
    let original = "{\n  \"a\": [\n    1\n  ],\n  \"b\": {\n    \"c\": \"d\"\n  }\n}\n";
    let once = splice(original, &json!({ "x": 1 }));
    assert_eq!(
        once,
        concat!(
            "{\n  \"a\": [\n    1\n  ],\n  \"b\": {\n    \"c\": \"d\"\n  },\n",
            "  \"hugin_derivation\": {\n    \"x\": 1\n  }\n}\n"
        )
    );
    let twice = splice(&once, &json!({ "x": 2 }));
    assert_eq!(twice, once.replace("\"x\": 1", "\"x\": 2"));
    assert_eq!(parse(&twice)["b"]["c"], "d");
}
