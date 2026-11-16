//! Vector C of `docs/tx-v2.md` §7, `docs/test-vectors/tx-v2-clsag.json`: single-key CLSAG
//! signatures with explicit randomness and the full challenge chain, for ring sizes 1-16 at
//! several signer indexes, plus named negative cases on one fixed signature (n = 11).
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-wallet-core --test tx_v2_clsag` writes the file;
//! without the variable the file is checked against the Rust implementation.

mod vectors;

use serde::{Deserialize, Serialize};
use vectors::{from_hex, public, scalar, secret};
use xkr_crypto::clsag::{CLSAG_AGG_DOMAIN, CLSAG_ROUND_DOMAIN, clsag_aggregation_coefficient};
use xkr_crypto::{
    EdwardsPoint, Scalar, check_key, clsag_challenge_chain, clsag_sign_with_randomness,
    clsag_verify, cn_fast_hash, decompress, generate_key_image, key_image_in_prime_subgroup,
    secret_key_to_public_key,
};
use xkr_types::{ClsagSignature, EcScalar, Hash, KeyImage, PublicKey, SecretKey};

const FILE: &str = "tx-v2-clsag.json";
const DESCRIPTION: &str = "Fork-14 single-key CLSAG (docs/tx-v2.md §4). Per vector: \
    message m, ring P_0..P_{n-1} (transaction order), signer index l and secret x, \
    key image I = x*H_p(P_l), mu, the randomness (alpha, then the decoy responses in \
    consumption order s_{l+1}, .., s_{n-1}, s_0, .., s_{l-1}, each reduced mod l), the challenge \
    chain c_0..c_{n-1} (c_0 = c1) and the expected c1 and s_0..s_{n-1}. Negative cases alter \
    one input of the n = 11, l = 5 signature; expected is the verification result. Secrets \
    are H_s(\"xkr tx-v2 vectors \" || label). Generated and checked by \
    crates/xkr-wallet-core/tests/tx_v2_clsag.rs.";
const RING_SIZES: [usize; 7] = [1, 2, 3, 4, 6, 11, 16];
/// The signature the negative cases alter.
const NEGATIVE_BASE: (usize, usize) = (11, 5);
/// An 8-torsion point (curve25519-dalek `EIGHT_TORSION[1]`, order 8).
const TORSION: &str = "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a";
/// `y = 2^255 - 1 >= p`: not a point encoding (ref10 `ge_frombytes_vartime` refuses it).
const NOT_A_POINT: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f";
/// The group order `l`, little endian.
const ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
];

#[derive(Debug, Serialize, Deserialize)]
struct ClsagVectors {
    description: String,
    /// `pad32("XKR_CLSAG_agg_0")`.
    agg_domain: String,
    /// `pad32("XKR_CLSAG_round")`.
    round_domain: String,
    vectors: Vec<ClsagVector>,
    negative: Vec<NegativeCase>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ClsagVector {
    name: String,
    ring_size: usize,
    signer_index: usize,
    message: String,
    ring: Vec<String>,
    secret: String,
    key_image: String,
    mu: String,
    alpha: String,
    /// `s_{l+1}, .., s_{l-1}` (indexes mod n), in the order the signer consumes them.
    decoy_responses: Vec<String>,
    /// `c_0 .. c_{n-1}`.
    challenges: Vec<String>,
    c1: String,
    s: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NegativeCase {
    name: String,
    message: String,
    ring: Vec<String>,
    key_image: String,
    s: Vec<String>,
    c1: String,
    /// What `clsag_verify` returns.
    expected: bool,
}

fn pad32(domain: &[u8]) -> String {
    let mut out = [0u8; 32];
    out[..domain.len()].copy_from_slice(domain);
    hex::encode(out)
}

fn signer_indexes(n: usize) -> Vec<usize> {
    let mut v = vec![0, n / 2, n - 1];
    v.dedup();
    v
}

fn ring_secrets(n: usize) -> Vec<SecretKey> {
    (0..n)
        .map(|i| secret(&format!("clsag n{n} member {i}")))
        .collect()
}

fn message(n: usize, l: usize) -> Hash {
    cn_fast_hash(format!("xkr tx-v2 vectors clsag message n{n} l{l}").as_bytes())
}

fn hexes(items: &[EcScalar]) -> Vec<String> {
    items.iter().map(EcScalar::to_hex).collect()
}

fn keys_hex(ring: &[PublicKey]) -> Vec<String> {
    ring.iter().map(PublicKey::to_hex).collect()
}

fn vector(n: usize, l: usize) -> ClsagVector {
    let secrets = ring_secrets(n);
    let ring: Vec<PublicKey> = secrets.iter().map(public).collect();
    let msg = message(n, l);
    let x = &secrets[l];
    let key_image = generate_key_image(&ring[l], x);
    let alpha = scalar(&format!("clsag n{n} l{l} alpha"));
    let decoys: Vec<EcScalar> = (1..n)
        .map(|k| scalar(&format!("clsag n{n} l{l} decoy {k}")))
        .collect();
    let sig = clsag_sign_with_randomness(&msg, &ring, x, l, &alpha, &decoys).expect("signs");
    assert!(clsag_verify(&msg, &ring, &key_image, &sig));
    let challenges = clsag_challenge_chain(&msg, &ring, &key_image, &sig).expect("chain");
    let mu = clsag_aggregation_coefficient(&ring, &key_image);
    ClsagVector {
        name: format!("n{n}_l{l}"),
        ring_size: n,
        signer_index: l,
        message: msg.to_hex(),
        ring: keys_hex(&ring),
        secret: x.to_hex(),
        key_image: key_image.to_hex(),
        mu: hex::encode(mu.to_bytes()),
        alpha: alpha.to_hex(),
        decoy_responses: hexes(&decoys),
        challenges: hexes(&challenges),
        c1: sig.c1.to_hex(),
        s: hexes(&sig.s),
    }
}

/// `x + l` (still below `2^256` for every canonical `x`).
fn plus_order(x: &EcScalar) -> EcScalar {
    let mut sum = [0u8; 32];
    let mut carry = 0u16;
    for ((out, a), b) in sum.iter_mut().zip(&x.0).zip(&ORDER) {
        let v = u16::from(*a) + u16::from(*b) + carry;
        *out = v as u8;
        carry = v >> 8;
    }
    assert_eq!(carry, 0);
    EcScalar(sum)
}

/// The inputs of one verification.
#[derive(Clone)]
struct Inputs {
    msg: Hash,
    ring: Vec<PublicKey>,
    key_image: KeyImage,
    sig: ClsagSignature,
}

fn case(name: &str, i: &Inputs) -> NegativeCase {
    NegativeCase {
        name: name.to_string(),
        message: i.msg.to_hex(),
        ring: keys_hex(&i.ring),
        key_image: i.key_image.to_hex(),
        s: hexes(&i.sig.s),
        c1: i.sig.c1.to_hex(),
        expected: clsag_verify(&i.msg, &i.ring, &i.key_image, &i.sig),
    }
}

fn negative_cases() -> Vec<NegativeCase> {
    let (n, l) = NEGATIVE_BASE;
    let secrets = ring_secrets(n);
    let ring: Vec<PublicKey> = secrets.iter().map(public).collect();
    let msg = message(n, l);
    let key_image = generate_key_image(&ring[l], &secrets[l]);
    let alpha = scalar(&format!("clsag n{n} l{l} alpha"));
    let decoys: Vec<EcScalar> = (1..n)
        .map(|k| scalar(&format!("clsag n{n} l{l} decoy {k}")))
        .collect();
    let sig =
        clsag_sign_with_randomness(&msg, &ring, &secrets[l], l, &alpha, &decoys).expect("signs");
    let base = Inputs {
        msg,
        ring,
        key_image,
        sig,
    };
    let mut out = vec![case("original", &base)];
    let mut i = base.clone();
    i.msg = cn_fast_hash(b"xkr tx-v2 vectors clsag other message");
    out.push(case("other_message", &i));
    let mut i = base.clone();
    i.msg = Hash::ZERO;
    out.push(case("zero_message", &i));
    let mut i = base.clone();
    i.key_image = generate_key_image(&base.ring[0], &secrets[0]);
    out.push(case("key_image_of_member_0", &i));
    let mut i = base.clone();
    let mut identity = [0u8; 32];
    identity[0] = 1;
    i.key_image = KeyImage(identity);
    out.push(case("identity_key_image", &i));
    let mut i = base.clone();
    let image_point = decompress(&base.key_image.0).expect("key image decodes");
    let torsion: EdwardsPoint = decompress(&from_hex::<PublicKey>(TORSION).0).expect("torsion");
    i.key_image = KeyImage((image_point + torsion).compress().to_bytes());
    assert!(!key_image_in_prime_subgroup(&i.key_image));
    out.push(case("key_image_plus_torsion", &i));
    for k in 0..n {
        let mut i = base.clone();
        i.sig.s[k].0[0] ^= 1;
        out.push(case(&format!("s_{k}_bit_0_flipped"), &i));
    }
    let mut i = base.clone();
    i.sig.c1.0[0] ^= 1;
    out.push(case("c1_bit_0_flipped", &i));
    let mut i = base.clone();
    i.sig.s[0] = plus_order(&base.sig.s[0]);
    out.push(case("s_0_plus_l", &i));
    let mut i = base.clone();
    i.sig.c1 = plus_order(&base.sig.c1);
    out.push(case("c1_plus_l", &i));
    let mut i = base.clone();
    i.sig.s.truncate(n - 1);
    out.push(case("s_one_short", &i));
    let mut i = base.clone();
    i.ring.truncate(n - 1);
    out.push(case("ring_one_short", &i));
    let mut i = base.clone();
    i.ring.swap(0, 1);
    out.push(case("ring_members_0_and_1_swapped", &i));
    let mut i = base.clone();
    i.ring[8] = public(&secret("clsag replaced member"));
    out.push(case("ring_member_8_replaced", &i));
    let mut i = base.clone();
    i.ring[7] = from_hex(NOT_A_POINT);
    assert!(!check_key(&i.ring[7]));
    out.push(case("ring_member_7_not_a_point", &i));
    let mut i = base.clone();
    i.ring.clear();
    i.sig.s.clear();
    out.push(case("empty_ring", &i));
    out
}

fn build() -> ClsagVectors {
    let mut vectors = Vec::new();
    for n in RING_SIZES {
        for l in signer_indexes(n) {
            vectors.push(vector(n, l));
        }
    }
    let negative = negative_cases();
    assert!(negative[0].expected);
    assert!(negative[1..].iter().all(|c| !c.expected));
    ClsagVectors {
        description: DESCRIPTION.to_string(),
        agg_domain: pad32(CLSAG_AGG_DOMAIN),
        round_domain: pad32(CLSAG_ROUND_DOMAIN),
        vectors,
        negative,
    }
}

fn scalars(items: &[String]) -> Vec<EcScalar> {
    items.iter().map(|s| from_hex(s.as_str())).collect()
}

fn keys(items: &[String]) -> Vec<PublicKey> {
    items.iter().map(|s| from_hex(s.as_str())).collect()
}

fn verify(v: &ClsagVectors) {
    assert_eq!(v.agg_domain, pad32(b"XKR_CLSAG_agg_0"));
    assert_eq!(v.round_domain, pad32(b"XKR_CLSAG_round"));
    let shapes: Vec<(usize, usize)> = v
        .vectors
        .iter()
        .map(|c| (c.ring_size, c.signer_index))
        .collect();
    let expected: Vec<(usize, usize)> = RING_SIZES
        .iter()
        .flat_map(|&n| signer_indexes(n).into_iter().map(move |l| (n, l)))
        .collect();
    assert_eq!(shapes, expected);
    for c in &v.vectors {
        let (n, l) = (c.ring_size, c.signer_index);
        let msg: Hash = from_hex(&c.message);
        let ring = keys(&c.ring);
        assert_eq!(ring.len(), n);
        let x: SecretKey = from_hex(&c.secret);
        assert_eq!(secret_key_to_public_key(&x), Some(ring[l]), "{}", c.name);
        let key_image = generate_key_image(&ring[l], &x);
        assert_eq!(key_image.to_hex(), c.key_image, "{}", c.name);
        let mu = clsag_aggregation_coefficient(&ring, &key_image);
        assert_eq!(hex::encode(mu.to_bytes()), c.mu, "{}", c.name);
        // Randomness: canonical scalars, n - 1 decoy responses landing at l+1, l+2, ...
        let alpha: EcScalar = from_hex(&c.alpha);
        let decoys = scalars(&c.decoy_responses);
        assert_eq!(decoys.len(), n - 1);
        for d in decoys.iter().chain([&alpha]) {
            let reduced = Scalar::from_bytes_mod_order(d.0).to_bytes();
            assert_eq!(reduced, d.0, "{}: randomness must be reduced", c.name);
        }
        let sig = ClsagSignature {
            s: scalars(&c.s),
            c1: from_hex(&c.c1),
        };
        for (k, d) in decoys.iter().enumerate() {
            assert_eq!(&sig.s[(l + 1 + k) % n], d, "{}", c.name);
        }
        let resigned = clsag_sign_with_randomness(&msg, &ring, &x, l, &alpha, &decoys);
        assert_eq!(resigned.as_ref(), Some(&sig), "{}: re-signing", c.name);
        assert!(clsag_verify(&msg, &ring, &key_image, &sig), "{}", c.name);
        let chain = clsag_challenge_chain(&msg, &ring, &key_image, &sig).expect("chain");
        assert_eq!(hexes(&chain), c.challenges, "{}", c.name);
        assert_eq!(c.challenges[0], c.c1);
    }
    let names: Vec<&str> = v.negative.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[0], "original");
    assert_eq!(v.negative.len(), 26);
    for c in &v.negative {
        let msg: Hash = from_hex(&c.message);
        let ring = keys(&c.ring);
        let key_image: KeyImage = from_hex(&c.key_image);
        let sig = ClsagSignature {
            s: scalars(&c.s),
            c1: from_hex(&c.c1),
        };
        let got = clsag_verify(&msg, &ring, &key_image, &sig);
        assert_eq!(got, c.expected, "{}", c.name);
        assert_eq!(c.expected, c.name == "original", "{}", c.name);
    }
}

#[test]
fn write_clsag_vectors() {
    if let Some(v) = vectors::generate(FILE, build) {
        verify(&v);
    }
}

#[test]
fn clsag_vectors() {
    if let Some(v) = vectors::load(FILE, build) {
        verify(&v);
    }
}
