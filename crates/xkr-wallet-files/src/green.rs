//! Format B: the `WalletGreen` container of walletd (kryptokrona-service) and zedwallet
//! (xkrwallet).
//!
//! * [`open`] reads the view keys, every address record (public key, secret key or none for a
//!   tracking address, creation timestamp) and, if the cache can be read, the transaction and
//!   transfer history (`src/wallet/wallet_green.cpp` `WalletGreen::load`,
//!   `loadContainerStorage`, `loadSpendKeys`, `loadWalletCache`).
//! * [`write_keys_only`] writes a container with the same keys and no cache (an empty
//!   suffix). C++ opens such a file and rescans from the creation timestamps
//!   (`WalletGreen::load` only reads the cache when `suffixSize() > 0`).
//! * [`write_with_cache`] writes the keys and the transaction history
//!   (`WalletGreen::exportWallet` / `saveWalletCache`, see [`cache::encode_cache`]).

pub mod binary;
pub mod cache;
pub mod chacha8;
pub mod container;

use std::path::Path;

use rand_core::CryptoRng;
use xkr_types::{PublicKey, SecretKey};

pub use cache::{GreenCachedKey, GreenHistory, GreenTransaction, GreenTransfer};

use crate::FileError;
use crate::atomic::atomic_write;
use container::{
    ContainerLayout, INITIAL_CAPACITY, SERIALIZATION_VERSION, decrypt_record, encrypt_record,
    inc_iv,
};

/// The view key pair of a container (C++ `ContainerStoragePrefix::encryptedViewKeys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GreenViewKeys {
    /// Public view key.
    pub public_key: PublicKey,
    /// Private view key.
    pub secret_key: SecretKey,
    /// Creation timestamp stored with the view keys.
    pub creation_timestamp: u64,
}

/// One address of a container (C++ `WalletRecord` as stored in an `EncryptedWalletRecord`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GreenRecord {
    /// Public spend key.
    pub spend_public_key: PublicKey,
    /// Private spend key; `None` for a tracking (view-only) address, stored as all zero
    /// (C++ `NULL_SECRET_KEY`).
    pub spend_secret_key: Option<SecretKey>,
    /// When the address was created; scanning for it starts here.
    pub creation_timestamp: u64,
}

/// What a container holds that this crate reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreenWallet {
    /// The shared view keys.
    pub view: GreenViewKeys,
    /// The addresses, in container order (the first is the one walletd and zedwallet show
    /// first).
    pub records: Vec<GreenRecord>,
    /// History from the cache; `None` if the file has no cache or it could not be read
    /// (C++ then resets the cached data and resynchronises, `WalletGreen::load`).
    pub history: Option<GreenHistory>,
    /// Why the cache could not be read, when the file has one.
    pub cache_error: Option<String>,
}

impl GreenWallet {
    /// True if every address is a tracking (view-only) address. C++ requires all records to
    /// agree (`loadSpendKeys`).
    #[must_use]
    pub fn is_tracking(&self) -> bool {
        !self.records.is_empty() && self.records.iter().all(|r| r.spend_secret_key.is_none())
    }

    /// The earliest creation timestamp of the addresses (the view keys' timestamp if there
    /// are none): a rescan from here finds every output.
    #[must_use]
    pub fn sync_start_timestamp(&self) -> u64 {
        self.records
            .iter()
            .map(|r| r.creation_timestamp)
            .min()
            .unwrap_or(self.view.creation_timestamp)
    }
}

/// Opens a container: `bytes` is the whole file, `password` the container password (may be
/// empty).
///
/// Errors as C++ `WalletGreen::load`:
/// * [`FileError::WrongVersion`]: empty file or first byte not 6;
/// * [`FileError::WalletFileCorrupted`]: the layout does not add up;
/// * [`FileError::WrongPassword`]: the decrypted view secret key does not give the stored view
///   public key, or a tracking record's public key is not a valid point;
/// * [`FileError::MixedTrackingRecords`]: tracking and spend records mixed.
///
/// A record is a spend record when its secret key gives its public key, and a tracking
/// (view-only) record otherwise; the secret field of a tracking record is not used.
///
/// A cache that cannot be read is not an error (see [`GreenWallet::cache_error`]).
pub fn open(bytes: &[u8], password: &str) -> Result<GreenWallet, FileError> {
    let layout = container::parse_layout(bytes)?;
    let key = chacha8::generate_chacha8_key(password);

    let (view_public, view_secret, view_ts) = decrypt_record(&layout.view_keys, &key);
    if xkr_crypto::secret_key_to_public_key(&view_secret) != Some(view_public) {
        return Err(FileError::WrongPassword);
    }

    let mut records = Vec::with_capacity(layout.records.len());
    let mut tracking: Option<bool> = None;
    for encrypted in &layout.records {
        let (spend_public, spend_secret, creation_timestamp) = decrypt_record(encrypted, &key);
        let is_tracking = !is_spend_pair(&spend_public, &spend_secret);
        match tracking {
            None => tracking = Some(is_tracking),
            Some(t) if t != is_tracking => return Err(FileError::MixedTrackingRecords),
            Some(_) => {}
        }
        if is_tracking && !xkr_crypto::check_key(&spend_public) {
            return Err(FileError::WrongPassword);
        }
        records.push(GreenRecord {
            spend_public_key: spend_public,
            spend_secret_key: (!is_tracking).then_some(spend_secret),
            creation_timestamp,
        });
    }

    let (history, cache_error) = if layout.suffix.is_empty() {
        (None, None)
    } else {
        match container::decrypt_suffix(layout.suffix, &key)
            .and_then(|plain| cache::parse_cache(&plain))
        {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e.to_string())),
        }
    };

    Ok(GreenWallet {
        view: GreenViewKeys {
            public_key: view_public,
            secret_key: view_secret,
            creation_timestamp: view_ts,
        },
        records,
        history,
        cache_error,
    })
}

/// Writes a container with the wallet's view keys and records and no cache, encrypted with
/// `password`. The layout is what C++ `WalletGreen::initWithKeys` and `addWallet` produce: a
/// random starting IV for the view record, each following record taking the next IV
/// (`incIv`), `nextIv` left after the last one, at least [`INITIAL_CAPACITY`] record slots.
/// `wallet.history` is not written. Pass `xkr_crypto::os_rng()`.
///
/// Fails with [`FileError::MixedTrackingRecords`] if some records have a spend secret key and
/// others not, which C++ would refuse to open.
pub fn write_keys_only<R: CryptoRng + ?Sized>(
    wallet: &GreenWallet,
    password: &str,
    rng: &mut R,
) -> Result<Vec<u8>, FileError> {
    write_container(wallet, None, password, rng)
}

/// Writes a container like [`write_keys_only`] and, when `wallet.history` is set, its cache
/// after the record slots (C++ `WalletGreen::exportWallet`: `copyContainerStorageKeys`, then
/// `saveWalletCache`, whose `encryptAndSaveContainerData` takes `nextIv` for the cache and
/// advances it). The cache is [`cache::encode_cache`] of the history. Pass
/// `xkr_crypto::os_rng()`.
pub fn write_with_cache<R: CryptoRng + ?Sized>(
    wallet: &GreenWallet,
    password: &str,
    rng: &mut R,
) -> Result<Vec<u8>, FileError> {
    write_container(wallet, wallet.history.as_ref(), password, rng)
}

fn write_container<R: CryptoRng + ?Sized>(
    wallet: &GreenWallet,
    history: Option<&GreenHistory>,
    password: &str,
    rng: &mut R,
) -> Result<Vec<u8>, FileError> {
    let tracking = wallet.records.first().map(|r| r.spend_secret_key.is_none());
    if wallet
        .records
        .iter()
        .any(|r| Some(r.spend_secret_key.is_none()) != tracking)
    {
        return Err(FileError::MixedTrackingRecords);
    }

    let key = chacha8::generate_chacha8_key(password);
    let mut iv = [0u8; chacha8::IV_SIZE];
    rng.fill_bytes(&mut iv);

    let view_keys = encrypt_record(
        &wallet.view.public_key,
        &wallet.view.secret_key,
        wallet.view.creation_timestamp,
        &key,
        &iv,
    );
    inc_iv(&mut iv);

    let mut records = Vec::with_capacity(wallet.records.len());
    for r in &wallet.records {
        let secret = r.spend_secret_key.unwrap_or(SecretKey::ZERO);
        records.push(encrypt_record(
            &r.spend_public_key,
            &secret,
            r.creation_timestamp,
            &key,
            &iv,
        ));
        inc_iv(&mut iv);
    }

    let suffix = match history {
        Some(history) => {
            let plain = zeroize::Zeroizing::new(cache::encode_cache(history));
            let suffix = container::encrypt_suffix(&plain, &key, &iv);
            inc_iv(&mut iv);
            suffix
        }
        None => Vec::new(),
    };

    Ok(container::encode_layout(&ContainerLayout {
        version: SERIALIZATION_VERSION,
        next_iv: iv,
        view_keys,
        capacity: INITIAL_CAPACITY.max(records.len() as u64),
        records,
        suffix: &suffix,
    }))
}

/// True if `secret` is a non-zero secret key that gives `public`.
fn is_spend_pair(public: &PublicKey, secret: &SecretKey) -> bool {
    !secret.is_zero() && xkr_crypto::secret_key_to_public_key(secret) == Some(*public)
}

/// Result of one record check in [`diagnose`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCheck {
    /// Spend record: the secret key gives the stored public key.
    KeyPairMatches,
    /// Tracking record with a zero secret key and a valid public key.
    TrackingValid,
    /// Tracking record with non-zero bytes in the secret key field (C++ `NULL_SECRET_KEY`, see
    /// [`open`]) and a valid public key.
    TrackingNonZeroSecret,
    /// Tracking record whose public key is not a valid point.
    TrackingInvalidPoint,
}

/// What each check of [`open`] finds in a container, for finding out why one does not open.
/// Holds public data only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreenDiagnosis {
    /// First byte, capacity, used records and suffix length, or the layout error.
    pub layout: Result<(u8, u64, usize, usize), String>,
    /// The view record decrypted with the given password's key gives a matching key pair.
    pub view_keys_match: bool,
    /// The view record decrypted with the empty password's key gives a matching key pair
    /// (a container written by C++ `exportWallet` with `encrypt = false`).
    pub view_keys_match_with_empty_password: bool,
    /// Public view key from the view record (given password), whatever the check found.
    pub view_public_key: Option<PublicKey>,
    /// Per record: its IV, public spend key and check result (given password).
    pub records: Vec<([u8; chacha8::IV_SIZE], PublicKey, RecordCheck)>,
    /// The cache: `Ok(save level)`, the read error, or `None` without a cache.
    pub cache: Option<Result<u8, String>>,
}

/// Runs the checks of [`open`] one by one and reports each result instead of stopping at the
/// first failure. Never fails; nothing secret is in the result.
#[must_use]
pub fn diagnose(bytes: &[u8], password: &str) -> GreenDiagnosis {
    let layout = match container::parse_layout(bytes) {
        Ok(l) => l,
        Err(e) => {
            return GreenDiagnosis {
                layout: Err(e.to_string()),
                view_keys_match: false,
                view_keys_match_with_empty_password: false,
                view_public_key: None,
                records: Vec::new(),
                cache: None,
            };
        }
    };
    let key = chacha8::generate_chacha8_key(password);
    let view_check = |k: &[u8; chacha8::KEY_SIZE]| {
        let (public, secret, _) = decrypt_record(&layout.view_keys, k);
        (
            xkr_crypto::secret_key_to_public_key(&secret) == Some(public),
            public,
        )
    };
    let (view_keys_match, view_public) = view_check(&key);
    let view_keys_match_with_empty_password = view_check(&chacha8::generate_chacha8_key("")).0;

    let records = layout
        .records
        .iter()
        .map(|r| {
            let (public, secret, _) = decrypt_record(r, &key);
            let check = if is_spend_pair(&public, &secret) {
                RecordCheck::KeyPairMatches
            } else if !xkr_crypto::check_key(&public) {
                RecordCheck::TrackingInvalidPoint
            } else if secret.is_zero() {
                RecordCheck::TrackingValid
            } else {
                RecordCheck::TrackingNonZeroSecret
            };
            (r.iv, public, check)
        })
        .collect();

    let cache = (!layout.suffix.is_empty()).then(|| {
        container::decrypt_suffix(layout.suffix, &key)
            .and_then(|plain| cache::parse_cache(&plain))
            .map(|h| h.save_level)
            .map_err(|e| e.to_string())
    });

    GreenDiagnosis {
        layout: Ok((
            layout.version,
            layout.capacity,
            layout.records.len(),
            layout.suffix.len(),
        )),
        view_keys_match,
        view_keys_match_with_empty_password,
        view_public_key: Some(view_public),
        records,
        cache,
    }
}

impl core::fmt::Display for GreenDiagnosis {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.layout {
            Ok((version, capacity, used, suffix)) => writeln!(
                f,
                "layout: version {version}, capacity {capacity}, records {used}, suffix {suffix} bytes"
            )?,
            Err(e) => return writeln!(f, "layout: {e}"),
        }
        writeln!(
            f,
            "view keys match: {} (with the empty password: {})",
            self.view_keys_match, self.view_keys_match_with_empty_password
        )?;
        if let Some(k) = &self.view_public_key {
            writeln!(f, "view public key (as decrypted): {k}")?;
        }
        for (i, (iv, public, check)) in self.records.iter().enumerate() {
            writeln!(
                f,
                "record {i}: iv {}, spend public key {public}: {check:?}",
                hex_iv(*iv)
            )?;
        }
        match &self.cache {
            None => writeln!(f, "cache: none"),
            Some(Ok(level)) => writeln!(f, "cache: save level {level}"),
            Some(Err(e)) => writeln!(f, "cache: {e}"),
        }
    }
}

fn hex_iv(iv: [u8; chacha8::IV_SIZE]) -> String {
    use core::fmt::Write as _;
    iv.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Reads and opens a container file. A missing file is [`FileError::FilenameNonExistent`].
pub fn open_file(path: &Path, password: &str) -> Result<GreenWallet, FileError> {
    let bytes = std::fs::read(path).map_err(FileError::from_open)?;
    open(&bytes, password)
}

/// [`write_with_cache`] to `path`, replacing the file in one step ([`crate::atomic_write`]).
pub fn write_with_cache_file<R: CryptoRng + ?Sized>(
    path: &Path,
    wallet: &GreenWallet,
    password: &str,
    rng: &mut R,
) -> Result<(), FileError> {
    let bytes = write_with_cache(wallet, password, rng)?;
    atomic_write(path, &bytes).map_err(FileError::InvalidWalletFilename)
}

/// [`write_keys_only`] to `path`, replacing the file in one step ([`crate::atomic_write`]).
pub fn write_keys_only_file<R: CryptoRng + ?Sized>(
    path: &Path,
    wallet: &GreenWallet,
    password: &str,
    rng: &mut R,
) -> Result<(), FileError> {
    let bytes = write_keys_only(wallet, password, rng)?;
    atomic_write(path, &bytes).map_err(FileError::InvalidWalletFilename)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::green::container::{decrypt_suffix, encrypt_suffix, parse_layout};

    /// A key pair from a small secret (`s * G`).
    fn pair(n: u8) -> (PublicKey, SecretKey) {
        let mut s = [0u8; 32];
        s[0] = n;
        let secret = SecretKey(s);
        (
            xkr_crypto::secret_key_to_public_key(&secret).unwrap(),
            secret,
        )
    }

    fn wallet(tracking: bool, n: u8) -> GreenWallet {
        let (vp, vs) = pair(1);
        GreenWallet {
            view: GreenViewKeys {
                public_key: vp,
                secret_key: vs,
                creation_timestamp: 1_500_000_000,
            },
            records: (0..n)
                .map(|i| {
                    let (p, s) = pair(10 + i);
                    GreenRecord {
                        spend_public_key: p,
                        spend_secret_key: (!tracking).then_some(s),
                        creation_timestamp: 1_600_000_000 + u64::from(i),
                    }
                })
                .collect(),
            history: None,
            cache_error: None,
        }
    }

    #[test]
    fn keys_only_roundtrip() {
        for (tracking, n) in [(false, 1), (false, 12), (true, 1), (true, 3), (false, 0)] {
            let w = wallet(tracking, n);
            let file = write_keys_only(&w, "pw", &mut xkr_crypto::os_rng()).unwrap();
            let layout = parse_layout(&file).unwrap();
            assert_eq!(layout.records.len(), usize::from(n));
            assert_eq!(layout.capacity, INITIAL_CAPACITY.max(u64::from(n)));
            assert!(matches!(layout.suffix, []));
            let back = open(&file, "pw").unwrap();
            assert_eq!(back, w);
            assert_eq!(back.is_tracking(), tracking && n > 0);
        }
    }

    #[test]
    fn ivs_follow_cpp_increment() {
        let w = wallet(false, 3);
        let file = write_keys_only(&w, "", &mut xkr_crypto::os_rng()).unwrap();
        let l = parse_layout(&file).unwrap();
        let mut iv = l.view_keys.iv;
        for r in &l.records {
            inc_iv(&mut iv);
            assert_eq!(r.iv, iv);
        }
        inc_iv(&mut iv);
        assert_eq!(l.next_iv, iv);
    }

    #[test]
    fn wrong_password() {
        let file = write_keys_only(&wallet(false, 2), "right", &mut xkr_crypto::os_rng()).unwrap();
        assert!(matches!(
            open(&file, "wrong"),
            Err(FileError::WrongPassword)
        ));
        assert!(matches!(open(&file, ""), Err(FileError::WrongPassword)));
    }

    #[test]
    fn mixed_records_are_refused() {
        let mut w = wallet(false, 2);
        w.records[1].spend_secret_key = None;
        assert!(matches!(
            write_keys_only(&w, "pw", &mut xkr_crypto::os_rng()),
            Err(FileError::MixedTrackingRecords)
        ));
        // A file with mixed records (built by hand) is refused on open as in C++.
        let key = chacha8::generate_chacha8_key("pw");
        let good = write_keys_only(&wallet(false, 2), "pw", &mut xkr_crypto::os_rng()).unwrap();
        let mut layout = parse_layout(&good).unwrap();
        let (p, _) = pair(11);
        layout.records[1] = encrypt_record(&p, &SecretKey::ZERO, 0, &key, &[0; 8]);
        let file = container::encode_layout(&layout);
        assert!(matches!(
            open(&file, "pw"),
            Err(FileError::MixedTrackingRecords)
        ));
    }

    #[test]
    fn non_matching_secret_is_a_tracking_record() {
        // A tracking record whose secret field is not zero.
        let key = chacha8::generate_chacha8_key("pw");
        let good = write_keys_only(&wallet(false, 2), "pw", &mut xkr_crypto::os_rng()).unwrap();
        let mut layout = parse_layout(&good).unwrap();
        let (p, _) = pair(50);
        let other = SecretKey([
            0xc0, 0x95, 0x2d, 0xad, 0x8d, 0x62, 0, 0, 0xf0, 0x3b, 0xf7, 0xac, 0x8d, 0x62, 0, 0,
            0xe8, 0x3e, 0xd0, 0x58, 0x5c, 0x73, 0, 0, 0x0b, 0xd9, 0xb2, 0x58, 0x5c, 0x73, 0, 0,
        ]);
        layout.records.truncate(1);
        layout.records[0] = encrypt_record(&p, &other, 7, &key, &[0; 8]);
        let w = open(&container::encode_layout(&layout), "pw").unwrap();
        assert_eq!(w.records.len(), 1);
        assert_eq!(w.records[0].spend_public_key, p);
        assert_eq!(w.records[0].spend_secret_key, None);
        assert!(w.is_tracking());
        let d = diagnose(&container::encode_layout(&layout), "pw");
        assert_eq!(d.records[0].2, RecordCheck::TrackingNonZeroSecret);

        // next to a spend record it is a mix
        let mut layout = parse_layout(&good).unwrap();
        layout.records[1] = encrypt_record(&p, &other, 7, &key, &[0; 8]);
        assert!(matches!(
            open(&container::encode_layout(&layout), "pw"),
            Err(FileError::MixedTrackingRecords)
        ));
    }

    #[test]
    fn history_is_read_from_the_cache() {
        let w = wallet(false, 2);
        let file = write_keys_only(&w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        let key = chacha8::generate_chacha8_key("pw");
        let history = cache::tests::sample();
        let plain_cache = cache::encode_cache(&history);
        let suffix = encrypt_suffix(&plain_cache, &key, &[8; 8]);
        let bad_suffix = [1u8, 2, 3];
        let mut layout = parse_layout(&file).unwrap();
        layout.suffix = &suffix;
        let with_cache = container::encode_layout(&layout);
        let stored = parse_layout(&with_cache).unwrap();
        assert_eq!(
            decrypt_suffix(stored.suffix, &key).unwrap().as_slice(),
            plain_cache.as_slice()
        );

        let back = open(&with_cache, "pw").unwrap();
        assert_eq!(back.records, w.records);
        assert_eq!(back.history, Some(history));
        assert_eq!(back.cache_error, None);

        // An unreadable cache leaves the keys usable.
        layout.suffix = &bad_suffix;
        let back = open(&container::encode_layout(&layout), "pw").unwrap();
        assert_eq!(back.records, w.records);
        assert!(back.history.is_none());
        assert!(back.cache_error.is_some());
    }

    #[test]
    fn cache_is_written_and_read_back() {
        let mut w = wallet(false, 2);
        w.history = Some(cache::tests::sample());
        let file = write_with_cache(&w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        let layout = parse_layout(&file).unwrap();
        // the cache took the IV after the last record, nextIv is the one after it
        let mut iv = layout.view_keys.iv;
        for _ in 0..=layout.records.len() {
            inc_iv(&mut iv);
        }
        assert_eq!(&layout.suffix[..8], &iv[..]);
        inc_iv(&mut iv);
        assert_eq!(layout.next_iv, iv);
        let back = open(&file, "pw").unwrap();
        assert_eq!(back, w);
        assert_eq!(
            diagnose(&file, "pw").cache,
            Some(Ok(cache::SAVE_KEYS_AND_TRANSACTIONS))
        );
        // without a history: the keys-only file
        w.history = None;
        let file = write_with_cache(&w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        assert!(matches!(parse_layout(&file).unwrap().suffix, []));
        // tracking records and no records at all
        for (tracking, n) in [(true, 2), (false, 0)] {
            let mut w = wallet(tracking, n);
            w.history = Some(GreenHistory {
                save_level: cache::SAVE_KEYS_AND_TRANSACTIONS,
                ..GreenHistory::default()
            });
            let file = write_with_cache(&w, "", &mut xkr_crypto::os_rng()).unwrap();
            assert_eq!(open(&file, "").unwrap(), w);
        }
    }

    #[test]
    fn diagnosis_reports_each_check() {
        let w = wallet(false, 2);
        let file = write_keys_only(&w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        let d = diagnose(&file, "pw");
        assert!(d.view_keys_match);
        assert!(!d.view_keys_match_with_empty_password);
        assert_eq!(d.records.len(), 2);
        assert!(d.records.iter().all(|r| r.2 == RecordCheck::KeyPairMatches));
        assert_eq!(d.cache, None);
        let text = d.to_string();
        assert!(!text.contains(&w.view.secret_key.to_hex()));
        let d = diagnose(&file, "other");
        assert!(!d.view_keys_match);
        assert!(diagnose(b"", "").layout.is_err());
    }

    #[test]
    fn sync_start_is_the_earliest_record() {
        let w = wallet(false, 3);
        assert_eq!(w.sync_start_timestamp(), 1_600_000_000);
        assert_eq!(wallet(false, 0).sync_start_timestamp(), 1_500_000_000);
    }

    #[test]
    fn files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.wallet");
        assert!(matches!(
            open_file(&path, "pw"),
            Err(FileError::FilenameNonExistent)
        ));
        let mut w = wallet(true, 2);
        write_keys_only_file(&path, &w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        assert_eq!(open_file(&path, "pw").unwrap(), w);
        w.history = Some(cache::tests::sample());
        write_with_cache_file(&path, &w, "pw", &mut xkr_crypto::os_rng()).unwrap();
        assert_eq!(open_file(&path, "pw").unwrap(), w);
    }
}
