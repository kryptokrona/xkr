//! The transfer cases of vector D: their JSON shape, how they are generated with explicit
//! randomness (`ScriptedRng` behind `build_transaction`) and how a file is checked.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use xkr_crypto::{
    check_ring_signature, clsag_sign_with_randomness, clsag_verify, derive_public_key,
    derive_secret_key, generate_key_derivation, generate_key_image, view_tag,
};
use xkr_serialization::{
    decode_transaction, encode_transaction, encode_transaction_prefix, transaction_hash_from_blob,
    transaction_prefix_hash,
};
use xkr_types::{
    ClsagSignature, EcScalar, KeyImage, PublicKey, SecretKey, Signature, TxInput,
    absolute_to_relative, tx_public_key,
};
use xkr_wallet_core::{
    Address, Destination, InputSource, SpendKeys, TransferOptions, TxVersion, WalletKeys,
    build_transaction, scan_for_subwallets, scan_tagged_outputs_with_stats,
};

use crate::vectors::{ScriptedRng, Stats, byte_hex, from_hex, public, scalar, secret};

/// Wallet keys by name.
pub type Wallets = HashMap<String, WalletKeys>;

/// One signed transfer and everything needed to rebuild it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferCase {
    pub version: u8,
    /// In transaction order.
    pub inputs: Vec<InputCase>,
    /// In the order given to `build_transaction` (outputs are sorted by amount afterwards).
    pub destinations: Vec<DestinationCase>,
    pub change_address: String,
    pub fee: u64,
    pub unlock_time: u64,
    pub dust_threshold: u64,
    /// Hugin data in the extra nonce, if any.
    pub hugin_data: Option<String>,
    pub tx_secret_key: String,
    pub tx_public_key: String,
    /// Every `random_scalar` draw of the build in order: `r`, then per input (v2) `alpha` and
    /// the decoy responses, or (v1) in ring order `k` at the real index and `c_i`, `r_i` at the
    /// others.
    pub randomness: Vec<String>,
    pub extra: String,
    /// In transaction order (sorted by amount).
    pub outputs: Vec<OutputCase>,
    pub prefix: String,
    pub prefix_hash: String,
    pub blob: String,
    pub hash: String,
    pub scans: Vec<ScanCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestinationCase {
    /// Standard or integrated address.
    pub address: String,
    pub amount: u64,
    /// Payment id of an integrated address.
    pub payment_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCase {
    pub amount: u64,
    pub global_index: u32,
    /// One-time key of the real output, its secret and key image.
    pub key: String,
    pub secret: String,
    pub key_image: String,
    pub received: ReceivedCase,
    /// Decoys as `/getrandom_outs` returned them (the order given to `build_transaction`).
    pub decoys: Vec<RingMember>,
    /// The ring: decoys and the real output sorted by global index.
    pub ring: Vec<RingMember>,
    pub real_index: usize,
    /// Relative global indexes as on the wire.
    pub key_offsets: Vec<u32>,
    /// Version 2: the nonce.
    pub alpha: Option<String>,
    /// Version 2: `s_{l+1}, .., s_{l-1}` in consumption order.
    pub decoy_responses: Option<Vec<String>>,
    /// Version 2: the signature.
    pub clsag: Option<ClsagCase>,
    /// Version 1: the nonce `k` of the real member.
    pub k: Option<String>,
    /// Version 1: the random `(c_i, r_i)` of the other members, in ring order.
    pub decoy_signatures: Option<Vec<SigPair>>,
    /// Version 1: the signature, one `(c, r)` per ring member.
    pub ring_signature: Option<Vec<SigPair>>,
}

/// How the wallet received the output it spends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedCase {
    pub wallet: String,
    /// `R` of the transaction that paid it.
    pub tx_public_key: String,
    pub output_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingMember {
    pub global_index: u32,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClsagCase {
    pub s: Vec<String>,
    pub c1: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigPair {
    pub ring_index: usize,
    pub c: String,
    pub r: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputCase {
    pub amount: u64,
    pub key: String,
    /// Version 2 only.
    pub view_tag: Option<String>,
    /// Wallet that owns it.
    pub recipient: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCase {
    pub wallet: String,
    pub found: Vec<FoundCase>,
    pub stats: Stats,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundCase {
    pub index: usize,
    pub amount: u64,
    pub key: String,
    pub view_tag: Option<String>,
    pub key_image: String,
}

/// An input to generate: an output `wallet` received, and its decoys.
#[derive(Debug, Clone)]
pub struct InputSpec {
    pub wallet: String,
    pub received_tx_public_key: PublicKey,
    pub output_index: u64,
    pub amount: u64,
    pub global_index: u32,
    pub decoys: Vec<(u32, PublicKey)>,
}

/// A transfer to generate.
#[derive(Debug, Clone)]
pub struct TransferSpec {
    /// Prefix of the randomness labels.
    pub label: String,
    /// Label of the transaction secret key.
    pub tx_key_label: String,
    pub version: TxVersion,
    pub inputs: Vec<InputSpec>,
    pub destinations: Vec<(Address, u64)>,
    pub change: Address,
    pub fee: u64,
    pub data: Option<Vec<u8>>,
    /// Wallets whose scan is recorded; together they own every output.
    pub scan: Vec<String>,
}

/// One-time key, secret and key image of output `index` of a transaction with public key
/// `tx_public_key`, as its owner computes them.
pub fn owned(
    owner: &WalletKeys,
    tx_public_key: &PublicKey,
    index: u64,
) -> (PublicKey, SecretKey, KeyImage) {
    let d = generate_key_derivation(tx_public_key, &owner.view_secret).expect("valid R");
    let key = derive_public_key(&d, index, &owner.spend_public).expect("valid spend key");
    let x = derive_secret_key(&d, index, &owner.spend_secret);
    let key_image = generate_key_image(&key, &x);
    (key, x, key_image)
}

fn hexes(items: &[EcScalar]) -> Vec<String> {
    items.iter().map(EcScalar::to_hex).collect()
}

fn scalars(items: &[String]) -> Vec<EcScalar> {
    items.iter().map(|s| from_hex(s.as_str())).collect()
}

fn version_of(v: TxVersion) -> u8 {
    match v {
        TxVersion::V1 => 1,
        TxVersion::V2 => 2,
    }
}

fn sig_pair(ring_index: usize, s: &Signature) -> SigPair {
    SigPair {
        ring_index,
        c: hex::encode(s.c()),
        r: hex::encode(s.r()),
    }
}

/// `(amount, key, view tag)` of every output.
fn output_list(tx: &xkr_types::Transaction) -> Vec<(u64, PublicKey, Option<u8>)> {
    tx.prefix
        .outputs
        .iter()
        .map(|o| (o.amount, *o.key(), o.view_tag()))
        .collect()
}

/// The wallet's scan of a transaction's outputs (view tags first, as a v2 wallet scans).
pub fn scan_case(
    name: &str,
    keys: &WalletKeys,
    tx_public: &PublicKey,
    outputs: &[(u64, PublicKey, Option<u8>)],
) -> ScanCase {
    let (found, stats) = scan_tagged_outputs_with_stats(keys, tx_public, outputs);
    ScanCase {
        wallet: name.to_string(),
        found: found
            .iter()
            .map(|f| FoundCase {
                index: f.index,
                amount: f.amount,
                key: f.key.to_hex(),
                view_tag: outputs[f.index].2.map(byte_hex),
                key_image: f.key_image.to_hex(),
            })
            .collect(),
        stats: stats.into(),
    }
}

/// Builds and signs `spec` with the randomness of its labels and records everything.
pub fn make_transfer(spec: &TransferSpec, wallets: &Wallets) -> TransferCase {
    let label = &spec.label;
    let tx_secret = secret(&spec.tx_key_label);
    let mut draws = vec![EcScalar(tx_secret.0)];
    let mut sources = Vec::new();
    let mut inputs = Vec::new();
    for (j, input) in spec.inputs.iter().enumerate() {
        let owner = &wallets[input.wallet.as_str()];
        let (key, x, key_image) = owned(owner, &input.received_tx_public_key, input.output_index);
        let mut ring = input.decoys.clone();
        ring.push((input.global_index, key));
        ring.sort_by_key(|m| m.0);
        let real = ring.iter().position(|m| m.1 == key).expect("real member");
        let n = ring.len();
        let globals: Vec<u32> = ring.iter().map(|m| m.0).collect();
        let mut case = InputCase {
            amount: input.amount,
            global_index: input.global_index,
            key: key.to_hex(),
            secret: x.to_hex(),
            key_image: key_image.to_hex(),
            received: ReceivedCase {
                wallet: input.wallet.clone(),
                tx_public_key: input.received_tx_public_key.to_hex(),
                output_index: input.output_index,
            },
            decoys: input.decoys.iter().map(member).collect(),
            ring: ring.iter().map(member).collect(),
            real_index: real,
            key_offsets: absolute_to_relative(&globals),
            alpha: None,
            decoy_responses: None,
            clsag: None,
            k: None,
            decoy_signatures: None,
            ring_signature: None,
        };
        match spec.version {
            TxVersion::V2 => {
                let alpha = scalar(&format!("{label} input {j} alpha"));
                let decoys: Vec<EcScalar> = (1..n)
                    .map(|k| scalar(&format!("{label} input {j} decoy {k}")))
                    .collect();
                draws.push(alpha);
                draws.extend_from_slice(&decoys);
                case.alpha = Some(alpha.to_hex());
                case.decoy_responses = Some(hexes(&decoys));
            }
            TxVersion::V1 => {
                let k = scalar(&format!("{label} input {j} k"));
                let mut pairs = Vec::new();
                for i in 0..n {
                    if i == real {
                        draws.push(k);
                        continue;
                    }
                    let c = scalar(&format!("{label} input {j} c {i}"));
                    let r = scalar(&format!("{label} input {j} r {i}"));
                    draws.extend_from_slice(&[c, r]);
                    pairs.push(sig_pair(i, &Signature::from_parts(&c.0, &r.0)));
                }
                case.k = Some(k.to_hex());
                case.decoy_signatures = Some(pairs);
            }
        }
        sources.push(InputSource {
            amount: input.amount,
            global_index: input.global_index,
            key,
            secret: x,
            key_image,
            decoys: input.decoys.clone(),
        });
        inputs.push(case);
    }
    let destinations: Vec<Destination> = spec
        .destinations
        .iter()
        .map(|&(address, amount)| Destination { address, amount })
        .collect();
    let opts = TransferOptions {
        version: spec.version,
        fee: spec.fee,
        unlock_time: 0,
        payment_id: None,
        data: spec.data.clone(),
        dust_threshold: 0,
        extra_raw: Vec::new(),
    };
    let mut rng = ScriptedRng::new(&draws);
    let built = build_transaction(&sources, &destinations, &spec.change, &opts, &mut rng)
        .expect("transfer builds");
    rng.assert_consumed();
    assert_eq!(built.tx_secret_key, tx_secret);
    let tx = &built.tx;
    for (j, case) in inputs.iter_mut().enumerate() {
        match spec.version {
            TxVersion::V2 => {
                let sig = &tx.clsag_signatures[j];
                case.clsag = Some(ClsagCase {
                    s: hexes(&sig.s),
                    c1: sig.c1.to_hex(),
                });
            }
            TxVersion::V1 => {
                let pairs = tx.signatures[j].iter().enumerate();
                case.ring_signature = Some(pairs.map(|(i, s)| sig_pair(i, s)).collect());
            }
        }
    }
    let tx_public = public(&tx_secret);
    let outputs = output_list(tx);
    let scans: Vec<ScanCase> = spec
        .scan
        .iter()
        .map(|name| scan_case(name, &wallets[name.as_str()], &tx_public, &outputs))
        .collect();
    let recipient = |index: usize| -> String {
        let owner = scans
            .iter()
            .find(|s| s.found.iter().any(|f| f.index == index));
        owner
            .expect("every output has a scanned owner")
            .wallet
            .clone()
    };
    let output_cases: Vec<OutputCase> = outputs
        .iter()
        .enumerate()
        .map(|(i, o)| OutputCase {
            amount: o.0,
            key: o.1.to_hex(),
            view_tag: o.2.map(byte_hex),
            recipient: recipient(i),
        })
        .collect();
    TransferCase {
        version: version_of(spec.version),
        inputs,
        destinations: spec
            .destinations
            .iter()
            .map(|(address, amount)| DestinationCase {
                address: address.to_string(),
                amount: *amount,
                payment_id: address.payment_id.map(|p| p.to_hex()),
            })
            .collect(),
        change_address: spec.change.to_string(),
        fee: spec.fee,
        unlock_time: 0,
        dust_threshold: 0,
        hugin_data: spec.data.as_ref().map(hex::encode),
        tx_secret_key: tx_secret.to_hex(),
        tx_public_key: tx_public.to_hex(),
        randomness: hexes(&draws),
        extra: hex::encode(&tx.prefix.extra),
        outputs: output_cases,
        prefix: hex::encode(encode_transaction_prefix(&tx.prefix)),
        prefix_hash: transaction_prefix_hash(&tx.prefix).to_hex(),
        blob: hex::encode(&built.blob),
        hash: built.hash.to_hex(),
        scans,
    }
}

fn member(m: &(u32, PublicKey)) -> RingMember {
    RingMember {
        global_index: m.0,
        key: m.1.to_hex(),
    }
}

/// Re-derives the inputs, checks the randomness order, rebuilds the transaction from the file
/// alone, then decodes the blob and checks layout, hashes, signatures, view tags and scans.
pub fn verify_transfer(name: &str, c: &TransferCase, wallets: &Wallets) {
    let version = match c.version {
        1 => TxVersion::V1,
        2 => TxVersion::V2,
        v => panic!("{name}: version {v}"),
    };
    let tx_secret: SecretKey = from_hex(&c.tx_secret_key);
    assert_eq!(public(&tx_secret).to_hex(), c.tx_public_key, "{name}");
    let mut draws = vec![c.tx_secret_key.clone()];
    let mut sources = Vec::new();
    for input in &c.inputs {
        let owner = &wallets[input.received.wallet.as_str()];
        let received: PublicKey = from_hex(&input.received.tx_public_key);
        let (key, x, key_image) = owned(owner, &received, input.received.output_index);
        assert_eq!(key.to_hex(), input.key, "{name}");
        assert_eq!(x.to_hex(), input.secret, "{name}");
        assert_eq!(key_image.to_hex(), input.key_image, "{name}");
        let decoys: Vec<(u32, PublicKey)> = input
            .decoys
            .iter()
            .map(|m| (m.global_index, from_hex(&m.key)))
            .collect();
        let mut ring = decoys.clone();
        ring.push((input.global_index, key));
        ring.sort_by_key(|m| m.0);
        let ring_cases: Vec<RingMember> = ring.iter().map(member).collect();
        assert_eq!(ring_cases, input.ring, "{name}: ring order");
        assert_eq!(ring[input.real_index], (input.global_index, key), "{name}");
        let globals: Vec<u32> = ring.iter().map(|m| m.0).collect();
        assert_eq!(absolute_to_relative(&globals), input.key_offsets, "{name}");
        match version {
            TxVersion::V2 => {
                draws.push(input.alpha.clone().expect("alpha"));
                draws.extend(input.decoy_responses.clone().expect("decoy responses"));
            }
            TxVersion::V1 => {
                let pairs = input.decoy_signatures.as_ref().expect("decoy signatures");
                let mut next = pairs.iter();
                for i in 0..ring.len() {
                    if i == input.real_index {
                        draws.push(input.k.clone().expect("k"));
                        continue;
                    }
                    let pair = next.next().expect("one pair per decoy");
                    assert_eq!(pair.ring_index, i, "{name}");
                    draws.extend([pair.c.clone(), pair.r.clone()]);
                }
                assert_eq!(next.next(), None, "{name}");
            }
        }
        sources.push(InputSource {
            amount: input.amount,
            global_index: input.global_index,
            key,
            secret: x,
            key_image,
            decoys,
        });
    }
    assert_eq!(draws, c.randomness, "{name}: randomness order");

    // Rebuild from the file alone.
    let destinations: Vec<Destination> = c
        .destinations
        .iter()
        .map(|d| {
            let address = Address::parse(&d.address).expect("destination address");
            assert_eq!(
                address.payment_id.map(|p| p.to_hex()),
                d.payment_id,
                "{name}"
            );
            Destination {
                address,
                amount: d.amount,
            }
        })
        .collect();
    let change = Address::parse(&c.change_address).expect("change address");
    let opts = TransferOptions {
        version,
        fee: c.fee,
        unlock_time: c.unlock_time,
        payment_id: None,
        data: c.hugin_data.as_ref().map(|d| hex::decode(d).expect("hex")),
        dust_threshold: c.dust_threshold,
        extra_raw: Vec::new(),
    };
    let mut rng = ScriptedRng::from_hex(&c.randomness);
    let built = build_transaction(&sources, &destinations, &change, &opts, &mut rng)
        .expect("transfer rebuilds");
    rng.assert_consumed();
    assert_eq!(hex::encode(&built.blob), c.blob, "{name}: rebuilt blob");
    assert_eq!(built.hash.to_hex(), c.hash, "{name}");

    // The blob itself.
    let blob = hex::decode(&c.blob).expect("hex");
    let tx = decode_transaction(&blob).expect("blob decodes");
    assert_eq!(encode_transaction(&tx).expect("encodes"), blob, "{name}");
    assert_eq!(tx.prefix.version, c.version);
    assert_eq!(tx.prefix.unlock_time, c.unlock_time);
    assert_eq!(hex::encode(encode_transaction_prefix(&tx.prefix)), c.prefix);
    let prefix_hash = transaction_prefix_hash(&tx.prefix);
    assert_eq!(prefix_hash.to_hex(), c.prefix_hash, "{name}");
    assert_eq!(transaction_hash_from_blob(&blob).to_hex(), c.hash, "{name}");
    assert_eq!(hex::encode(&tx.prefix.extra), c.extra, "{name}");
    let tx_public: PublicKey = from_hex(&c.tx_public_key);
    assert_eq!(tx_public_key(&tx.prefix.extra), Some(tx_public), "{name}");
    let outputs = output_list(&tx);
    assert_eq!(outputs.len(), c.outputs.len(), "{name}");
    for (i, (o, expected)) in outputs.iter().zip(&c.outputs).enumerate() {
        assert_eq!(o.0, expected.amount, "{name} output {i}");
        assert_eq!(o.1.to_hex(), expected.key, "{name} output {i}");
        assert_eq!(o.2.map(byte_hex), expected.view_tag, "{name} output {i}");
        // Sender side: key and tag from r and the recipient's address.
        let to = &wallets[expected.recipient.as_str()];
        let d = generate_key_derivation(&to.view_public, &tx_secret).expect("view key");
        let key = derive_public_key(&d, i as u64, &to.spend_public).expect("spend key");
        assert_eq!(key, o.1, "{name} output {i}");
        let tag = (version == TxVersion::V2).then(|| view_tag(&d, i as u64));
        assert_eq!(o.2, tag, "{name} output {i}");
    }
    let paid: u64 = outputs.iter().map(|o| o.0).sum();
    let spent: u64 = c.inputs.iter().map(|i| i.amount).sum();
    assert_eq!(paid + c.fee, spent, "{name}: amounts");

    // Signatures.
    for (j, (input, source)) in c.inputs.iter().zip(&sources).enumerate() {
        let TxInput::Key {
            amount,
            output_indexes,
            key_image,
        } = &tx.prefix.inputs[j]
        else {
            panic!("{name}: input {j} is not a key input");
        };
        assert_eq!(*amount, input.amount, "{name}");
        assert_eq!(output_indexes, &input.key_offsets, "{name}");
        assert_eq!(key_image.to_hex(), input.key_image, "{name}");
        let members: Vec<PublicKey> = input.ring.iter().map(|m| from_hex(&m.key)).collect();
        match version {
            TxVersion::V2 => {
                let expected = input.clsag.as_ref().expect("clsag");
                let sig = ClsagSignature {
                    s: scalars(&expected.s),
                    c1: from_hex(&expected.c1),
                };
                assert_eq!(tx.clsag_signatures[j], sig, "{name} input {j}");
                assert!(
                    clsag_verify(&prefix_hash, &members, key_image, &sig),
                    "{name}"
                );
                let alpha: EcScalar = from_hex(input.alpha.as_deref().expect("alpha"));
                let decoys = scalars(input.decoy_responses.as_deref().expect("decoys"));
                let resigned = clsag_sign_with_randomness(
                    &prefix_hash,
                    &members,
                    &source.secret,
                    input.real_index,
                    &alpha,
                    &decoys,
                );
                assert_eq!(resigned, Some(sig), "{name} input {j}: re-signing");
            }
            TxVersion::V1 => {
                let pairs = input.ring_signature.as_ref().expect("members signature");
                let ring_sig: Vec<Signature> = pairs
                    .iter()
                    .map(|p| {
                        let c: EcScalar = from_hex(&p.c);
                        let r: EcScalar = from_hex(&p.r);
                        Signature::from_parts(&c.0, &r.0)
                    })
                    .collect();
                assert_eq!(tx.signatures[j], ring_sig, "{name} input {j}");
                let valid = check_ring_signature(&prefix_hash, key_image, &members, &ring_sig);
                assert!(valid, "{name} input {j}");
                for pair in input.decoy_signatures.as_deref().expect("decoy signatures") {
                    assert_eq!(&pairs[pair.ring_index], pair, "{name} input {j}");
                }
            }
        }
    }

    // Scans: each listed wallet finds exactly its outputs.
    for s in &c.scans {
        let scanner = &wallets[s.wallet.as_str()];
        assert_eq!(
            &scan_case(&s.wallet, scanner, &tx_public, &outputs),
            s,
            "{name}"
        );
        // The subwallet scan with the wallet's one spend key finds the same outputs and key
        // images; view-only, the same outputs without key images.
        let recorded: Vec<(usize, u64, String)> = s
            .found
            .iter()
            .map(|f| (f.index, f.amount, f.key.clone()))
            .collect();
        let subwallet_keys = SpendKeys::from(scanner);
        let owned =
            scan_for_subwallets(&scanner.view_secret, &subwallet_keys, &tx_public, &outputs);
        let got: Vec<(usize, u64, String)> = owned
            .iter()
            .map(|o| (o.index, o.amount, o.key.to_hex()))
            .collect();
        assert_eq!(got, recorded, "{name}");
        let images: Vec<String> = owned
            .iter()
            .map(|o| o.key_image.expect("spend key").to_hex())
            .collect();
        let recorded_images: Vec<String> = s.found.iter().map(|f| f.key_image.clone()).collect();
        assert_eq!(images, recorded_images, "{name}");
        assert!(owned.iter().all(|o| o.owner == scanner.spend_public));
        let view_only = SpendKeys::view_only([scanner.spend_public]);
        let viewed = scan_for_subwallets(&scanner.view_secret, &view_only, &tx_public, &outputs);
        assert_eq!(viewed.len(), owned.len(), "{name}");
        assert!(
            viewed
                .iter()
                .all(|o| o.key_image.is_none() && o.secret.is_none())
        );
    }
    for (i, o) in c.outputs.iter().enumerate() {
        let scan = c
            .scans
            .iter()
            .find(|s| s.wallet == o.recipient)
            .expect("scan");
        assert!(scan.found.iter().any(|f| f.index == i), "{name} output {i}");
    }
}
