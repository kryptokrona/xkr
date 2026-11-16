//! Seeded cases for `compat/compare.js`, which recomputes them with kryptokrona-crypto's Node
//! addon (the C++ implementation the desktop wallet uses) and reports every difference.
//!
//! Always runs (so the cases themselves are exercised); writes the file only when
//! `XKR_COMPAT_DUMP=<path>` is set:
//!
//! ```text
//! XKR_COMPAT_DUMP=/tmp/xkr-compat.json cargo test -p xkr-crypto-ffi --test compat_dump
//! node crates/xkr-crypto-ffi/compat/compare.js /tmp/xkr-compat.json <kryptokrona-crypto checkout>
//! ```
//!
//! Each case is `{ "fn": <Crypto method>, "args": [...], "result": <value> }` with the method
//! names and argument shapes of kryptokrona-crypto's `Crypto` class; only deterministic
//! functions, and only inputs on which both implementations are specified to agree (canonical
//! scalars, valid points where the JavaScript wrapper requires them). `XKR_COMPAT_SEED`
//! changes the seed (default 1), `XKR_COMPAT_COUNT` the number of cases per function (default
//! 64).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use xkr_crypto_ffi::api;

/// The functions `cases` writes once per round (`count` rounds); `checkSignature` is written
/// twice per round (a valid and a changed signature).
const PER_ROUND: [&str; 26] = [
    "checkKey",
    "checkScalar",
    "scReduce32",
    "hashToEllipticCurve",
    "hashToScalar",
    "cn_fast_hash",
    "secretKeyToPublicKey",
    "generateKeyDerivation",
    "generateKeyImage",
    "derivePublicKey",
    "deriveSecretKey",
    "underivePublicKey",
    "deriveViewTag",
    "derivationToScalar",
    "generateKeyDerivationScalar",
    "scalarDerivePublicKey",
    "scalarDeriveSecretKey",
    "generateDeterministicSubwalletKeys",
    "generateViewKeysFromPrivateSpendKey",
    "generatePrivateViewKeyFromPrivateSpendKey",
    "tree_hash",
    "tree_branch",
    "tree_depth",
    "checkRingSignatures",
    "clsagSign",
    "clsagVerify",
];

/// The CryptoNight families and their kryptokrona-crypto method prefixes.
const CN_FAMILIES: [(api::CnFamily, &str); 6] = [
    (api::CnFamily::Cn, "cn"),
    (api::CnFamily::Lite, "cn_lite"),
    (api::CnFamily::Dark, "cn_dark"),
    (api::CnFamily::DarkLite, "cn_dark_lite"),
    (api::CnFamily::Turtle, "cn_turtle"),
    (api::CnFamily::TurtleLite, "cn_turtle_lite"),
];

/// splitmix64: a small deterministic generator, so the same seed gives the same file anywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn bytes(&mut self, len: usize) -> String {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        hex::encode(out)
    }

    fn scalar(&mut self) -> String {
        api::sc_reduce32(&self.bytes(32)).unwrap()
    }

    fn point(&mut self) -> String {
        api::secret_key_to_public_key(&self.scalar()).unwrap()
    }

    fn derivation(&mut self) -> String {
        api::generate_key_derivation(&self.point(), &self.scalar()).unwrap()
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn keypair(k: &api::KeyPair) -> Value {
    json!({ "private_key": k.private_key, "public_key": k.public_key })
}

#[allow(clippy::too_many_lines)]
fn cases(seed: u64, count: u64) -> Vec<Value> {
    let mut random = Rng(seed);
    let mut out = Vec::new();
    let mut push = |f: &str, args: Value, result: Value| {
        out.push(json!({ "fn": f, "args": args, "result": result }));
    };

    for _ in 0..count {
        let raw = random.bytes(32);
        push("checkKey", json!([raw]), json!(api::check_key(&raw)));
        push("checkScalar", json!([raw]), json!(api::check_scalar(&raw)));
        push(
            "scReduce32",
            json!([raw]),
            json!(api::sc_reduce32(&raw).unwrap()),
        );
        push(
            "hashToEllipticCurve",
            json!([raw]),
            json!(api::hash_to_elliptic_curve(&raw).unwrap()),
        );
        push(
            "hashToScalar",
            json!([raw]),
            json!(api::hash_to_scalar(&raw).unwrap()),
        );

        let len = usize::try_from(random.below(200)).unwrap();
        let data = random.bytes(len);
        push(
            "cn_fast_hash",
            json!([data]),
            json!(api::cn_fast_hash(&data).unwrap()),
        );

        let secret = random.scalar();
        push(
            "secretKeyToPublicKey",
            json!([secret]),
            json!(api::secret_key_to_public_key(&secret).unwrap()),
        );
        let public = random.point();
        push(
            "generateKeyDerivation",
            json!([public, secret]),
            json!(api::generate_key_derivation(&public, &secret).unwrap()),
        );
        push(
            "generateKeyImage",
            json!([public, secret]),
            json!(api::generate_key_image(&public, &secret).unwrap()),
        );

        let d = random.derivation();
        let index = random.below(1000);
        push(
            "derivePublicKey",
            json!([d, index, public]),
            json!(api::derive_public_key(&d, index, &public).unwrap()),
        );
        push(
            "deriveSecretKey",
            json!([d, index, secret]),
            json!(api::derive_secret_key(&d, index, &secret).unwrap()),
        );
        push(
            "underivePublicKey",
            json!([d, index, public]),
            json!(api::underive_public_key(&d, index, &public).unwrap()),
        );
        push(
            "deriveViewTag",
            json!([d, index]),
            json!(api::derive_view_tag(&d, index).unwrap()),
        );
        let ds = api::derivation_to_scalar(&d, index).unwrap();
        push("derivationToScalar", json!([d, index]), json!(ds));
        push(
            "generateKeyDerivationScalar",
            json!([public, secret, index]),
            json!(api::generate_key_derivation_scalar(&public, &secret, index).unwrap()),
        );
        push(
            "scalarDerivePublicKey",
            json!([ds, public]),
            json!(api::scalar_derive_public_key(&ds, &public).unwrap()),
        );
        push(
            "scalarDeriveSecretKey",
            json!([ds, secret]),
            json!(api::scalar_derive_secret_key(&ds, &secret).unwrap()),
        );

        let sub_index = random.below(20);
        push(
            "generateDeterministicSubwalletKeys",
            json!([secret, sub_index]),
            keypair(&api::generate_deterministic_subwallet_keys(&secret, sub_index).unwrap()),
        );
        push(
            "generateViewKeysFromPrivateSpendKey",
            json!([secret]),
            keypair(&api::generate_view_keys_from_private_spend_key(&secret).unwrap()),
        );
        push(
            "generatePrivateViewKeyFromPrivateSpendKey",
            json!([secret]),
            json!(api::generate_private_view_key_from_private_spend_key(&secret).unwrap()),
        );

        let n = usize::try_from(random.below(9) + 1).unwrap();
        let hashes: Vec<String> = (0..n).map(|_| random.bytes(32)).collect();
        push(
            "tree_hash",
            json!([hashes]),
            json!(api::tree_hash(&hashes).unwrap()),
        );
        push(
            "tree_branch",
            json!([hashes]),
            json!(api::tree_branch(&hashes).unwrap()),
        );
        push("tree_depth", json!([n]), json!(api::tree_depth(n as u64)));

        // signatures made here must verify there (the signing itself is random)
        let prefix = random.bytes(32);
        let own_public = api::secret_key_to_public_key(&secret).unwrap();
        let sig = api::generate_signature(&prefix, &own_public, &secret).unwrap();
        push(
            "checkSignature",
            json!([prefix, own_public, sig]),
            json!(true),
        );
        let mut tampered = sig.clone();
        tampered.replace_range(0..2, if sig.starts_with("00") { "01" } else { "00" });
        push(
            "checkSignature",
            json!([prefix, own_public, tampered]),
            json!(api::check_signature(&prefix, &own_public, &tampered)),
        );

        let ring_size = usize::try_from(random.below(6) + 1).unwrap();
        let real = usize::try_from(random.below(ring_size as u64)).unwrap();
        let ring_secrets: Vec<String> = (0..ring_size).map(|_| random.scalar()).collect();
        let ring: Vec<String> = ring_secrets
            .iter()
            .map(|s| api::secret_key_to_public_key(s).unwrap())
            .collect();
        let image = api::generate_key_image(&ring[real], &ring_secrets[real]).unwrap();
        let sigs =
            api::generate_ring_signatures(&prefix, &image, &ring, &ring_secrets[real], real as u64)
                .unwrap();
        push(
            "checkRingSignatures",
            json!([prefix, image, ring, sigs]),
            json!(true),
        );
        let alpha = random.scalar();
        let decoys: Vec<String> = (1..ring_size).map(|_| random.scalar()).collect();
        let clsag = api::clsag_sign(
            &prefix,
            &ring,
            &ring_secrets[real],
            real as u64,
            &alpha,
            &decoys,
        )
        .unwrap();
        push(
            "clsagSign",
            json!([prefix, ring, ring_secrets[real], real, alpha, decoys]),
            json!({ "c1": clsag.c1, "s": clsag.s }),
        );
        push(
            "clsagVerify",
            json!([prefix, ring, image, clsag.c1, clsag.s]),
            json!(true),
        );
    }

    // slow hashes: fewer cases
    for _ in 0..count.div_ceil(8) {
        let data = random.bytes(76);
        push(
            "cn_turtle_lite_slow_hash_v2",
            json!([data]),
            json!(api::cn_turtle_lite_slow_hash_v2(&data).unwrap()),
        );
    }
    let data = random.bytes(76);
    for (family, name) in CN_FAMILIES {
        for variant in 0..=2u8 {
            push(
                &format!("{name}_slow_hash_v{variant}"),
                json!([data]),
                json!(api::cn_slow_hash(&data, family, variant).unwrap()),
            );
        }
    }
    out
}

/// How many cases `cases(_, count)` writes for each function.
fn expected_counts(count: u64) -> BTreeMap<String, u64> {
    let mut want: BTreeMap<String, u64> = PER_ROUND
        .into_iter()
        .map(|f| (f.to_owned(), count))
        .collect();
    want.insert("checkSignature".to_owned(), 2 * count);
    for (_, name) in CN_FAMILIES {
        for variant in 0..=2u8 {
            want.insert(format!("{name}_slow_hash_v{variant}"), 1);
        }
    }
    // the separate turtle-lite v2 loop comes on top of its family case
    *want.get_mut("cn_turtle_lite_slow_hash_v2").unwrap() += count.div_ceil(8);
    want
}

#[test]
fn compat_cases() {
    let seed = env_u64("XKR_COMPAT_SEED", 1);
    let count = env_u64("XKR_COMPAT_COUNT", 64);
    let all = cases(seed, count);
    // Signing draws OS randomness, so two runs differ in the signature arguments only; every
    // deterministic case is reproducible from the seed.
    let deterministic = |list: &[Value]| -> Vec<Value> {
        list.iter()
            .filter(|c| {
                !matches!(
                    c["fn"].as_str(),
                    Some("checkSignature" | "checkRingSignatures" | "clsagVerify")
                )
            })
            .cloned()
            .collect()
    };
    assert_eq!(deterministic(&all), deterministic(&cases(seed, count)));
    let mut got: BTreeMap<String, u64> = BTreeMap::new();
    for case in &all {
        *got.entry(case["fn"].as_str().unwrap().to_owned())
            .or_default() += 1;
    }
    assert_eq!(
        got,
        expected_counts(count),
        "cases per function (left: written, right: expected)"
    );
    if let Ok(path) = std::env::var("XKR_COMPAT_DUMP") {
        let doc = json!({ "seed": seed, "count": count, "cases": all });
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        eprintln!(
            "wrote {} cases to {path}",
            doc["cases"].as_array().unwrap().len()
        );
    }
}
