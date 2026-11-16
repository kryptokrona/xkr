//! `cryptonight::CnHasher` against `docs/test-vectors/cryptonight.txt` (C++
//! `src/crypto_test/main.cpp`) and against `cn_slow_hash`, with hardware AES and with the
//! software table rounds.

use xkr_crypto::cryptonight::{
    AesImpl, CnHasher, CnParams, Variant, cn_slow_hash, hardware_aes_available,
};

const PRESETS: [(&str, CnParams); 6] = [
    ("CN", CnParams::CN),
    ("CN_LITE", CnParams::CN_LITE),
    ("CN_DARK", CnParams::CN_DARK),
    ("CN_DARK_LITE", CnParams::CN_DARK_LITE),
    ("CN_TURTLE", CnParams::CN_TURTLE),
    ("CN_TURTLE_LITE", CnParams::CN_TURTLE_LITE),
];
const VARIANTS: [(&str, Variant); 3] = [
    ("V0", Variant::V0),
    ("V1", Variant::V1),
    ("V2", Variant::V2),
];
const IMPLS: [AesImpl; 2] = [AesImpl::Software, AesImpl::Hardware];

/// `NAME = "value"` lines of the vector file.
fn vectors() -> Vec<(String, String)> {
    include_str!("../../../docs/test-vectors/cryptonight.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains(" = "))
        .map(|l| {
            let (name, value) = l.split_once('=').unwrap();
            (
                name.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            )
        })
        .collect()
}

fn expected<'a>(vectors: &'a [(String, String)], name: &str) -> &'a str {
    &vectors
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no vector {name}"))
        .1
}

/// Every vector of the file, through one hasher per preset, variant and AES implementation;
/// the hardware path is the CPU's AES instructions when it has them (printed), the `aes`
/// crate's software rounds otherwise.
#[test]
fn vector_file_with_and_without_hardware_aes() {
    eprintln!("hardware AES available: {}", hardware_aes_available());
    let vectors = vectors();
    let input = hex::decode(expected(&vectors, "INPUT_DATA")).unwrap();
    let mut checked = 0;
    for (pname, params) in PRESETS {
        for (vname, variant) in VARIANTS {
            let want = expected(&vectors, &format!("{pname}_SLOW_HASH_{vname}"));
            for aes in IMPLS {
                let mut hasher = CnHasher::with_aes(variant, params, aes);
                let got = hex::encode(hasher.hash(&input).unwrap());
                assert_eq!(got, want, "{pname} {vname} {aes:?}");
                // The reused scratchpad gives the same hash again.
                let again = hex::encode(hasher.hash(&input).unwrap());
                assert_eq!(again, want, "{pname} {vname} {aes:?} (second hash)");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 36);
}

/// The preset constructors pick the PoW algorithms of the block major versions.
#[test]
fn pow_constructors() {
    let vectors = vectors();
    let input = hex::decode(expected(&vectors, "INPUT_DATA")).unwrap();
    for (mut hasher, name) in [
        (CnHasher::cn_v0(), "CN_SLOW_HASH_V0"),
        (CnHasher::cn_lite_v1(), "CN_LITE_SLOW_HASH_V1"),
        (CnHasher::cn_turtle_lite_v2(), "CN_TURTLE_LITE_SLOW_HASH_V2"),
    ] {
        assert_eq!(hasher.aes(), AesImpl::detect());
        assert_eq!(
            hex::encode(hasher.hash(&input).unwrap()),
            expected(&vectors, name)
        );
    }
}

/// Inputs of many lengths, interleaved on one hasher: identical to `cn_slow_hash` (which
/// starts from a fresh scratchpad each time) for every preset, variant and AES implementation.
#[test]
fn same_as_cn_slow_hash_on_other_inputs() {
    let mut x = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let inputs: Vec<Vec<u8>> = [43usize, 76, 77, 120, 255]
        .iter()
        .map(|&len| (0..len).map(|_| next() as u8).collect())
        .collect();
    for (pname, params) in PRESETS {
        // The 2 MiB presets are the slow ones; two inputs are enough there.
        let take = if params == CnParams::CN || params == CnParams::CN_LITE {
            2
        } else {
            inputs.len()
        };
        for (vname, variant) in VARIANTS {
            let reference: Vec<[u8; 32]> = inputs[..take]
                .iter()
                .map(|i| cn_slow_hash(i, variant, &params).unwrap())
                .collect();
            for aes in IMPLS {
                let mut hasher = CnHasher::with_aes(variant, params, aes);
                for (i, (input, want)) in inputs.iter().zip(&reference).enumerate() {
                    assert_eq!(
                        &hasher.hash(input).unwrap(),
                        want,
                        "{pname} {vname} {aes:?} input {i}"
                    );
                }
                // Back to the first input after the others.
                assert_eq!(&hasher.hash(&inputs[0]).unwrap(), &reference[0]);
            }
        }
    }
}
