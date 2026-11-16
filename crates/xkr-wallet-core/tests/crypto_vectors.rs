//! The CryptoNote primitives the wallet uses, against the reference vectors of
//! `monero/tests/crypto/tests.txt` (a subset in `data/crypto_vectors.txt`). These functions are
//! byte-identical in kryptokrona's `src/crypto/crypto.cpp`.

use xkr_crypto::{
    check_ring_signature, derive_public_key, derive_secret_key, generate_key_derivation,
    generate_key_image, secret_key_to_public_key,
};
use xkr_types::{Hash, KeyDerivation, KeyImage, PublicKey, SecretKey, Signature};

fn b32(s: &str) -> [u8; 32] {
    let v = hex::decode(s).unwrap();
    v.try_into().unwrap()
}

fn flag(s: &str) -> bool {
    match s {
        "true" => true,
        "false" => false,
        other => panic!("not a bool: {other}"),
    }
}

#[test]
fn reference_vectors() {
    let data = include_str!("data/crypto_vectors.txt");
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for line in data
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let f: Vec<&str> = line.split_whitespace().collect();
        let cmd = f[0];
        *counts.entry(cmd).or_default() += 1;
        match cmd {
            "secret_key_to_public_key" => {
                let got = secret_key_to_public_key(&SecretKey(b32(f[1])));
                if flag(f[2]) {
                    assert_eq!(got, Some(PublicKey(b32(f[3]))), "{line}");
                } else {
                    assert_eq!(got, None, "{line}");
                }
            }
            "generate_key_derivation" => {
                let got = generate_key_derivation(&PublicKey(b32(f[1])), &SecretKey(b32(f[2])));
                if flag(f[3]) {
                    assert_eq!(got, Some(KeyDerivation(b32(f[4]))), "{line}");
                } else {
                    assert_eq!(got, None, "{line}");
                }
            }
            "derive_public_key" => {
                let index: u64 = f[2].parse().unwrap();
                let got =
                    derive_public_key(&KeyDerivation(b32(f[1])), index, &PublicKey(b32(f[3])));
                if flag(f[4]) {
                    assert_eq!(got, Some(PublicKey(b32(f[5]))), "{line}");
                } else {
                    assert_eq!(got, None, "{line}");
                }
            }
            "derive_secret_key" => {
                let index: u64 = f[2].parse().unwrap();
                let got =
                    derive_secret_key(&KeyDerivation(b32(f[1])), index, &SecretKey(b32(f[3])));
                assert_eq!(got, SecretKey(b32(f[4])), "{line}");
            }
            "generate_key_image" => {
                let got = generate_key_image(&PublicKey(b32(f[1])), &SecretKey(b32(f[2])));
                assert_eq!(got, KeyImage(b32(f[3])), "{line}");
            }
            "check_ring_signature" => {
                let prefix = Hash(b32(f[1]));
                let image = KeyImage(b32(f[2]));
                let n: usize = f[3].parse().unwrap();
                let pubs: Vec<PublicKey> = (0..n).map(|i| PublicKey(b32(f[4 + i]))).collect();
                let sig_bytes = hex::decode(f[4 + n]).unwrap();
                assert_eq!(sig_bytes.len(), 64 * n);
                let sigs: Vec<Signature> = sig_bytes
                    .chunks(64)
                    .map(|c| {
                        let mut s = [0u8; 64];
                        s.copy_from_slice(c);
                        Signature(s)
                    })
                    .collect();
                assert_eq!(
                    check_ring_signature(&prefix, &image, &pubs, &sigs),
                    flag(f[5 + n]),
                    "{line}"
                );
            }
            other => panic!("unexpected command {other}"),
        }
    }
    for cmd in [
        "secret_key_to_public_key",
        "generate_key_derivation",
        "derive_public_key",
        "derive_secret_key",
        "generate_key_image",
        "check_ring_signature",
    ] {
        assert!(
            counts.get(cmd).copied().unwrap_or(0) >= 20,
            "too few {cmd} vectors: {counts:?}"
        );
    }
}
