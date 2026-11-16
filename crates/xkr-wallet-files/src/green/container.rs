//! The byte layout of a `WalletGreen` container file and its encrypted records.
//!
//! The file is a C++ `common::FileMappedVector<EncryptedWalletRecord>`
//! (`src/common/file_mapped_vector.h`) with a `ContainerStoragePrefix`
//! (`src/wallet/wallet_green.h`, packed):
//!
//! ```text
//! prefix   (89 bytes) = version u8 || nextIv [8] || encryptedViewKeys {iv [8], data [72]}
//! metadata (16 bytes) = capacity u64 LE || size u64 LE
//! records             = capacity x {iv [8], data [72]}; the first `size` are used
//! suffix              = the rest of the file: {suffixIv [8], encryptedContainer} in the binary
//!                       serializer (`WalletGreen::encryptAndSaveContainerData`), or empty
//! record data         = chacha8(key, iv, publicKey [32] || secretKey [32] || creationTimestamp u64 LE)
//! ```
//!
//! Record (de)cryption is `src/wallet/wallet_green.cpp` `WalletGreen::encryptKeyPair` /
//! `decryptKeyPair`; the IV counter is `WalletGreen::incIv`.

use xkr_types::{PublicKey, SecretKey};
use zeroize::{Zeroize as _, Zeroizing};

use super::binary::{BinaryError, Reader, Writer};
use super::chacha8::{IV_SIZE, KEY_SIZE, chacha8_xor};
use crate::FileError;

/// C++ `WalletSerializerV2::SERIALIZATION_VERSION` (also `MIN_VERSION`): the only version
/// C++ opens.
pub const SERIALIZATION_VERSION: u8 = 6;
/// Encrypted part of a record: public key, secret key, creation timestamp.
pub const RECORD_DATA_SIZE: usize = 32 + 32 + 8;
/// C++ `sizeof(EncryptedWalletRecord)`.
pub const RECORD_SIZE: usize = IV_SIZE + RECORD_DATA_SIZE;
/// C++ `sizeof(ContainerStoragePrefix)`.
pub const PREFIX_SIZE: usize = 1 + IV_SIZE + RECORD_SIZE;
/// C++ `FileMappedVector::metadataSize`: capacity and size.
pub const METADATA_SIZE: usize = 16;
/// C++ `FileMappedVector::open` `initialCapacity` of a new container.
pub const INITIAL_CAPACITY: u64 = 10;

/// C++ `EncryptedWalletRecord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptedRecord {
    /// IV of this record.
    pub iv: [u8; IV_SIZE],
    /// Encrypted public key, secret key and timestamp.
    pub data: [u8; RECORD_DATA_SIZE],
}

impl EncryptedRecord {
    fn from_bytes(b: &[u8]) -> Self {
        let mut iv = [0u8; IV_SIZE];
        iv.copy_from_slice(&b[..IV_SIZE]);
        let mut data = [0u8; RECORD_DATA_SIZE];
        data.copy_from_slice(&b[IV_SIZE..RECORD_SIZE]);
        Self { iv, data }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.iv);
        out.extend_from_slice(&self.data);
    }
}

/// A container split into its parts, nothing decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerLayout<'a> {
    /// First byte (C++ `ContainerStoragePrefix::version`).
    pub version: u8,
    /// IV for the next record or suffix (C++ `nextIv`).
    pub next_iv: [u8; IV_SIZE],
    /// The view key pair record.
    pub view_keys: EncryptedRecord,
    /// Number of record slots in the file (C++ `capacity()`); at least `records.len()`.
    pub capacity: u64,
    /// The used records (C++ `size()`), one per address.
    pub records: Vec<EncryptedRecord>,
    /// Everything after the record slots (C++ `suffix()`); empty if no cache was saved.
    pub suffix: &'a [u8],
}

/// Splits a container file into its parts, checking what C++ `WalletGreen::load` and
/// `FileMappedVector::open` check:
/// * empty file, or first byte not [`SERIALIZATION_VERSION`]: [`FileError::WrongVersion`];
/// * shorter than prefix + metadata, `size > capacity`, or shorter than the record slots:
///   [`FileError::WalletFileCorrupted`] (C++ throws a `std::runtime_error` without a wallet
///   error code).
pub fn parse_layout(file: &[u8]) -> Result<ContainerLayout<'_>, FileError> {
    let Some(&version) = file.first() else {
        return Err(FileError::WrongVersion);
    };
    if version != SERIALIZATION_VERSION {
        return Err(FileError::WrongVersion);
    }
    if file.len() < PREFIX_SIZE + METADATA_SIZE {
        return Err(FileError::corrupted("container file is too small"));
    }
    let mut next_iv = [0u8; IV_SIZE];
    next_iv.copy_from_slice(&file[1..=IV_SIZE]);
    let view_keys = EncryptedRecord::from_bytes(&file[1 + IV_SIZE..PREFIX_SIZE]);

    let capacity = le64(&file[PREFIX_SIZE..PREFIX_SIZE + 8]);
    let size = le64(&file[PREFIX_SIZE + 8..PREFIX_SIZE + METADATA_SIZE]);
    if size > capacity {
        return Err(FileError::corrupted(
            "container size is greater than its capacity",
        ));
    }
    let records_start = PREFIX_SIZE + METADATA_SIZE;
    let records_end = usize::try_from(capacity)
        .ok()
        .and_then(|c| c.checked_mul(RECORD_SIZE))
        .and_then(|n| n.checked_add(records_start))
        .filter(|&end| end <= file.len())
        .ok_or_else(|| FileError::corrupted("container file is shorter than its records"))?;

    // size <= capacity and the slots fit in the file, so this cannot overflow.
    let used = size as usize;
    let records = file[records_start..records_start + used * RECORD_SIZE]
        .as_chunks::<RECORD_SIZE>()
        .0
        .iter()
        .map(|b| EncryptedRecord::from_bytes(b))
        .collect();

    Ok(ContainerLayout {
        version,
        next_iv,
        view_keys,
        capacity,
        records,
        suffix: &file[records_end..],
    })
}

/// Writes a container file; unused record slots (`capacity > records.len()`) are zero, as in a
/// file C++ has grown.
#[must_use]
pub fn encode_layout(layout: &ContainerLayout<'_>) -> Vec<u8> {
    let capacity = layout.capacity.max(layout.records.len() as u64);
    let slots = capacity as usize;
    let mut out =
        Vec::with_capacity(PREFIX_SIZE + METADATA_SIZE + slots * RECORD_SIZE + layout.suffix.len());
    out.push(layout.version);
    out.extend_from_slice(&layout.next_iv);
    layout.view_keys.write(&mut out);
    out.extend_from_slice(&capacity.to_le_bytes());
    out.extend_from_slice(&(layout.records.len() as u64).to_le_bytes());
    for record in &layout.records {
        record.write(&mut out);
    }
    out.resize(PREFIX_SIZE + METADATA_SIZE + slots * RECORD_SIZE, 0);
    out.extend_from_slice(layout.suffix);
    out
}

/// C++ `WalletGreen::incIv`: the IV as a native-endian (little-endian on every supported
/// platform) `u64`, plus one, wrapping to 0 after the maximum.
pub fn inc_iv(iv: &mut [u8; IV_SIZE]) {
    *iv = u64::from_le_bytes(*iv).wrapping_add(1).to_le_bytes();
}

/// C++ `WalletGreen::decryptKeyPair`: public key, secret key, creation timestamp.
#[must_use]
pub fn decrypt_record(
    record: &EncryptedRecord,
    key: &[u8; KEY_SIZE],
) -> (PublicKey, SecretKey, u64) {
    let mut plain = record.data;
    chacha8_xor(&mut plain, key, &record.iv);
    let mut public = [0u8; 32];
    public.copy_from_slice(&plain[..32]);
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&plain[32..64]);
    let timestamp = le64(&plain[64..72]);
    plain.zeroize();
    let out = (PublicKey(public), SecretKey(secret), timestamp);
    secret.zeroize();
    out
}

/// C++ `WalletGreen::encryptKeyPair` with an explicit IV.
#[must_use]
pub fn encrypt_record(
    public: &PublicKey,
    secret: &SecretKey,
    creation_timestamp: u64,
    key: &[u8; KEY_SIZE],
    iv: &[u8; IV_SIZE],
) -> EncryptedRecord {
    let mut data = [0u8; RECORD_DATA_SIZE];
    data[..32].copy_from_slice(&public.0);
    data[32..64].copy_from_slice(&secret.0);
    data[64..].copy_from_slice(&creation_timestamp.to_le_bytes());
    chacha8_xor(&mut data, key, iv);
    EncryptedRecord { iv: *iv, data }
}

/// C++ `WalletGreen::loadAndDecryptContainerData`: reads `suffixIv` and `encryptedContainer`
/// from the suffix and decrypts the cache.
pub fn decrypt_suffix(
    suffix: &[u8],
    key: &[u8; KEY_SIZE],
) -> Result<Zeroizing<Vec<u8>>, BinaryError> {
    let mut r = Reader::new(suffix);
    let iv: [u8; IV_SIZE] = r.pod("suffixIv")?;
    let mut data = Zeroizing::new(r.byte_vector("encryptedContainer")?);
    chacha8_xor(&mut data, key, &iv);
    Ok(data)
}

/// C++ `WalletGreen::encryptAndSaveContainerData`: encrypts a cache and serialises
/// `suffixIv` and `encryptedContainer`.
#[must_use]
pub fn encrypt_suffix(cache: &[u8], key: &[u8; KEY_SIZE], iv: &[u8; IV_SIZE]) -> Vec<u8> {
    let mut data = cache.to_vec();
    chacha8_xor(&mut data, key, iv);
    let mut w = Writer::new();
    w.bytes(iv);
    w.byte_vector(&data);
    w.into_bytes()
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(records: usize, capacity: u64, suffix: &[u8]) -> Vec<u8> {
        let rec = EncryptedRecord {
            iv: [1; 8],
            data: [2; 72],
        };
        encode_layout(&ContainerLayout {
            version: SERIALIZATION_VERSION,
            next_iv: [3; 8],
            view_keys: rec,
            capacity,
            records: vec![rec; records],
            suffix,
        })
    }

    #[test]
    fn sizes_match_cpp_structs() {
        assert_eq!(RECORD_SIZE, 80);
        assert_eq!(PREFIX_SIZE, 89);
    }

    #[test]
    fn layout_roundtrip() {
        let file = layout(3, 10, b"tail");
        assert_eq!(file.len(), 89 + 16 + 10 * 80 + 4);
        let l = parse_layout(&file).unwrap();
        assert_eq!(l.version, 6);
        assert_eq!(l.capacity, 10);
        assert_eq!(l.records.len(), 3);
        assert_eq!(l.next_iv, [3; 8]);
        assert_eq!(l.suffix, b"tail");
        assert_eq!(encode_layout(&l), file);
        // capacity below the record count is raised
        let file = layout(2, 0, b"");
        assert_eq!(parse_layout(&file).unwrap().capacity, 2);
    }

    #[test]
    fn layout_errors() {
        assert!(matches!(parse_layout(&[]), Err(FileError::WrongVersion)));
        assert!(matches!(
            parse_layout(&[5; 200]),
            Err(FileError::WrongVersion)
        ));
        assert!(matches!(
            parse_layout(&[7; 200]),
            Err(FileError::WrongVersion)
        ));
        let file = layout(1, 1, b"");
        assert!(matches!(
            parse_layout(&file[..PREFIX_SIZE + METADATA_SIZE - 1]),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            parse_layout(&file[..file.len() - 1]),
            Err(FileError::WalletFileCorrupted(_))
        ));
        // size > capacity
        let mut bad = file.clone();
        bad[PREFIX_SIZE + 8..PREFIX_SIZE + 16].copy_from_slice(&2u64.to_le_bytes());
        assert!(matches!(
            parse_layout(&bad),
            Err(FileError::WalletFileCorrupted(_))
        ));
        // absurd capacity
        let mut bad = file;
        bad[PREFIX_SIZE..PREFIX_SIZE + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            parse_layout(&bad),
            Err(FileError::WalletFileCorrupted(_))
        ));
    }

    #[test]
    fn iv_increment_wraps() {
        let mut iv = [0xff, 0, 0, 0, 0, 0, 0, 0];
        inc_iv(&mut iv);
        assert_eq!(iv, [0, 1, 0, 0, 0, 0, 0, 0]);
        let mut iv = [0xff; 8];
        inc_iv(&mut iv);
        assert_eq!(iv, [0; 8]);
    }

    #[test]
    fn record_roundtrip() {
        let key = [7u8; 32];
        let iv = [9u8; 8];
        let rec = encrypt_record(
            &PublicKey([1; 32]),
            &SecretKey([2; 32]),
            1_600_000_000,
            &key,
            &iv,
        );
        assert_eq!(rec.iv, iv);
        assert_ne!(&rec.data[..32], &[1u8; 32][..]);
        let (p, s, t) = decrypt_record(&rec, &key);
        assert_eq!(p, PublicKey([1; 32]));
        assert_eq!(s, SecretKey([2; 32]));
        assert_eq!(t, 1_600_000_000);
    }

    #[test]
    fn suffix_roundtrip() {
        let key = [5u8; 32];
        let cache: Vec<u8> = (0..=255u8).collect();
        let suffix = encrypt_suffix(&cache, &key, &[4; 8]);
        assert_eq!(&suffix[..8], &[4u8; 8][..]);
        assert_eq!(
            decrypt_suffix(&suffix, &key).unwrap().as_slice(),
            cache.as_slice()
        );
        assert!(decrypt_suffix(&suffix[..20], &key).is_err());
    }
}
