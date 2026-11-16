//! Vector D of `docs/tx-v2.md` §7, `docs/test-vectors/tx-v2-transactions.json`: complete
//! transactions with every secret and every random value, which the JS libraries rebuild and
//! re-sign byte for byte: a version-2 spend, the same transfer as version 1, a version-2
//! coinbase and a version-2 spend of an output received in a version-1 transaction.
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_transactions` writes the
//! file; without the variable the file is checked against the Rust implementation.

#[path = "../vectors/mod.rs"]
mod vectors;

mod transfer;

use serde::{Deserialize, Serialize};
use transfer::{
    InputSpec, OutputCase, ScanCase, TransferCase, TransferSpec, Wallets, make_transfer, scan_case,
    verify_transfer,
};
use vectors::{Wallet, byte_hex, from_hex, public, secret};
use xkr_consensus::params::CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW;
use xkr_consensus::reward::{decompose_amount, default_dust_threshold};
use xkr_crypto::{cn_fast_hash, derive_public_key, generate_key_derivation, view_tag};
use xkr_serialization::{
    decode_transaction, encode_transaction, transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_types::extra::TX_EXTRA_TAG_PUBKEY;
use xkr_types::{
    Hash, PublicKey, SecretKey, Transaction, TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};
use xkr_wallet_core::{Address, TxVersion, WalletKeys};

const FILE: &str = "tx-v2-transactions.json";
const DESCRIPTION: &str = "Fork-14 transactions (docs/tx-v2.md), \
    mainnet addresses, fee 10, unlock time 0, dust threshold 0. Secrets and randomness are \
    H_s(\"xkr tx-v2 vectors \" || label). Inputs are in the caller's order (Rust keeps it; \
    kryptokrona-utils sorts by key image, so JS must take the order given here), decoys in \
    /getrandom_outs order, ring = decoys + real output sorted by global index, outputs sorted \
    by amount (each listed with its owner). randomness lists every random_scalar draw of the \
    build: r, then per input v2: alpha and the decoy responses s_{l+1}..s_{l-1}; v1: in ring \
    order k at the real index and (c_i, r_i) elsewhere. Compare extra bytes as given. \
    Generated and checked by crates/xkr-wallet-core/tests/tx_v2_transactions/.";
/// First block of mainnet fork 14 (`FORK_14_HEIGHT_MAINNET + 1`).
const COINBASE_HEIGHT: u32 = 3_000_001;
/// Base reward plus fees of that coinbase (an example value).
const COINBASE_REWARD: u64 = 2_934_212;

#[derive(Debug, Serialize, Deserialize)]
struct TransactionVectors {
    description: String,
    wallets: Vec<Wallet>,
    /// The payment id of the integrated destination.
    payment_id: String,
    v2_spend: TransferCase,
    v1_twin: TransferCase,
    v2_coinbase: CoinbaseCase,
    mixed_history: MixedCase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoinbaseCase {
    miner: String,
    address: String,
    height: u32,
    reward: u64,
    dust_threshold: u64,
    /// `decompose_amount(reward, dust_threshold)`, the output amounts in order.
    denominations: Vec<u64>,
    unlock_time: u64,
    tx_secret_key: String,
    tx_public_key: String,
    extra: String,
    outputs: Vec<OutputCase>,
    prefix_hash: String,
    blob: String,
    hash: String,
    scan: ScanCase,
}

/// A version-1 transaction paying `holder`, and a version-2 transaction spending that output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MixedCase {
    funding: TransferCase,
    spend: TransferCase,
    /// Index of the holder's output in `funding`.
    output_index: usize,
    /// Its key image: the holder's scan of `funding` and the input of `spend` agree.
    key_image: String,
}

const WALLETS: [&str; 6] = [
    "sender",
    "recipient",
    "integrated_recipient",
    "miner",
    "funder",
    "holder",
];

fn wallets() -> Wallets {
    WALLETS
        .iter()
        .map(|name| {
            let spend = secret(&format!("tx {name} spend"));
            let keys = WalletKeys::from_spend_secret(&spend).expect("reduced secret");
            ((*name).to_string(), keys)
        })
        .collect()
}

fn payment_id() -> Hash {
    cn_fast_hash(b"xkr tx-v2 vectors payment id")
}

fn address(wallets: &Wallets, name: &str) -> Address {
    wallets[name].address()
}

/// A decoy `(global index, key)` of `amount`.
fn decoy(amount: u64, global_index: u32) -> (u32, PublicKey) {
    let key = public(&secret(&format!("decoy {amount} {global_index}")));
    (global_index, key)
}

fn received(label: &str) -> PublicKey {
    public(&secret(label))
}

/// The transfer of `v2_spend` and `v1_twin`: inputs with rings 4 and 2, a standard and an
/// integrated destination, change, 64 bytes of Hugin data.
fn spend_spec(wallets: &Wallets, version: TxVersion, label: &str) -> TransferSpec {
    TransferSpec {
        label: label.to_string(),
        tx_key_label: "spend tx key".to_string(),
        version,
        inputs: vec![
            InputSpec {
                wallet: "sender".to_string(),
                received_tx_public_key: received("spend input 0 received tx key"),
                output_index: 2,
                amount: 1_000_000,
                global_index: 1999,
                decoys: vec![
                    decoy(1_000_000, 812),
                    decoy(1_000_000, 4051),
                    decoy(1_000_000, 77),
                ],
            },
            InputSpec {
                wallet: "sender".to_string(),
                received_tx_public_key: received("spend input 1 received tx key"),
                output_index: 0,
                amount: 90_000,
                global_index: 52,
                decoys: vec![decoy(90_000, 301)],
            },
        ],
        destinations: vec![
            (address(wallets, "recipient"), 400_000),
            (
                address(wallets, "integrated_recipient").integrated(payment_id()),
                123_456,
            ),
        ],
        change: address(wallets, "sender"),
        fee: 10,
        data: Some((0u8..64).collect()),
        scan: vec![
            "recipient".to_string(),
            "integrated_recipient".to_string(),
            "sender".to_string(),
        ],
    }
}

fn mixed(wallets: &Wallets) -> MixedCase {
    let funding = make_transfer(
        &TransferSpec {
            label: "mixed funding".to_string(),
            tx_key_label: "mixed funding tx key".to_string(),
            version: TxVersion::V1,
            inputs: vec![InputSpec {
                wallet: "funder".to_string(),
                received_tx_public_key: received("mixed funding input received tx key"),
                output_index: 1,
                amount: 500_000,
                global_index: 7,
                decoys: vec![decoy(500_000, 11), decoy(500_000, 3)],
            }],
            destinations: vec![(address(wallets, "holder"), 300_000)],
            change: address(wallets, "funder"),
            fee: 10,
            data: None,
            scan: vec!["holder".to_string(), "funder".to_string()],
        },
        wallets,
    );
    let holder_scan = &funding.scans[0];
    assert_eq!(holder_scan.found.len(), 1);
    let output_index = holder_scan.found[0].index;
    let amount = holder_scan.found[0].amount;
    let key_image = holder_scan.found[0].key_image.clone();
    let spend = make_transfer(
        &TransferSpec {
            label: "mixed spend".to_string(),
            tx_key_label: "mixed spend tx key".to_string(),
            version: TxVersion::V2,
            inputs: vec![InputSpec {
                wallet: "holder".to_string(),
                received_tx_public_key: from_hex(&funding.tx_public_key),
                output_index: output_index as u64,
                amount,
                global_index: 18,
                decoys: vec![decoy(300_000, 25), decoy(300_000, 4)],
            }],
            destinations: vec![(address(wallets, "recipient"), 250_000)],
            change: address(wallets, "holder"),
            fee: 10,
            data: None,
            scan: vec!["recipient".to_string(), "holder".to_string()],
        },
        wallets,
    );
    MixedCase {
        funding,
        spend,
        output_index,
        key_image,
    }
}

/// The version-2 coinbase `xkr-chain` `construct_miner_tx` builds (without an extra nonce).
fn coinbase_tx(miner: &Address, height: u32, reward: u64, r: &SecretKey) -> Transaction {
    let derivation = generate_key_derivation(&miner.view, r).expect("miner view key");
    let outputs = decompose_amount(reward, default_dust_threshold(height))
        .into_iter()
        .enumerate()
        .map(|(i, amount)| TxOutput {
            amount,
            target: TxOutputTarget::TaggedKey {
                key: derive_public_key(&derivation, i as u64, &miner.spend).expect("spend key"),
                view_tag: view_tag(&derivation, i as u64),
            },
        })
        .collect();
    let mut extra = vec![TX_EXTRA_TAG_PUBKEY];
    extra.extend_from_slice(&public(r).0);
    Transaction {
        prefix: TransactionPrefix {
            version: 2,
            unlock_time: u64::from(height) + u64::from(CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW),
            inputs: vec![TxInput::Coinbase { height }],
            outputs,
            extra,
        },
        signatures: Vec::new(),
        clsag_signatures: Vec::new(),
    }
}

fn coinbase(wallets: &Wallets) -> CoinbaseCase {
    let keys = &wallets["miner"];
    let miner = keys.address();
    let r = secret("coinbase tx key");
    let tx = coinbase_tx(&miner, COINBASE_HEIGHT, COINBASE_REWARD, &r);
    let blob = encode_transaction(&tx).expect("coinbase encodes");
    let outputs: Vec<(u64, PublicKey, Option<u8>)> = tx
        .prefix
        .outputs
        .iter()
        .map(|o| (o.amount, *o.key(), o.view_tag()))
        .collect();
    CoinbaseCase {
        miner: "miner".to_string(),
        address: miner.to_string(),
        height: COINBASE_HEIGHT,
        reward: COINBASE_REWARD,
        dust_threshold: default_dust_threshold(COINBASE_HEIGHT),
        denominations: outputs.iter().map(|o| o.0).collect(),
        unlock_time: tx.prefix.unlock_time,
        tx_secret_key: r.to_hex(),
        tx_public_key: public(&r).to_hex(),
        extra: hex::encode(&tx.prefix.extra),
        outputs: outputs
            .iter()
            .map(|o| OutputCase {
                amount: o.0,
                key: o.1.to_hex(),
                view_tag: o.2.map(byte_hex),
                recipient: "miner".to_string(),
            })
            .collect(),
        prefix_hash: transaction_prefix_hash(&tx.prefix).to_hex(),
        blob: hex::encode(&blob),
        hash: transaction_hash_from_blob(&blob).to_hex(),
        scan: scan_case("miner", keys, &public(&r), &outputs),
    }
}

fn build() -> TransactionVectors {
    let wallets = wallets();
    TransactionVectors {
        description: DESCRIPTION.to_string(),
        wallets: WALLETS
            .iter()
            .map(|name| Wallet::new(name, &wallets[*name]))
            .collect(),
        payment_id: payment_id().to_hex(),
        v2_spend: make_transfer(&spend_spec(&wallets, TxVersion::V2, "v2 spend"), &wallets),
        v1_twin: make_transfer(&spend_spec(&wallets, TxVersion::V1, "v1 twin"), &wallets),
        v2_coinbase: coinbase(&wallets),
        mixed_history: mixed(&wallets),
    }
}

fn verify_coinbase(c: &CoinbaseCase, wallets: &Wallets) {
    let keys = &wallets[c.miner.as_str()];
    let miner = Address::parse(&c.address).expect("miner address");
    assert_eq!(miner, keys.address());
    assert_eq!(c.dust_threshold, default_dust_threshold(c.height));
    assert_eq!(
        c.denominations,
        decompose_amount(c.reward, c.dust_threshold)
    );
    let r: SecretKey = from_hex(&c.tx_secret_key);
    assert_eq!(public(&r).to_hex(), c.tx_public_key);
    let rebuilt = coinbase_tx(&miner, c.height, c.reward, &r);
    let blob = hex::decode(&c.blob).expect("hex");
    assert_eq!(
        encode_transaction(&rebuilt).expect("encodes"),
        blob,
        "coinbase blob"
    );
    let tx = decode_transaction(&blob).expect("coinbase decodes");
    assert_eq!(tx, rebuilt);
    assert!(tx.is_coinbase());
    assert_eq!(tx.prefix.version, 2);
    assert_eq!(tx.signature_section_len(), 0);
    assert_eq!(tx.prefix.unlock_time, c.unlock_time);
    assert_eq!(hex::encode(&tx.prefix.extra), c.extra);
    assert_eq!(transaction_prefix_hash(&tx.prefix).to_hex(), c.prefix_hash);
    // A coinbase hash is the plain blob hash (no Monero v2 suffix).
    assert_eq!(cn_fast_hash(&blob).to_hex(), c.hash);
    let d = generate_key_derivation(&keys.view_public, &r).expect("view key");
    let outputs: Vec<(u64, PublicKey, Option<u8>)> = tx
        .prefix
        .outputs
        .iter()
        .map(|o| (o.amount, *o.key(), o.view_tag()))
        .collect();
    assert_eq!(outputs.len(), c.outputs.len());
    for (i, (o, expected)) in outputs.iter().zip(&c.outputs).enumerate() {
        assert_eq!(o.0, c.denominations[i]);
        assert_eq!(o.0, expected.amount);
        assert_eq!(o.1.to_hex(), expected.key);
        assert_eq!(o.2, Some(view_tag(&d, i as u64)), "coinbase output {i}");
        assert_eq!(o.2.map(byte_hex), expected.view_tag);
    }
    let tx_public: PublicKey = from_hex(&c.tx_public_key);
    assert_eq!(scan_case(&c.miner, keys, &tx_public, &outputs), c.scan);
    assert_eq!(c.scan.found.len(), c.outputs.len());
}

/// The twin has the same inputs, destinations, keys and output keys, only version 1.
fn verify_twin(v2: &TransferCase, v1: &TransferCase) {
    assert_eq!((v2.version, v1.version), (2, 1));
    for (a, b) in v2.inputs.iter().zip(&v1.inputs) {
        assert_eq!(
            (&a.key, &a.key_image, &a.ring, &a.decoys, a.real_index),
            (&b.key, &b.key_image, &b.ring, &b.decoys, b.real_index)
        );
    }
    assert_eq!(v2.inputs.len(), v1.inputs.len());
    assert_eq!(v2.destinations, v1.destinations);
    assert_eq!(v2.change_address, v1.change_address);
    assert_eq!(v2.tx_secret_key, v1.tx_secret_key);
    assert_eq!(v2.extra, v1.extra);
    assert_eq!(v2.outputs.len(), v1.outputs.len());
    for (a, b) in v2.outputs.iter().zip(&v1.outputs) {
        assert_eq!(
            (a.amount, &a.key, &a.recipient),
            (b.amount, &b.key, &b.recipient)
        );
        assert!(a.view_tag.is_some() && b.view_tag.is_none());
    }
}

fn verify_mixed(m: &MixedCase, wallets: &Wallets) {
    verify_transfer("mixed_history.funding", &m.funding, wallets);
    verify_transfer("mixed_history.spend", &m.spend, wallets);
    assert_eq!((m.funding.version, m.spend.version), (1, 2));
    let holder = m.funding.scans.iter().find(|s| s.wallet == "holder");
    let found = &holder.expect("holder scan").found;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].index, m.output_index);
    assert_eq!(found[0].key_image, m.key_image);
    let input = &m.spend.inputs[0];
    assert_eq!(input.received.wallet, "holder");
    assert_eq!(input.received.tx_public_key, m.funding.tx_public_key);
    assert_eq!(input.received.output_index, m.output_index as u64);
    assert_eq!(input.key, m.funding.outputs[m.output_index].key);
    assert_eq!(input.key_image, m.key_image);
}

fn verify(v: &TransactionVectors) {
    let wallets: Wallets = v
        .wallets
        .iter()
        .map(|w| (w.name.clone(), w.keys()))
        .collect();
    assert_eq!(wallets.len(), WALLETS.len());
    assert_eq!(v.payment_id, payment_id().to_hex());
    verify_transfer("v2_spend", &v.v2_spend, &wallets);
    verify_transfer("v1_twin", &v.v1_twin, &wallets);
    verify_twin(&v.v2_spend, &v.v1_twin);
    let rings: Vec<usize> = v.v2_spend.inputs.iter().map(|i| i.ring.len()).collect();
    assert_eq!(rings, [4, 2]);
    assert!(v.v2_spend.hugin_data.is_some());
    assert!(v.v2_spend.destinations[1].payment_id.is_some());
    verify_coinbase(&v.v2_coinbase, &wallets);
    verify_mixed(&v.mixed_history, &wallets);
}

#[test]
fn write_transaction_vectors() {
    if let Some(v) = vectors::generate(FILE, build) {
        verify(&v);
    }
}

#[test]
fn transaction_vectors() {
    if let Some(v) = vectors::load(FILE, build) {
        verify(&v);
    }
}
