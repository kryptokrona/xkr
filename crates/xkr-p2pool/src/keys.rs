//! Deterministic transaction keys and output keys of a share (`src/crypto.cpp`,
//! `src/wallet.cpp`).
//!
//! * [`get_tx_keys`]: C++ `get_tx_keys(pub, sec, seed, prev_id)`: the coinbase key pair of a
//!   share is derived from the sidechain's tx key seed and the main-chain block it builds on, so
//!   every node can recompute it;
//! * [`check_keys`]: C++ `check_keys(pub, sec)`;
//! * [`eph_public_key`]: C++ `Wallet::get_eph_public_key`: the one-time key and view tag of
//!   output `i` paid to a miner wallet.

use xkr_crypto::{
    EdwardsPoint, Scalar, cn_fast_hash, derive_public_key, generate_key_derivation, view_tag,
};
use xkr_types::{Hash, PublicKey, SecretKey};

use crate::wallet::MinerWallet;

/// `15 * l` as 32 little-endian bytes: the largest multiple of the group order below `2^256`
/// (`limit`, `src/crypto.cpp`).
const LIMIT: [u8; 32] = [
    0xe3, 0x6a, 0x67, 0x72, 0x8b, 0xce, 0x13, 0x29, 0x8f, 0x30, 0x82, 0x8c, 0x0b, 0xa4, 0x10, 0x39,
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0,
];

/// Domain of [`get_tx_keys`] (13 bytes, no terminator).
const TX_SECRET_KEY_DOMAIN: &[u8] = b"tx_secret_key";

/// C++ `less32`: `a < b` as little-endian 256-bit integers.
fn less32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().rev().lt(b.iter().rev())
}

/// C++ `generate_keys_deterministic`: `sec = keccak(entropy || counter)` for `counter = 1,
/// 2, …` (32-bit little-endian) until it is below `15·l`, reduced mod `l`, repeated while zero;
/// `pub = sec·G`.
#[must_use]
pub fn generate_keys_deterministic(entropy: &[u8]) -> (PublicKey, SecretKey) {
    let mut counter = 0u32;
    let mut buf = Vec::with_capacity(entropy.len() + 4);
    loop {
        let sec = loop {
            counter = counter.wrapping_add(1);
            buf.clear();
            buf.extend_from_slice(entropy);
            buf.extend_from_slice(&counter.to_le_bytes());
            let h = cn_fast_hash(&buf).0;
            if less32(&h, &LIMIT) {
                break h;
            }
        };
        let s = Scalar::from_bytes_mod_order(sec);
        if s != Scalar::ZERO {
            let p = EdwardsPoint::mul_base(&s);
            return (PublicKey(p.compress().to_bytes()), SecretKey(s.to_bytes()));
        }
    }
}

/// C++ `get_tx_keys(pub, sec, seed, prev_id)`: entropy `"tx_secret_key" || seed || prev_id`.
#[must_use]
pub fn get_tx_keys(seed: &Hash, prev_id: &Hash) -> (PublicKey, SecretKey) {
    let mut entropy = Vec::with_capacity(TX_SECRET_KEY_DOMAIN.len() + 64);
    entropy.extend_from_slice(TX_SECRET_KEY_DOMAIN);
    entropy.extend_from_slice(&seed.0);
    entropy.extend_from_slice(&prev_id.0);
    generate_keys_deterministic(&entropy)
}

/// C++ `check_keys(pub, sec)`: the top byte of `sec` is at most 127 and `sec·G == pub` (the
/// point `sec·G`; G has prime order, so this is `(sec mod l)·G` for every `sec` below `2^255`).
#[must_use]
pub fn check_keys(public: &PublicKey, sec: &SecretKey) -> bool {
    if sec.0[31] > 127 {
        return false;
    }
    let s = Scalar::from_bytes_mod_order(sec.0);
    EdwardsPoint::mul_base(&s).compress().to_bytes() == public.0
}

/// C++ `Wallet::get_eph_public_key`: one-time key `Hs(8rA || i)·G + B` and view tag of output
/// `output_index` paying `wallet`, with `r = tx_secret`. `None` if a key is not a valid point.
#[must_use]
pub fn eph_public_key(
    wallet: &MinerWallet,
    tx_secret: &SecretKey,
    output_index: u64,
) -> Option<(PublicKey, u8)> {
    let derivation = generate_key_derivation(&wallet.view, tx_secret)?;
    let key = derive_public_key(&derivation, output_index, &wallet.spend)?;
    Some((key, view_tag(&derivation, output_index)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_keys() {
        let seed = Hash([7; 32]);
        let prev = Hash([9; 32]);
        let (public, sec) = get_tx_keys(&seed, &prev);
        assert_eq!(get_tx_keys(&seed, &prev), (public, sec));
        assert_ne!(get_tx_keys(&prev, &seed).0, public);
        // Canonical, non-zero, and the matching public key.
        assert!(xkr_crypto::sc_check(&sec.0));
        assert!(sec.0.iter().any(|b| *b != 0));
        assert_eq!(xkr_crypto::secret_key_to_public_key(&sec), Some(public));
        assert!(check_keys(&public, &sec));
        let mut high = sec;
        high.0[31] = 0x80;
        assert!(!check_keys(&public, &high));
        assert!(!check_keys(&PublicKey([1; 32]), &sec));
    }

    #[test]
    fn limit_compare() {
        assert!(less32(&[0; 32], &LIMIT));
        assert!(!less32(&LIMIT, &LIMIT));
        let mut above = LIMIT;
        above[0] += 1;
        assert!(!less32(&above, &LIMIT));
        let mut below = LIMIT;
        below[31] -= 1;
        below[0] = 0xff;
        assert!(less32(&below, &LIMIT));
    }

    #[test]
    fn output_keys_and_view_tags() {
        let (spend, _) = get_tx_keys(&Hash([1; 32]), &Hash([2; 32]));
        let (view, view_sec) = get_tx_keys(&Hash([3; 32]), &Hash([4; 32]));
        let (r_pub, r) = get_tx_keys(&Hash([5; 32]), &Hash([6; 32]));
        let wallet = MinerWallet::assign(spend, view).unwrap();
        for i in [0u64, 1, 127, 128, 1000] {
            let (key, tag) = eph_public_key(&wallet, &r, i).unwrap();
            // The payee finds it with its view key: 8·a·R is the same derivation.
            let d = generate_key_derivation(&r_pub, &view_sec).unwrap();
            assert_eq!(derive_public_key(&d, i, &spend), Some(key));
            assert_eq!(view_tag(&d, i), tag);
        }
    }
}
