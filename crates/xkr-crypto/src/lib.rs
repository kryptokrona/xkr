//! xkr-crypto: the cryptography of the Kryptokrona C++ node (`src/crypto/`), in Rust.
//!
//! | Module | C++ source |
//! |---|---|
//! | [`keccak`] | `keccak.c`, `hash.c` (`cn_fast_hash`) |
//! | [`tree_hash`] | `tree-hash.c` |
//! | [`ops`] | `crypto.cpp` key/derivation/key-image functions, `crypto-ops.c` decoding and `ge_fromfe_frombytes_vartime` |
//! | [`ring`] | `crypto.cpp` `generateRingSignatures` / `checkRingSignature` |
//! | [`clsag`] | single-key CLSAG of transaction version 2 (fork 14, `docs/tx-v2.md`; Monero `rctSigs.cpp` `CLSAG_Gen` / `verRctCLSAGSimple` without `RingCT`) |
//! | [`pow`] | `hash.h` slow-hash entry points, `check_difficulty.cpp` `check_hash` |
//! | [`cryptonight`] | `slow-hash.c` (vendored from Cuprate, MIT; see `VENDORED.md`), plus [`cryptonight::CnHasher`] for miners (reused scratchpad, hardware AES) |
//!
//! Elliptic-curve arithmetic is `curve25519-dalek`; the only hand-written field arithmetic is
//! the private `field` module used by `hash_to_ec`.
#![forbid(unsafe_code)]

pub mod clsag;
pub mod cryptonight;
mod field;
pub mod keccak;
pub mod ops;
pub mod pow;
pub mod ring;
pub mod tree_hash;

pub use clsag::{
    ClsagCheck, clsag_challenge_chain, clsag_sign, clsag_sign_with_randomness, clsag_verify,
    clsag_verify_all, clsag_verify_checked,
};
pub use keccak::{FastHasher, cn_fast_hash, keccak1600};
pub use ops::{
    CheckedKeyImage, check_key, check_signature, decompress, derivation_to_scalar,
    derive_public_key, derive_secret_key, generate_key_derivation, generate_key_image,
    generate_keys, generate_signature, hash_data_to_ec, hash_to_ec, hash_to_scalar,
    key_image_in_prime_subgroup, sc_check, secret_key_to_public_key, underive_public_key, view_tag,
};
pub use pow::{
    check_hash, cn_lite_slow_hash_v1, cn_slow_hash_v0, cn_turtle_lite_slow_hash_v2,
    cn_turtle_lite_slow_hash_v2_reused,
};
pub use ring::{
    RingSignatureCheck, check_ring_signature, check_ring_signature_checked, check_ring_signatures,
    generate_ring_signature,
};
pub use tree_hash::{tree_branch, tree_depth, tree_hash, tree_hash_from_branch};

/// Re-export so callers can name scalars and points without depending on dalek directly.
pub use curve25519_dalek::{edwards::EdwardsPoint, scalar::Scalar};

/// An OS-backed cryptographic RNG for key generation and signing
/// (`rand_core::OsRng` with errors turned into panics, as the C++ `rnd::randomBytes` aborts).
#[must_use]
pub fn os_rng() -> rand_core::UnwrapErr<rand_core::OsRng> {
    use rand_core::TryRngCore as _;
    rand_core::OsRng.unwrap_err()
}
