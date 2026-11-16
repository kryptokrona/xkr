//! Vector B of `docs/tx-v2.md` §7, `docs/test-vectors/tx-v2-layout.json`: the structural
//! version-2 fixtures of `crates/xkr-serialization/src/tx.rs` (`v2_spend`, the v2 coinbase) as
//! hex, and blobs the decoder must refuse, each with the Rust error.
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_layout` writes the file;
//! without the variable the file is checked against the Rust implementation.

mod vectors;

use serde::{Deserialize, Serialize};
use xkr_serialization::{
    DecodeError, decode_transaction, encode_transaction, encode_transaction_prefix,
    transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_types::{
    ClsagSignature, EcScalar, KeyImage, PublicKey, Transaction, TransactionPrefix, TxInput,
    TxOutput, TxOutputTarget,
};

const FILE: &str = "tx-v2-layout.json";
const DESCRIPTION: &str = "Fork-14 transaction layout (docs/tx-v2.md §2): the v2_spend \
    and v2 coinbase fixtures of crates/xkr-serialization/src/tx.rs (dummy keys and scalars, \
    not valid signatures) and blobs every decoder must refuse; error is the \
    Rust DecodeError variant, message its text. Generated and checked by \
    crates/xkr-wallet-core/tests/tx_v2_layout.rs.";

#[derive(Debug, Serialize, Deserialize)]
struct LayoutVectors {
    description: String,
    valid: Vec<ValidBlob>,
    invalid: Vec<InvalidBlob>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidBlob {
    name: String,
    version: u8,
    /// The prefix bytes (everything the signatures sign).
    prefix: String,
    /// The whole transaction.
    blob: String,
    /// `cn_fast_hash(prefix)`.
    prefix_hash: String,
    /// `cn_fast_hash(blob)`.
    hash: String,
    /// Bytes after the prefix: `32 * (ring size + 1)` per input.
    signature_section_len: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct InvalidBlob {
    name: String,
    blob: String,
    /// `DecodeError` variant.
    error: String,
    /// Its `Display` text.
    message: String,
}

/// `v2_spend()` of `crates/xkr-serialization/src/tx.rs`: two inputs with rings 3 and 1, two
/// tagged outputs, a 33-byte extra and dummy scalars.
fn v2_spend() -> Transaction {
    let key = |b: u8| PublicKey([b; 32]);
    Transaction {
        prefix: TransactionPrefix {
            version: 2,
            unlock_time: 0,
            inputs: vec![
                TxInput::Key {
                    amount: 100,
                    output_indexes: vec![7, 1, 2],
                    key_image: KeyImage([0x11; 32]),
                },
                TxInput::Key {
                    amount: 1_000,
                    output_indexes: vec![300],
                    key_image: KeyImage([0x22; 32]),
                },
            ],
            outputs: vec![
                TxOutput {
                    amount: 90,
                    target: TxOutputTarget::TaggedKey {
                        key: key(0x33),
                        view_tag: 0xab,
                    },
                },
                TxOutput {
                    amount: 1_000,
                    target: TxOutputTarget::TaggedKey {
                        key: key(0x44),
                        view_tag: 0x00,
                    },
                },
            ],
            extra: vec![0x01; 33],
        },
        signatures: Vec::new(),
        clsag_signatures: vec![
            ClsagSignature {
                s: vec![EcScalar([1; 32]), EcScalar([2; 32]), EcScalar([3; 32])],
                c1: EcScalar([4; 32]),
            },
            ClsagSignature {
                s: vec![EcScalar([5; 32])],
                c1: EcScalar([6; 32]),
            },
        ],
    }
}

/// The version-2 coinbase of `v2_coinbase_roundtrip` in `tx.rs` (no signatures).
fn v2_coinbase() -> Transaction {
    Transaction {
        prefix: TransactionPrefix {
            version: 2,
            unlock_time: 30,
            inputs: vec![TxInput::Coinbase { height: 10 }],
            outputs: vec![TxOutput {
                amount: 2_980_232,
                target: TxOutputTarget::TaggedKey {
                    key: PublicKey([9; 32]),
                    view_tag: 0x5a,
                },
            }],
            extra: vec![],
        },
        ..Transaction::default()
    }
}

fn fixture(name: &str) -> Transaction {
    match name {
        "v2_spend" => v2_spend(),
        "v2_coinbase" => v2_coinbase(),
        other => panic!("unknown fixture {other}"),
    }
}

fn encode(tx: &Transaction) -> Vec<u8> {
    encode_transaction(tx).expect("fixture encodes")
}

/// The blobs to refuse, in file order.
fn invalid_blobs() -> Vec<(String, Vec<u8>)> {
    let tx = v2_spend();
    let blob = encode(&tx);
    let prefix_len = encode_transaction_prefix(&tx.prefix).len();
    let signatures = &blob[prefix_len..];
    let mut out = Vec::new();

    // Tag 0x03 outputs under version 1 (UnknownOutputTag).
    let mut v1 = tx.prefix.clone();
    v1.version = 1;
    let mut b = encode_transaction_prefix(&v1);
    b.extend_from_slice(signatures);
    out.push(("tagged_output_in_version_1".to_string(), b));

    // Tag 0x02 outputs under version 2 (WrongTransactionVersion(2)).
    let mut untagged = tx.prefix.clone();
    for o in &mut untagged.outputs {
        let key = *o.key();
        o.target = TxOutputTarget::Key(key);
    }
    let mut b = encode_transaction_prefix(&untagged);
    b.extend_from_slice(signatures);
    out.push(("untagged_output_in_version_2".to_string(), b));

    // Version 3.
    let mut b = blob.clone();
    b[0] = 3;
    out.push(("version_3".to_string(), b));

    // A coinbase input next to a key input, followed by 4 dummy scalars.
    let mut mixed = tx.prefix.clone();
    mixed.inputs[1] = TxInput::Coinbase { height: 1 };
    let mut b = encode_transaction_prefix(&mixed);
    b.extend_from_slice(&[0u8; 4 * 32]);
    out.push(("coinbase_input_next_to_key_input".to_string(), b));

    // One extra byte after a valid blob.
    let mut b = blob.clone();
    b.push(0);
    out.push(("trailing_byte".to_string(), b));

    // Every truncation of the valid blob, including the empty one.
    for cut in 0..blob.len() {
        out.push((format!("truncated_{cut}"), blob[..cut].to_vec()));
    }
    out
}

/// The variant name of a decode error (`UnexpectedEof { .. }` -> `UnexpectedEof`).
fn error_name(e: &DecodeError) -> String {
    let debug = format!("{e:?}");
    debug
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string()
}

fn build() -> LayoutVectors {
    let valid = ["v2_spend", "v2_coinbase"]
        .into_iter()
        .map(|name| {
            let tx = fixture(name);
            let blob = encode(&tx);
            ValidBlob {
                name: name.to_string(),
                version: tx.prefix.version,
                prefix: hex::encode(encode_transaction_prefix(&tx.prefix)),
                blob: hex::encode(&blob),
                prefix_hash: transaction_prefix_hash(&tx.prefix).to_hex(),
                hash: transaction_hash_from_blob(&blob).to_hex(),
                signature_section_len: tx.signature_section_len(),
            }
        })
        .collect();
    let invalid = invalid_blobs()
        .into_iter()
        .map(|(name, blob)| {
            let e = decode_transaction(&blob).expect_err("an invalid blob must not decode");
            InvalidBlob {
                name,
                blob: hex::encode(blob),
                error: error_name(&e),
                message: e.to_string(),
            }
        })
        .collect();
    LayoutVectors {
        description: DESCRIPTION.to_string(),
        valid,
        invalid,
    }
}

fn verify(v: &LayoutVectors) {
    let names: Vec<&str> = v.valid.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["v2_spend", "v2_coinbase"]);
    for b in &v.valid {
        let blob = hex::decode(&b.blob).expect("hex");
        let tx = decode_transaction(&blob).expect("valid blob decodes");
        assert_eq!(tx, fixture(&b.name), "{}", b.name);
        assert_eq!(encode(&tx), blob, "{} re-encodes", b.name);
        assert_eq!(tx.prefix.version, b.version);
        let prefix = encode_transaction_prefix(&tx.prefix);
        assert_eq!(hex::encode(&prefix), b.prefix);
        assert_eq!(&blob[..prefix.len()], prefix.as_slice());
        assert_eq!(blob.len() - prefix.len(), b.signature_section_len);
        assert_eq!(tx.signature_section_len(), b.signature_section_len);
        assert_eq!(transaction_prefix_hash(&tx.prefix).to_hex(), b.prefix_hash);
        assert_eq!(transaction_hash_from_blob(&blob).to_hex(), b.hash);
    }
    let spend_len = hex::decode(&v.valid[0].blob).expect("hex").len();
    let truncations = v
        .invalid
        .iter()
        .filter(|b| b.name.starts_with("truncated_"))
        .count();
    assert_eq!(truncations, spend_len);
    assert_eq!(v.invalid.len(), spend_len + 5);
    for b in &v.invalid {
        let blob = hex::decode(&b.blob).expect("hex");
        let e = decode_transaction(&blob).expect_err(&b.name);
        assert_eq!(error_name(&e), b.error, "{}", b.name);
        assert_eq!(e.to_string(), b.message, "{}", b.name);
    }
    let error_of = |name: &str| {
        let b = v.invalid.iter().find(|b| b.name == name).expect(name);
        b.error.as_str()
    };
    assert_eq!(error_of("tagged_output_in_version_1"), "UnknownOutputTag");
    assert_eq!(
        error_of("untagged_output_in_version_2"),
        "WrongTransactionVersion"
    );
    assert_eq!(error_of("version_3"), "WrongTransactionVersion");
    assert_eq!(
        error_of("coinbase_input_next_to_key_input"),
        "CoinbaseInputInSignedTransaction"
    );
    assert_eq!(error_of("trailing_byte"), "TrailingBytes");
}

#[test]
fn write_layout_vectors() {
    if let Some(v) = vectors::generate(FILE, build) {
        verify(&v);
    }
}

#[test]
fn layout_vectors() {
    if let Some(v) = vectors::load(FILE, build) {
        verify(&v);
    }
}
