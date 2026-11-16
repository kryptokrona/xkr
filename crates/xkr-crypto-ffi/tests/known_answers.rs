//! Known-answer vectors from the sources the mobile module has to agree with:
//!
//! * kryptokrona-crypto `tests/crypto.ts` (the values the JavaScript libraries are tested with),
//! * `monero/tests/crypto/tests.txt` (subset in `crates/xkr-wallet-core/tests/data`),
//! * `docs/test-vectors/cryptonight.txt` (TurtleCoin/Kryptokrona `crypto_test`),
//! * `docs/test-vectors/tx-v2-clsag.json` and `tx-v2-view-tags.json` (transaction version 2).

use serde_json::Value;
use xkr_crypto_ffi::api::{self, CnFamily};
use xkr_crypto_ffi::{CryptoError, pow};

const TESTDATA: &str = "0100fb8e8ac805899323371bb790db19218afd8db8e3755d8b90f39b3d5506a9abce4fa912244500000000ee8146d49fa93ee724deb57d12cbc6c6f3b924d946127c7a97418f9348828f0f02";

// ---------------------------------------------------------------------------------------------
// kryptokrona-crypto tests/crypto.ts

const TX_PUBLIC: &str = "3b0cc2b066812e6b9fcc42a797dc3c723a7344b604fd4be0b22e06254ff57f94";
const VIEW_SECRET: &str = "6968a0b8f744ec4b8cea5ec124a1b4bd1626a2e6f31e999f8adbab52c4dfa909";
const DERIVATION: &str = "4827dbde0c0994c0979e2f9c046825bb4a065b6e35cabc0290ff5216af060c20";
const SPEND_PUBLIC: &str = "854a637b2863af9e8e8216eb2382f3d16616b3ac3e53d0976fbd6f8da6c56418";
const SPEND_SECRET: &str = "d9d555a892a85f64916cae1a168bd3f7f400b6471c7b12b438b599601298210b";
const OUTPUT_KEY: &str = "bb55bef919d1c9f74b5b52a8a6995a1dc4af4c0bb8824f5dc889012bc748173d";
const OUTPUT_SECRET: &str = "e52ece5717f01843e3accc4df651d669e339c31eb8059145e881faae19ad4a0e";
const DERIVATION_SCALAR: &str = "0c5978af8447b9de51401e33e0c60272ee380dd79b8a7e91afcc604e07152903";

#[test]
fn check_key_and_secret_key_to_public_key() {
    assert!(api::check_key(
        "7849297236cd7c0d6c69a3c8c179c038d3c1c434735741bb3c8995c3c9d6f2ac"
    ));
    assert!(!api::check_key(
        "4a078e76cd41a3d3b534b83dc6f2ea2de500b653ca82273b7bfad8045d85a400"
    ));
    assert_eq!(
        api::secret_key_to_public_key(
            "4a078e76cd41a3d3b534b83dc6f2ea2de500b653ca82273b7bfad8045d85a400"
        )
        .unwrap(),
        "7849297236cd7c0d6c69a3c8c179c038d3c1c434735741bb3c8995c3c9d6f2ac"
    );
}

#[test]
fn derivation_chain() {
    assert_eq!(
        api::generate_key_derivation(TX_PUBLIC, VIEW_SECRET).unwrap(),
        DERIVATION
    );
    assert_eq!(
        api::derive_public_key(DERIVATION, 2, SPEND_PUBLIC).unwrap(),
        OUTPUT_KEY
    );
    assert_eq!(
        api::underive_public_key(DERIVATION, 2, OUTPUT_KEY).unwrap(),
        SPEND_PUBLIC
    );
    assert_ne!(
        api::underive_public_key(DERIVATION, 0, OUTPUT_KEY).unwrap(),
        SPEND_PUBLIC
    );
    assert_eq!(
        api::derive_secret_key(DERIVATION, 2, SPEND_SECRET).unwrap(),
        OUTPUT_SECRET
    );
    assert_eq!(
        api::generate_key_derivation_scalar(TX_PUBLIC, VIEW_SECRET, 2).unwrap(),
        DERIVATION_SCALAR
    );
    assert_eq!(
        api::derivation_to_scalar(DERIVATION, 2).unwrap(),
        DERIVATION_SCALAR
    );
    assert_eq!(
        api::scalar_derive_public_key(DERIVATION_SCALAR, SPEND_PUBLIC).unwrap(),
        OUTPUT_KEY
    );
    assert_eq!(
        api::scalar_derive_secret_key(DERIVATION_SCALAR, SPEND_SECRET).unwrap(),
        OUTPUT_SECRET
    );
    assert_eq!(
        api::generate_key_image(OUTPUT_KEY, OUTPUT_SECRET).unwrap(),
        "5997cf23543ce2e05c327297a47f26e710af868344859a6f8d65683d8a2498b0"
    );
}

#[test]
fn deterministic_subwallets() {
    let base = "dd0c02d3202634821b4d9d91b63d919725f5c3e97e803f3512e52fb0dc2aab0c";
    for (index, want) in [
        (
            0,
            "dd0c02d3202634821b4d9d91b63d919725f5c3e97e803f3512e52fb0dc2aab0c",
        ),
        (
            1,
            "c55cbe4fd1c49dca5958fa1c7b9212c2dbf3fd5bfec84de741d434056e298600",
        ),
        (
            64,
            "29c2afed13271e2bb3321c2483356fd8798f2709af4de3906b6627ec71727108",
        ),
    ] {
        let keys = api::generate_deterministic_subwallet_keys(base, index).unwrap();
        assert_eq!(keys.private_key, want, "subwallet {index}");
        assert_eq!(
            api::secret_key_to_public_key(&keys.private_key).unwrap(),
            keys.public_key
        );
    }
}

#[test]
fn ring_signatures_of_the_kryptokrona_crypto_test() {
    let prefix = "b542df5b6e7f5f05275c98e7345884e2ac726aeeb07e03e44e0389eb86cd05f0";
    let key_image = "6865866ed8a25824e042e21dd36e946836b58b03366e489aecf979f444f599b0";
    let ring = [
        "492390897da1cabd3886e3eff43ad1d04aa510a905bec0acd31a0a2f260e7862",
        "7644ccb5410cca2be18b033e5f7497aeeeafd1d8f317f29cba4803e4306aa402",
        "bb9a956ffdf8159ad69474e6b0811316c44a17a540d5e39a44642d4d933a6460",
        "e1cd9ccdfdf2b3a45ac2cfd1e29185d22c185742849f52368c3cdd1c0ce499c0",
    ];
    let secret = "73a8e577d58f7c11992201d4014ac7eef39c1e9f6f6d78673103de60a0c3240b";
    let sigs = api::generate_ring_signatures(prefix, key_image, &ring, secret, 3).unwrap();
    assert!(api::check_ring_signature(prefix, key_image, &ring, &sigs));
    let prepared = api::prepare_ring_signatures(prefix, key_image, &ring, 3, None).unwrap();
    let completed =
        api::complete_ring_signatures(secret, 3, &prepared.k, &prepared.signatures).unwrap();
    assert!(api::check_ring_signature(
        prefix, key_image, &ring, &completed
    ));
}

#[test]
fn view_keys_from_spend_key() {
    // kryptokrona-wallet-backend-js seed wallet (xkr-wallet-core keys.rs)
    let keys = api::generate_view_keys_from_private_spend_key(
        "d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904",
    )
    .unwrap();
    assert_eq!(
        keys.private_key,
        "688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801"
    );
    assert_eq!(
        api::secret_key_to_public_key(&keys.private_key).unwrap(),
        keys.public_key
    );
}

// ---------------------------------------------------------------------------------------------
// monero tests.txt subset

fn flag(s: &str) -> bool {
    match s {
        "true" => true,
        "false" => false,
        other => panic!("not a bool: {other}"),
    }
}

#[test]
fn monero_reference_vectors() {
    let data = include_str!("../../xkr-wallet-core/tests/data/crypto_vectors.txt");
    let mut checked = 0usize;
    for line in data
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "secret_key_to_public_key" => {
                let got = api::secret_key_to_public_key(f[1]);
                if flag(f[2]) {
                    assert_eq!(got.as_deref(), Ok(f[3]), "{line}");
                } else {
                    assert_eq!(got, Err(CryptoError::InvalidScalar("privateKey")), "{line}");
                }
            }
            "generate_key_derivation" => {
                let got = api::generate_key_derivation(f[1], f[2]);
                if flag(f[3]) {
                    assert_eq!(got.as_deref(), Ok(f[4]), "{line}");
                } else {
                    assert_eq!(
                        got,
                        Err(CryptoError::InvalidPublicKey("publicKey")),
                        "{line}"
                    );
                }
            }
            "derive_public_key" => {
                let got = api::derive_public_key(f[1], f[2].parse().unwrap(), f[3]);
                if flag(f[4]) {
                    assert_eq!(got.as_deref(), Ok(f[5]), "{line}");
                } else {
                    assert!(got.is_err(), "{line}");
                }
            }
            "derive_secret_key" => {
                let got = api::derive_secret_key(f[1], f[2].parse().unwrap(), f[3]);
                assert_eq!(got.as_deref(), Ok(f[4]), "{line}");
            }
            "generate_key_image" => {
                assert_eq!(
                    api::generate_key_image(f[1], f[2]).as_deref(),
                    Ok(f[3]),
                    "{line}"
                );
            }
            "check_ring_signature" => {
                let n: usize = f[3].parse().unwrap();
                let ring: Vec<&str> = f[4..4 + n].to_vec();
                let joined = f[4 + n];
                let sigs: Vec<&str> = (0..n).map(|i| &joined[i * 128..(i + 1) * 128]).collect();
                assert_eq!(
                    api::check_ring_signature(f[1], f[2], &ring, &sigs),
                    flag(f[5 + n]),
                    "{line}"
                );
            }
            other => panic!("unexpected command {other}"),
        }
        checked += 1;
    }
    assert!(checked >= 150, "only {checked} vectors");
}

// ---------------------------------------------------------------------------------------------
// CryptoNight

fn cryptonight_vectors() -> Vec<(String, String)> {
    include_str!("../../../docs/test-vectors/cryptonight.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (name, value) = l.split_once('=').unwrap();
            (
                name.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            )
        })
        .collect()
}

fn expected(vectors: &[(String, String)], name: &str) -> String {
    vectors
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no vector {name}"))
        .1
        .clone()
}

#[test]
fn cryptonight_family() {
    let vectors = cryptonight_vectors();
    assert_eq!(expected(&vectors, "INPUT_DATA"), TESTDATA);
    assert_eq!(
        api::cn_fast_hash(TESTDATA).unwrap(),
        expected(&vectors, "CN_FAST_HASH")
    );
    assert_eq!(
        api::cn_turtle_lite_slow_hash_v2(TESTDATA).unwrap(),
        expected(&vectors, "CN_TURTLE_LITE_SLOW_HASH_V2")
    );
    assert_eq!(
        api::cn_turtle_lite_slow_hash_v2(TESTDATA).unwrap(),
        "b2172ec9466e1aee70ec8572a14c233ee354582bcb93f869d429744de5726a26"
    );
    for (family, prefix) in [
        (CnFamily::Cn, "CN_SLOW_HASH"),
        (CnFamily::Lite, "CN_LITE_SLOW_HASH"),
        (CnFamily::Dark, "CN_DARK_SLOW_HASH"),
        (CnFamily::DarkLite, "CN_DARK_LITE_SLOW_HASH"),
        (CnFamily::Turtle, "CN_TURTLE_SLOW_HASH"),
        (CnFamily::TurtleLite, "CN_TURTLE_LITE_SLOW_HASH"),
    ] {
        for variant in 0..=2u8 {
            let name = format!("{prefix}_V{variant}");
            assert_eq!(
                api::cn_slow_hash(TESTDATA, family, variant).unwrap(),
                expected(&vectors, &name),
                "{name}"
            );
        }
    }
    assert_eq!(
        api::cn_slow_hash("00", CnFamily::Lite, 1),
        Err(CryptoError::DataCanNotBeHashed)
    );
}

#[test]
fn pow_share_of_the_test_blob_with_the_easiest_target() {
    // target ffffffff: every hash meets it, so the first nonce wins
    let search = pow::find_pow_share(TESTDATA, "ffffffff", 7, 100, 1).unwrap();
    let share = search.share.unwrap();
    assert_eq!(share.nonce, "07000000");
    assert_eq!(search.hashes_performed, 1);
    assert_eq!(search.threads, 1);
    // the nonce of TESTDATA is at offset 39 (header 1 + 1 + 5 bytes, prev id 32)
    let mut blob = hex::decode(TESTDATA).unwrap();
    assert_eq!(pow::nonce_offset(&blob), 39);
    blob[39..43].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(
        share.result,
        api::cn_turtle_lite_slow_hash_v2(&hex::encode(&blob)).unwrap()
    );
}

// ---------------------------------------------------------------------------------------------
// transaction version 2

fn load(name: &str) -> Value {
    let text = match name {
        "clsag" => include_str!("../../../docs/test-vectors/tx-v2-clsag.json"),
        "view_tags" => include_str!("../../../docs/test-vectors/tx-v2-view-tags.json"),
        other => panic!("unknown vector file {other}"),
    };
    serde_json::from_str(text).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap()
}

#[test]
fn clsag_vectors() {
    let vectors = load("clsag");
    let list = vectors["vectors"].as_array().unwrap();
    assert!(list.len() >= 10);
    for v in list {
        let name = str_of(v, "name");
        let ring = strs(&v["ring"]);
        let index = v["signer_index"].as_u64().unwrap();
        let secret = str_of(v, "secret");
        assert_eq!(
            api::secret_key_to_public_key(secret).unwrap(),
            ring[index as usize],
            "{name}"
        );
        let image = api::generate_key_image(&ring[index as usize], secret).unwrap();
        assert_eq!(image, str_of(v, "key_image"), "{name}");
        let c1 = str_of(v, "c1");
        let s = strs(&v["s"]);
        let message = str_of(v, "message");
        assert!(api::clsag_verify(message, &ring, &image, c1, &s), "{name}");
        let decoys = strs(&v["decoy_responses"]);
        let alpha = str_of(v, "alpha");
        let signed = api::clsag_sign(message, &ring, secret, index, alpha, &decoys).unwrap();
        assert_eq!(signed.c1, c1, "{name}");
        assert_eq!(signed.s, s, "{name}");
    }
    let negative = vectors["negative"].as_array().unwrap();
    assert_eq!(negative.len(), 26);
    for n in negative {
        assert_eq!(
            api::clsag_verify(
                str_of(n, "message"),
                &strs(&n["ring"]),
                str_of(n, "key_image"),
                str_of(n, "c1"),
                &strs(&n["s"]),
            ),
            n["expected"].as_bool().unwrap(),
            "{}",
            str_of(n, "name")
        );
    }
}

#[test]
fn view_tag_vectors() {
    let vectors = load("view_tags");
    let monero = vectors["monero"].as_array().unwrap();
    assert_eq!(monero.len(), 70);
    for v in monero {
        assert_eq!(
            api::derive_view_tag(str_of(v, "derivation"), v["index"].as_u64().unwrap()).unwrap(),
            str_of(v, "view_tag")
        );
    }
    let wallets = vectors["wallets"].as_array().unwrap();
    for tx in vectors["transactions"].as_array().unwrap() {
        let wallet = wallets
            .iter()
            .find(|w| w["name"] == tx["wallet"])
            .expect("wallet of the transaction");
        let derivation = api::generate_key_derivation(
            str_of(tx, "tx_public_key"),
            str_of(wallet, "view_secret"),
        )
        .unwrap();
        assert_eq!(derivation, str_of(tx, "derivation"));
        for tag in tx["tags"].as_array().unwrap() {
            assert_eq!(
                api::derive_view_tag(&derivation, tag["index"].as_u64().unwrap()).unwrap(),
                str_of(tag, "view_tag")
            );
        }
    }
}
