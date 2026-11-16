//! Output ownership scanning.
//!
//! For a transaction with public key `R` the wallet computes the derivation `D = 8*a*R`
//! (`generate_key_derivation`) once; output `i` is ours iff its key equals
//! `derive_public_key(D, i, B) = H_s(D || i)*G + B`. The one-time secret of an owned output is
//! `derive_secret_key(D, i, b) = H_s(D || i) + b` and its key image is
//! `generate_key_image(P, x) = x * H_p(P)`.
//!
//! Transaction version 2 (fork 14, `docs/tx-v2.md`) adds a view tag per output:
//! [`scan_tagged_outputs`] compares `view_tag(D, i)` (one Keccak) first and only does the point
//! arithmetic of `derive_public_key` for the ~1/256 of foreign outputs whose tag collides.
//!
//! [`scan_for_subwallets`] scans for many subwallets sharing one view key at once (C++ and
//! wallet-backend-js `SubWallets`): it recovers the spend key with `underive_public_key(D, i, P)`
//! and looks it up in [`SpendKeys`]; a view-only subwallet (no private spend key) gets its
//! outputs without one-time secrets or key images.

use std::collections::HashMap;

use xkr_crypto::{
    derive_public_key, derive_secret_key, generate_key_derivation, generate_key_image,
    underive_public_key, view_tag,
};
use xkr_types::{KeyDerivation, KeyImage, PublicKey, SecretKey, TransactionPrefix, tx_public_key};

use crate::keys::WalletKeys;

/// An output that belongs to the wallet.
#[derive(Clone, PartialEq, Eq)]
pub struct FoundOutput {
    /// Index of the output inside its transaction.
    pub index: usize,
    /// Amount.
    pub amount: u64,
    /// One-time public key `P`.
    pub key: PublicKey,
    /// One-time secret `x` with `P = x*G` (needed to sign).
    pub secret: SecretKey,
    /// Key image `x * H_p(P)`; the output is spent once this appears in an input.
    pub key_image: KeyImage,
}

impl std::fmt::Debug for FoundOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FoundOutput")
            .field("index", &self.index)
            .field("amount", &self.amount)
            .field("key", &self.key)
            .field("key_image", &self.key_image)
            .finish_non_exhaustive()
    }
}

/// Scans the outputs `(amount, key)` of one transaction with public key `tx_public_key`.
/// An invalid transaction public key owns nothing.
#[must_use]
pub fn scan_outputs(
    keys: &WalletKeys,
    tx_public_key: &PublicKey,
    outputs: &[(u64, PublicKey)],
) -> Vec<FoundOutput> {
    let tagged: Vec<(u64, PublicKey, Option<u8>)> =
        outputs.iter().map(|(a, k)| (*a, *k, None)).collect();
    scan_tagged_outputs(keys, tx_public_key, &tagged)
}

/// What a scan did, to show the view-tag speed-up (`xkr-testwallet balance`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Outputs looked at.
    pub outputs: u64,
    /// Outputs that carried a view tag (version 2).
    pub tagged: u64,
    /// Tagged outputs rejected by the tag alone (one Keccak, no point arithmetic).
    pub skipped_by_tag: u64,
    /// Outputs that needed the full `derive_public_key` check (untagged ones and tag matches).
    pub full_checks: u64,
}

impl ScanStats {
    /// Adds `other` to `self`.
    pub fn add(&mut self, other: &Self) {
        self.outputs += other.outputs;
        self.tagged += other.tagged;
        self.skipped_by_tag += other.skipped_by_tag;
        self.full_checks += other.full_checks;
    }
}

/// [`scan_outputs`] for outputs `(amount, key, view_tag)`: an output with a view tag is only
/// derived if `view_tag(D, index)` matches (version-2 outputs); `None` (version 0/1 outputs)
/// always takes the full check. The result is the same as without tags, only cheaper.
#[must_use]
pub fn scan_tagged_outputs(
    keys: &WalletKeys,
    tx_public_key: &PublicKey,
    outputs: &[(u64, PublicKey, Option<u8>)],
) -> Vec<FoundOutput> {
    scan_tagged_outputs_with_stats(keys, tx_public_key, outputs).0
}

/// [`scan_tagged_outputs`] that also counts how many outputs the view tags filtered out. An
/// invalid transaction public key owns nothing and counts no checks.
#[must_use]
pub fn scan_tagged_outputs_with_stats(
    keys: &WalletKeys,
    tx_public_key: &PublicKey,
    outputs: &[(u64, PublicKey, Option<u8>)],
) -> (Vec<FoundOutput>, ScanStats) {
    let mut stats = ScanStats {
        outputs: outputs.len() as u64,
        tagged: outputs.iter().filter(|o| o.2.is_some()).count() as u64,
        ..ScanStats::default()
    };
    let Some(derivation) = generate_key_derivation(tx_public_key, &keys.view_secret) else {
        return (Vec::new(), stats);
    };
    let mut found = Vec::new();
    for (index, (amount, key, tag)) in outputs.iter().enumerate() {
        if tag.is_some_and(|t| t != view_tag(&derivation, index as u64)) {
            stats.skipped_by_tag += 1;
            continue;
        }
        stats.full_checks += 1;
        if let Some(f) = check_output(keys, &derivation, index, *amount, key) {
            found.push(f);
        }
    }
    (found, stats)
}

/// Scans every output of a transaction prefix (tx public key from `extra`, view tags if
/// present). A prefix without a valid tx public key owns nothing.
#[must_use]
pub fn scan_transaction(keys: &WalletKeys, prefix: &TransactionPrefix) -> Vec<FoundOutput> {
    let Some(tx_pub) = tx_public_key(&prefix.extra) else {
        return Vec::new();
    };
    let outputs: Vec<(u64, PublicKey, Option<u8>)> = prefix
        .outputs
        .iter()
        .map(|o| (o.amount, *o.key(), o.view_tag()))
        .collect();
    scan_tagged_outputs(keys, &tx_pub, &outputs)
}

// ---------------------------------------------------------------------------------------------
// subwallets: one view key, many spend keys

/// The spend keys of the subwallets that share one private view key (C++ `SubWallets`,
/// wallet-backend-js `SubWallets`): public spend key to private spend key, `None` for a view
/// wallet (no key images then).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpendKeys {
    /// Public spend key to private spend key.
    pub keys: HashMap<PublicKey, Option<SecretKey>>,
}

impl std::fmt::Debug for SpendKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpendKeys")
            .field("subwallets", &self.keys.len())
            .field(
                "view_only",
                &self.keys.values().filter(|s| s.is_none()).count(),
            )
            .finish_non_exhaustive()
    }
}

impl SpendKeys {
    /// No subwallets.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces, as C++ `spendKeys[publicKey] = privateKey` does) a subwallet.
    pub fn insert(&mut self, public: PublicKey, secret: Option<SecretKey>) {
        self.keys.insert(public, secret);
    }

    /// Subwallets of a view wallet: public spend keys only.
    #[must_use]
    pub fn view_only<I: IntoIterator<Item = PublicKey>>(publics: I) -> Self {
        Self {
            keys: publics.into_iter().map(|p| (p, None)).collect(),
        }
    }

    /// Whether `public` is the public spend key of one of the subwallets.
    #[must_use]
    pub fn contains(&self, public: &PublicKey) -> bool {
        self.keys.contains_key(public)
    }

    /// The private spend key of `public`: `None` if it is not ours or view-only.
    #[must_use]
    pub fn secret(&self, public: &PublicKey) -> Option<&SecretKey> {
        self.keys.get(public).and_then(Option::as_ref)
    }

    /// Number of subwallets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether there are no subwallets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl FromIterator<(PublicKey, Option<SecretKey>)> for SpendKeys {
    fn from_iter<I: IntoIterator<Item = (PublicKey, Option<SecretKey>)>>(iter: I) -> Self {
        let mut keys = Self::new();
        for (public, secret) in iter {
            keys.insert(public, secret);
        }
        keys
    }
}

impl From<&WalletKeys> for SpendKeys {
    /// The one subwallet of a single-key wallet.
    fn from(keys: &WalletKeys) -> Self {
        std::iter::once((keys.spend_public, Some(keys.spend_secret))).collect()
    }
}

/// An output that belongs to one of the subwallets.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnedOutput {
    /// Index of the output inside its transaction.
    pub index: usize,
    /// Amount.
    pub amount: u64,
    /// One-time public key `P`.
    pub key: PublicKey,
    /// Public spend key of the owning subwallet.
    pub owner: PublicKey,
    /// Key image `x * H_p(P)`; `None` for a view-only subwallet.
    pub key_image: Option<KeyImage>,
    /// One-time secret `x`; `None` for a view-only subwallet.
    pub secret: Option<SecretKey>,
}

impl std::fmt::Debug for OwnedOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedOutput")
            .field("index", &self.index)
            .field("amount", &self.amount)
            .field("key", &self.key)
            .field("owner", &self.owner)
            .field("key_image", &self.key_image)
            .finish_non_exhaustive()
    }
}

/// Scans the outputs `(amount, key, view_tag)` of one transaction for every subwallet at once
/// (`src/wallet_backend/wallet_synchronizer.cpp` `WalletSynchronizer::processTransactionOutputs`,
/// `SubWallet::getTxInputKeyImage`; wallet-backend-js `processBlockOutputs`).
///
/// The derivation `D = 8*a*R` is computed once; output `i` belongs to the subwallet whose public
/// spend key is `underive_public_key(D, i, P)`. A tagged output whose tag does not match
/// `view_tag(D, i)` is skipped first, so the result is the same as without tags. Outputs come
/// back in index order. An invalid transaction public key owns nothing.
#[must_use]
pub fn scan_for_subwallets(
    view_secret: &SecretKey,
    spend_keys: &SpendKeys,
    tx_public_key: &PublicKey,
    outputs: &[(u64, PublicKey, Option<u8>)],
) -> Vec<OwnedOutput> {
    let Some(derivation) = generate_key_derivation(tx_public_key, view_secret) else {
        return Vec::new();
    };
    outputs
        .iter()
        .enumerate()
        .filter_map(|(index, (amount, key, tag))| {
            scan_subwallet_output(&derivation, index, *amount, key, *tag, spend_keys)
        })
        .collect()
}

/// One output of [`scan_for_subwallets`], for callers that compute the derivation themselves or
/// skip outputs they cannot parse: `None` unless the output belongs to a subwallet. An output
/// key that does not decode owns nothing.
#[must_use]
pub fn scan_subwallet_output(
    derivation: &KeyDerivation,
    index: usize,
    amount: u64,
    key: &PublicKey,
    tag: Option<u8>,
    spend_keys: &SpendKeys,
) -> Option<OwnedOutput> {
    let i = index as u64;
    if tag.is_some_and(|t| t != view_tag(derivation, i)) {
        return None;
    }
    let owner = underive_public_key(derivation, i, key)?;
    if !spend_keys.contains(&owner) {
        return None;
    }
    let (secret, key_image) = match spend_keys.secret(&owner) {
        Some(spend_secret) => {
            let secret = derive_secret_key(derivation, i, spend_secret);
            (Some(secret), Some(generate_key_image(key, &secret)))
        }
        None => (None, None),
    };
    Some(OwnedOutput {
        index,
        amount,
        key: *key,
        owner,
        key_image,
        secret,
    })
}

fn check_output(
    keys: &WalletKeys,
    derivation: &KeyDerivation,
    index: usize,
    amount: u64,
    key: &PublicKey,
) -> Option<FoundOutput> {
    let expected = derive_public_key(derivation, index as u64, &keys.spend_public)?;
    if expected != *key {
        return None;
    }
    let secret = derive_secret_key(derivation, index as u64, &keys.spend_secret);
    let key_image = generate_key_image(key, &secret);
    Some(FoundOutput {
        index,
        amount,
        key: *key,
        secret,
        key_image,
    })
}

#[cfg(test)]
mod tests {
    use xkr_crypto::{generate_keys, os_rng, secret_key_to_public_key};

    use super::*;

    #[test]
    fn finds_outputs_sent_to_the_wallet() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let other = WalletKeys::generate(&mut rng);
        let (tx_pub, tx_sec) = generate_keys(&mut rng);
        let d_me = generate_key_derivation(&me.view_public, &tx_sec).unwrap();
        let d_other = generate_key_derivation(&other.view_public, &tx_sec).unwrap();
        let outputs = vec![
            (
                5,
                derive_public_key(&d_other, 0, &other.spend_public).unwrap(),
            ),
            (7, derive_public_key(&d_me, 1, &me.spend_public).unwrap()),
        ];
        let found = scan_outputs(&me, &tx_pub, &outputs);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].index, found[0].amount), (1, 7));
        assert_eq!(
            secret_key_to_public_key(&found[0].secret),
            Some(outputs[1].1)
        );
        assert!(
            scan_outputs(&other, &tx_pub, &outputs)
                .iter()
                .all(|f| f.index == 0)
        );
    }

    #[test]
    fn view_tags_filter_without_changing_the_result() {
        let mut rng = os_rng();
        let me = WalletKeys::generate(&mut rng);
        let other = WalletKeys::generate(&mut rng);
        let (tx_pub, tx_sec) = generate_keys(&mut rng);
        let d_me = generate_key_derivation(&me.view_public, &tx_sec).unwrap();
        let d_other = generate_key_derivation(&other.view_public, &tx_sec).unwrap();
        let mut outputs = Vec::new();
        for i in 0..20u64 {
            let (d, spend) = if i % 3 == 0 {
                (d_me, me.spend_public)
            } else {
                (d_other, other.spend_public)
            };
            let key = derive_public_key(&d, i, &spend).unwrap();
            outputs.push((i + 1, key, Some(view_tag(&d, i))));
        }
        let mine = scan_tagged_outputs(&me, &tx_pub, &outputs);
        let idx: Vec<usize> = mine.iter().map(|f| f.index).collect();
        assert_eq!(idx, vec![0, 3, 6, 9, 12, 15, 18]);
        let untagged: Vec<(u64, PublicKey)> = outputs.iter().map(|o| (o.0, o.1)).collect();
        assert_eq!(scan_outputs(&me, &tx_pub, &untagged), mine);
        // A wrong tag hides an output (the tag is part of the signed prefix, so a sender that
        // writes a wrong tag only hurts its own recipient).
        let mut wrong = outputs.clone();
        wrong[0].2 = Some(view_tag(&d_me, 0) ^ 1);
        assert_eq!(
            scan_tagged_outputs(&me, &tx_pub, &wrong).len(),
            mine.len() - 1
        );
        // The statistics: 7 of 20 are ours; foreign outputs are skipped unless a tag collides.
        let (found, stats) = scan_tagged_outputs_with_stats(&me, &tx_pub, &outputs);
        assert_eq!(found, mine);
        assert_eq!((stats.outputs, stats.tagged), (20, 20));
        assert_eq!(stats.skipped_by_tag + stats.full_checks, 20);
        assert!(stats.full_checks >= 7);
        let untagged3: Vec<(u64, PublicKey, Option<u8>)> =
            outputs.iter().map(|o| (o.0, o.1, None)).collect();
        let (_, plain) = scan_tagged_outputs_with_stats(&me, &tx_pub, &untagged3);
        assert_eq!(
            (plain.tagged, plain.skipped_by_tag, plain.full_checks),
            (0, 0, 20)
        );
    }

    /// Subwallets of one wallet: the shared view key and `n` random spend key pairs.
    fn subwallets(n: usize) -> (SecretKey, PublicKey, Vec<WalletKeys>) {
        let mut rng = os_rng();
        let base = WalletKeys::generate(&mut rng);
        let subs = (0..n)
            .map(|_| {
                let (_, spend) = generate_keys(&mut rng);
                WalletKeys::from_secrets(&spend, &base.view_secret).unwrap()
            })
            .collect();
        (base.view_secret, base.view_public, subs)
    }

    #[test]
    fn subwallet_scan_equals_the_scan_of_each_key() {
        let mut rng = os_rng();
        let (view_secret, view_public, subs) = subwallets(3);
        let other = WalletKeys::generate(&mut rng);
        let (tx_pub, tx_sec) = generate_keys(&mut rng);
        let d = generate_key_derivation(&view_public, &tx_sec).unwrap();
        let d_other = generate_key_derivation(&other.view_public, &tx_sec).unwrap();
        let mut outputs = Vec::new();
        for i in 0..12u64 {
            let (d, spend) = match i % 4 {
                3 => (d_other, other.spend_public),
                s => (d, subs[s as usize].spend_public),
            };
            let key = derive_public_key(&d, i, &spend).unwrap();
            // half tagged (version 2), half untagged (version 1)
            let tag = i.is_multiple_of(2).then(|| view_tag(&d, i));
            outputs.push((100 + i, key, tag));
        }
        let spend_keys: SpendKeys = subs
            .iter()
            .map(|k| (k.spend_public, Some(k.spend_secret)))
            .collect();
        let found = scan_for_subwallets(&view_secret, &spend_keys, &tx_pub, &outputs);
        assert_eq!(found.len(), 9);
        assert!(found.windows(2).all(|w| w[0].index < w[1].index));
        for sub in &subs {
            let mine: Vec<&OwnedOutput> = found
                .iter()
                .filter(|f| f.owner == sub.spend_public)
                .collect();
            let single = scan_tagged_outputs(sub, &tx_pub, &outputs);
            assert_eq!(mine.len(), single.len());
            for (m, s) in mine.iter().zip(&single) {
                assert_eq!((m.index, m.amount, m.key), (s.index, s.amount, s.key));
                assert_eq!(m.secret, Some(s.secret));
                assert_eq!(m.key_image, Some(s.key_image));
            }
        }
        // one subwallet as `SpendKeys::from(&WalletKeys)` finds the same as the single scan
        let one = scan_for_subwallets(&view_secret, &SpendKeys::from(&subs[1]), &tx_pub, &outputs);
        let single = scan_tagged_outputs(&subs[1], &tx_pub, &outputs);
        assert_eq!(
            one.iter().map(|o| o.index).collect::<Vec<_>>(),
            single.iter().map(|o| o.index).collect::<Vec<_>>()
        );
        // a wrong tag hides the output; another transaction key owns nothing
        let mut wrong = outputs.clone();
        wrong[0].2 = wrong[0].2.map(|t| t ^ 1);
        assert_eq!(
            scan_for_subwallets(&view_secret, &spend_keys, &tx_pub, &wrong).len(),
            8
        );
        let found =
            scan_for_subwallets(&view_secret, &spend_keys, &PublicKey([0xff; 32]), &outputs);
        assert!(matches!(found.as_slice(), []));
    }

    #[test]
    fn view_only_subwallets_get_no_secrets_or_key_images() {
        let mut rng = os_rng();
        let (view_secret, view_public, subs) = subwallets(2);
        let (tx_pub, tx_sec) = generate_keys(&mut rng);
        let d = generate_key_derivation(&view_public, &tx_sec).unwrap();
        let outputs: Vec<(u64, PublicKey, Option<u8>)> = (0..4u64)
            .map(|i| {
                let key = derive_public_key(&d, i, &subs[(i % 2) as usize].spend_public).unwrap();
                (i + 1, key, Some(view_tag(&d, i)))
            })
            .collect();
        let view_only = SpendKeys::view_only(subs.iter().map(|k| k.spend_public));
        let found = scan_for_subwallets(&view_secret, &view_only, &tx_pub, &outputs);
        assert_eq!(found.len(), 4);
        assert!(
            found
                .iter()
                .all(|f| f.key_image.is_none() && f.secret.is_none())
        );
        assert_eq!(found[1].owner, subs[1].spend_public);
        // mixed: one spend subwallet, one view-only
        let mut mixed = view_only.clone();
        mixed.insert(subs[0].spend_public, Some(subs[0].spend_secret));
        assert_eq!(mixed.len(), 2);
        assert!(mixed.contains(&subs[1].spend_public));
        assert_eq!(mixed.secret(&subs[1].spend_public), None);
        let found = scan_for_subwallets(&view_secret, &mixed, &tx_pub, &outputs);
        let with_images: Vec<usize> = found
            .iter()
            .filter(|f| f.key_image.is_some())
            .map(|f| f.index)
            .collect();
        assert_eq!(with_images, vec![0, 2]);
        assert!(SpendKeys::new().is_empty());
    }
}
