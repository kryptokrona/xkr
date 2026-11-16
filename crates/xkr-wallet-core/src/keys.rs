//! Wallet key pairs (`AccountBase` in `src/cryptonote_core/account.cpp`).
//!
//! A deterministic wallet has a random spend secret and a view secret derived from it
//! (`generateViewFromSpend`): `view_secret = sc_reduce32(keccak256(spend_secret))`, which is
//! exactly `hash_to_scalar(spend_secret)`. Mnemonic seeds encode the spend secret only.

use rand_core::CryptoRng;
use xkr_crypto::{
    EdwardsPoint, Scalar, cn_fast_hash, generate_keys, hash_to_scalar, secret_key_to_public_key,
};
use xkr_types::{PublicKey, SecretKey};

use crate::address::Address;

/// C++ `AccountBase::generateViewFromSpend`: the view key pair of a deterministic wallet.
#[must_use]
pub fn view_key_from_spend(spend_secret: &SecretKey) -> (SecretKey, PublicKey) {
    let view_secret = SecretKey(hash_to_scalar(&spend_secret.0).to_bytes());
    // A reduced scalar is canonical, so this cannot fail.
    let view_public = secret_key_to_public_key(&view_secret).unwrap_or_default();
    (view_secret, view_public)
}

/// Whether `view_secret` is the view key derived from `spend_secret`
/// (`src/wallet_backend/wallet_backend.cpp` `WalletBackend::getMnemonicSeedForAddress`: only
/// then does the address have a mnemonic seed; otherwise C++ returns `KEYS_NOT_DETERMINISTIC`).
#[must_use]
pub fn is_deterministic(spend_secret: &SecretKey, view_secret: &SecretKey) -> bool {
    view_key_from_spend(spend_secret).0 == *view_secret
}

/// Deterministic subwallet keys (kryptokrona-crypto `generateDeterministicSubwalletKeys`:
/// turtlecoin-crypto `generate_deterministic_subwallet_key` followed by
/// `secret_key_to_public_key`).
///
/// `tmp = base || le64(index)`; `index` times `tmp[0..32] = cn_fast_hash(tmp)`; the private key
/// is `sc_reduce32(tmp[0..32])`. Index 0 returns the reduced base key. The work is linear in
/// the index, as in C++. `base` is used as raw bytes (it need not be reduced).
///
/// Neither C++ wallet-api nor walletd creates subwallets this way (their subwallets are
/// random); this is for wallets that choose to.
#[must_use]
pub fn deterministic_subwallet_keys(base: &SecretKey, index: u64) -> (SecretKey, PublicKey) {
    let mut tmp = [0u8; 40];
    tmp[..32].copy_from_slice(&base.0);
    tmp[32..].copy_from_slice(&index.to_le_bytes());
    for _ in 0..index {
        let h = cn_fast_hash(&tmp);
        tmp[..32].copy_from_slice(&h.0);
    }
    let mut first = [0u8; 32];
    first.copy_from_slice(&tmp[..32]);
    let secret = Scalar::from_bytes_mod_order(first);
    let public = EdwardsPoint::mul_base(&secret).compress().to_bytes();
    (SecretKey(secret.to_bytes()), PublicKey(public))
}

/// The four keys of a wallet.
#[derive(Clone, PartialEq, Eq)]
pub struct WalletKeys {
    /// Private spend key `b`.
    pub spend_secret: SecretKey,
    /// Public spend key `B = b*G`.
    pub spend_public: PublicKey,
    /// Private view key `a`.
    pub view_secret: SecretKey,
    /// Public view key `A = a*G`.
    pub view_public: PublicKey,
}

impl std::fmt::Debug for WalletKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletKeys")
            .field("spend_public", &self.spend_public)
            .field("view_public", &self.view_public)
            .finish_non_exhaustive()
    }
}

impl WalletKeys {
    /// A new deterministic wallet (`AccountBase::generate`).
    #[must_use]
    pub fn generate<R: CryptoRng + ?Sized>(rng: &mut R) -> Self {
        let (_, spend_secret) = generate_keys(rng);
        Self::from_spend_secret(&spend_secret).expect("generate_keys returns a reduced scalar")
    }

    /// A deterministic wallet from its spend secret; `None` if the key is not a canonical
    /// scalar.
    #[must_use]
    pub fn from_spend_secret(spend_secret: &SecretKey) -> Option<Self> {
        let spend_public = secret_key_to_public_key(spend_secret)?;
        let (view_secret, view_public) = view_key_from_spend(spend_secret);
        Some(Self {
            spend_secret: *spend_secret,
            spend_public,
            view_secret,
            view_public,
        })
    }

    /// A (possibly non-deterministic) wallet from both secrets.
    #[must_use]
    pub fn from_secrets(spend_secret: &SecretKey, view_secret: &SecretKey) -> Option<Self> {
        Some(Self {
            spend_secret: *spend_secret,
            spend_public: secret_key_to_public_key(spend_secret)?,
            view_secret: *view_secret,
            view_public: secret_key_to_public_key(view_secret)?,
        })
    }

    /// The standard `SEKR…` address.
    #[must_use]
    pub const fn address(&self) -> Address {
        Address::new(self.spend_public, self.view_public)
    }

    /// Whether the view key derives from the spend key, so the wallet has a mnemonic seed
    /// ([`is_deterministic`]).
    #[must_use]
    pub fn is_deterministic(&self) -> bool {
        is_deterministic(&self.spend_secret, &self.view_secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sk(hex: &str) -> SecretKey {
        hex.parse().unwrap()
    }

    #[test]
    fn deterministic_view_key_matches_wallet_backend() {
        // kryptokrona-wallet-backend-js tests: seed wallet "skulls woozy ouch ..."
        let k = WalletKeys::from_spend_secret(&sk(
            "d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904",
        ))
        .unwrap();
        assert_eq!(
            k.view_secret,
            sk("688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801")
        );
    }

    #[test]
    fn spend_public_matches_wallet_backend() {
        let k = WalletKeys::from_secrets(
            &sk("55e0aa4ca65c0ae016c7364eec313f56fc162901ead0e38a9f846686ac78560f"),
            &sk("1f3f6c220dd9f97619dbf44d967f79f3041b9b1c63da2c895f980f1411d5d704"),
        )
        .unwrap();
        assert_eq!(
            k.spend_public.to_string(),
            "ff9b6e048297ee435d6219005974c2c8df620a4aca9ca5c4e13f071823482029"
        );
    }

    #[test]
    fn generated_wallet_round_trips_through_its_address() {
        let k = WalletKeys::generate(&mut xkr_crypto::os_rng());
        let a = Address::parse(&k.address().to_string()).unwrap();
        assert_eq!((a.spend, a.view), (k.spend_public, k.view_public));
        assert_eq!(WalletKeys::from_spend_secret(&k.spend_secret).unwrap(), k);
        assert!(k.is_deterministic());
    }

    #[test]
    fn non_deterministic_keys_are_recognised() {
        // wallet-backend-js tests: "Verifying non deterministic wallet doesn't create seed"
        let k = WalletKeys::from_secrets(
            &sk("55e0aa4ca65c0ae016c7364eec313f56fc162901ead0e38a9f846686ac78560f"),
            &sk("1f3f6c220dd9f97619dbf44d967f79f3041b9b1c63da2c895f980f1411d5d704"),
        )
        .unwrap();
        assert!(!k.is_deterministic());
        assert!(is_deterministic(
            &sk("d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904"),
            &sk("688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801"),
        ));
    }

    #[test]
    fn deterministic_subwallets_match_kryptokrona_crypto() {
        // crates/xkr-crypto-ffi/tests/known_answers.rs `deterministic_subwallets`
        let base = sk("dd0c02d3202634821b4d9d91b63d919725f5c3e97e803f3512e52fb0dc2aab0c");
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
            let (secret, public) = deterministic_subwallet_keys(&base, index);
            assert_eq!(secret, sk(want), "subwallet {index}");
            assert_eq!(secret_key_to_public_key(&secret), Some(public));
        }
    }
}
