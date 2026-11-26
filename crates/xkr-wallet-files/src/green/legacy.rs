//! The wallet files the C++ wallets wrote before the version 6 container.
//!
//! C++ `WalletGreen::load` read these until upstream commit 95f1e209 (`Remove WalletLegacy`,
//! October 2018) and converted them to a version 6 container on first open, keeping the old
//! file as `<file>.backup` (`WalletGreen::convertAndLoadWalletFile`). The readers here follow
//! the C++ of that commit's parent:
//!
//! * version 1: the `WalletLegacy` file of simplewallet and the first walletd
//!   (`src/WalletLegacy/WalletLegacySerializer.cpp`, read by `WalletSerializerV1::loadWalletV1`):
//!   the version, an IV and one chacha8-encrypted blob holding `KeysStorage` (creation
//!   timestamp, spend key pair, view key pair), `has_details` and, if set, the transaction
//!   cache (`WalletUserTransactionsCache`: transactions, then transfers);
//! * versions 2 to 5: the `WalletSerializerV1` stream of walletd and zedwallet
//!   (`src/Wallet/WalletSerializationV1.cpp`): the version, an IV, then one encrypted string
//!   per field, each with the next IV (`CryptoContext::incIv`): view public key, view secret
//!   key, address count and records, the `details` and `cache` flags, and with `details` the
//!   transactions and transfers.
//!
//! The rest (balances, the transfers-synchronizer blob, unlock jobs, uncommitted
//! transactions) is not read: as for a version 6 cache, a rescan from the creation timestamps
//! finds the outputs again, and C++ itself dropped that part for versions below 5.
//!
//! All integers are CryptoNote varints, as in the container cache ([`super::binary`]).

use std::io;
use std::path::{Path, PathBuf};

use rand_core::CryptoRng;
use xkr_types::{Hash, PublicKey, SecretKey};
use zeroize::Zeroizing;

use super::binary::{BinaryError, Reader};
use super::cache::{
    GreenCachedKey, GreenHistory, GreenTransaction, GreenTransfer, SAVE_KEYS_AND_TRANSACTIONS,
    checked_count, read_block_height, transaction_state, transfer_type,
};
use super::chacha8::{self, IV_SIZE, KEY_SIZE};
use super::container::inc_iv;
use super::{GreenRecord, GreenViewKeys, GreenWallet};
use crate::FileError;
use crate::atomic::atomic_write;

/// Version of the `WalletLegacy` file (`WalletLegacySerializer::walletSerializationVersion`).
pub const WALLET_LEGACY_VERSION: u8 = 1;
/// Highest `WalletSerializerV1` version (`WalletSerializerV1::SERIALIZATION_VERSION`).
pub const MAX_LEGACY_VERSION: u8 = 5;

/// C++ `WALLET_LEGACY_INVALID_TRANSFER_ID` (`size_t` maximum, 64-bit builds).
const WALLET_LEGACY_INVALID_TRANSFER_ID: u64 = u64::MAX;

/// Encoded size of a public or secret key.
const KEY_LEN: u64 = 32;
/// Smallest encrypted string of a `WalletRecordDto`: length byte, two keys, three varints.
const MIN_RECORD_CIPHER: u64 = 1 + 2 * KEY_LEN + 3;
/// Smallest encrypted transaction (`WalletTransactionDto`): length byte, the hash and seven
/// one-byte fields.
const MIN_TRANSACTION_CIPHER: u64 = 1 + KEY_LEN + 7;
/// Smallest encrypted transfer: the transaction id string and the transfer string.
const MIN_TRANSFER_CIPHERS: u64 = 2 + 1 + 2;
/// Smallest `WalletLegacyTransaction`: the hash, eight one-byte fields and the empty extra.
const MIN_LEGACY_TRANSACTION: u64 = KEY_LEN + 9;
/// Smallest `WalletLegacyTransfer`: empty address and amount.
const MIN_LEGACY_TRANSFER: u64 = 2;

/// The version (1 to 5) of an old wallet file, if `bytes` starts like one. Nothing is
/// decrypted; the check is the framing of the first fields:
/// * version 1: version, 8-byte IV, then a string that runs exactly to the end of the file;
/// * versions 2 to 5: version, 8-byte IV, then two 32-byte strings (the encrypted view keys)
///   and a string of 1 to 10 bytes (the encrypted address count, a varint).
#[must_use]
pub fn legacy_version(bytes: &[u8]) -> Option<u8> {
    let mut r = Reader::new(bytes);
    let version = u8::try_from(r.u32("version").ok()?).ok()?;
    if !(WALLET_LEGACY_VERSION..=MAX_LEGACY_VERSION).contains(&version) {
        return None;
    }
    r.bytes(IV_SIZE, "iv").ok()?;
    if version == WALLET_LEGACY_VERSION {
        let len = r.u64("data").ok()?;
        return (len > 0 && len == r.remaining() as u64).then_some(version);
    }
    for what in ["public_key", "secret_key"] {
        if r.u64(what).ok()? != KEY_LEN {
            return None;
        }
        r.bytes(KEY_LEN as usize, what).ok()?;
    }
    let count_len = r.u64("wallets_count").ok()?;
    ((1..=10).contains(&count_len) && r.remaining() as u64 >= count_len).then_some(version)
}

/// Opens an old wallet file (version 1 to 5, see the module documentation) with its
/// password. The result is what [`super::open`] gives for a version 6 container: the view
/// keys, the addresses and, when the file has its transaction details, the history (at
/// [`SAVE_KEYS_AND_TRANSACTIONS`]). The view keys take the earliest creation timestamp of the
/// addresses.
///
/// Errors as C++ `WalletSerializerV1::load`:
/// * [`FileError::WrongVersion`]: not a version 1 to 5 file;
/// * [`FileError::WrongPassword`]: the view secret key does not give the view public key, a
///   spend secret key does not give its public key, or a tracking address's public key is
///   not a valid point;
/// * [`FileError::MixedTrackingRecords`]: tracking and spend addresses mixed;
/// * [`FileError::WalletFileCorrupted`]: the keys or address records do not parse.
///
/// Details that do not parse are not an error (see [`GreenWallet::cache_error`]): C++ would
/// refuse the file, but the keys are what matters and a rescan rebuilds the rest.
pub fn open_legacy(bytes: &[u8], password: &str) -> Result<GreenWallet, FileError> {
    let version = legacy_version(bytes).ok_or(FileError::WrongVersion)?;
    let key = chacha8::generate_chacha8_key(password);
    if version == WALLET_LEGACY_VERSION {
        open_v1(bytes, &key)
    } else {
        open_v2_to_v5(bytes, &key, version)
    }
}

/// [`open_legacy`] of a file on disk.
pub fn open_legacy_file(path: &Path, password: &str) -> Result<GreenWallet, FileError> {
    let bytes = read(path)?;
    open_legacy(&bytes, password)
}

/// Where [`convert_file`] keeps the old file: `<path>.backup`, as C++.
#[must_use]
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".backup");
    PathBuf::from(name)
}

/// Converts an old wallet file to a version 6 container in place, as C++
/// `WalletGreen::convertAndLoadWalletFile`: opens it ([`open_legacy`]), writes its bytes to
/// [`backup_path`], then replaces `path` with a container holding the same keys, password and
/// history ([`super::write_with_cache`]). Returns the wallet as read.
///
/// Both writes are atomic, and the backup is written first: if the conversion stops part way,
/// `path` still holds the old file or already the new one. A backup that already exists is
/// kept if it holds the same bytes as `path` (an earlier conversion that stopped part way);
/// otherwise the conversion fails, as in C++ (".backup file already exists"), so that an older
/// backup is never overwritten.
pub fn convert_file<R: CryptoRng + ?Sized>(
    path: &Path,
    password: &str,
    rng: &mut R,
) -> Result<GreenWallet, FileError> {
    let bytes = read(path)?;
    let wallet = open_legacy(&bytes, password)?;
    let backup = backup_path(path);
    match std::fs::read(&backup) {
        Ok(existing) if existing == bytes => {}
        Ok(_) => {
            return Err(FileError::InvalidWalletFilename(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", backup.display()),
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            atomic_write(&backup, &bytes).map_err(FileError::InvalidWalletFilename)?;
        }
        Err(e) => return Err(FileError::InvalidWalletFilename(e)),
    }
    super::write_with_cache_file(path, &wallet, password, rng)?;
    Ok(wallet)
}

fn read(path: &Path) -> Result<Vec<u8>, FileError> {
    std::fs::read(path).map_err(FileError::from_open)
}

/// The transactions and transfers of a file's details, `None` without details.
type Details = Result<Option<(Vec<GreenTransaction>, Vec<GreenTransfer>)>, BinaryError>;

fn corrupted(e: &BinaryError) -> FileError {
    FileError::WalletFileCorrupted(e.to_string())
}

/// Checks a spend key pair as `WalletSerializerV1::loadWallets` and
/// `WalletLegacySerializer::deserialize`: a zero secret key is a tracking address and needs a
/// valid public key, any other secret key must give the public key. Returns the secret key
/// for a spend address.
fn check_spend(public: &PublicKey, secret: &SecretKey) -> Result<Option<SecretKey>, FileError> {
    if secret.is_zero() {
        if !xkr_crypto::check_key(public) {
            return Err(FileError::WrongPassword);
        }
        Ok(None)
    } else if xkr_crypto::secret_key_to_public_key(secret) == Some(*public) {
        Ok(Some(*secret))
    } else {
        Err(FileError::WrongPassword)
    }
}

fn check_view(public: &PublicKey, secret: &SecretKey) -> Result<(), FileError> {
    if xkr_crypto::secret_key_to_public_key(secret) == Some(*public) {
        Ok(())
    } else {
        Err(FileError::WrongPassword)
    }
}

fn wallet(
    view_public: PublicKey,
    view_secret: SecretKey,
    records: Vec<GreenRecord>,
    details: Details,
) -> GreenWallet {
    let (history, cache_error) = match details {
        Ok(Some((transactions, transfers))) => (
            Some(GreenHistory {
                save_level: SAVE_KEYS_AND_TRANSACTIONS,
                cached_keys: records
                    .iter()
                    .map(|r| GreenCachedKey {
                        spend_public_key: r.spend_public_key,
                        balances: None,
                    })
                    .collect(),
                transactions,
                transfers,
            }),
            None,
        ),
        Ok(None) => (None, None),
        Err(e) => (None, Some(e.to_string())),
    };
    let creation_timestamp = records
        .iter()
        .map(|r| r.creation_timestamp)
        .min()
        .unwrap_or(0);
    GreenWallet {
        view: GreenViewKeys {
            public_key: view_public,
            secret_key: view_secret,
            creation_timestamp,
        },
        records,
        history,
        cache_error,
    }
}

// ---------------------------------------------------------------------------------------------
// version 1: WalletLegacy

fn open_v1(bytes: &[u8], key: &[u8; KEY_SIZE]) -> Result<GreenWallet, FileError> {
    let mut r = Reader::new(bytes);
    r.u32("version").map_err(|e| corrupted(&e))?;
    let iv: [u8; IV_SIZE] = r.pod("iv").map_err(|e| corrupted(&e))?;
    let cipher = r.string("data").map_err(|e| corrupted(&e))?;
    let plain = Zeroizing::new(chacha8::chacha8(&cipher, key, &iv));
    let mut p = Reader::new(&plain);

    // KeysStorage::serialize; a parse error is a wrong password in C++ (loadKeys)
    let keys = (|| -> Result<_, BinaryError> {
        let creation_timestamp = p.u64("creation_timestamp")?;
        let spend_public = PublicKey(p.pod("spend_public_key")?);
        let spend_secret = SecretKey(p.pod("spend_secret_key")?);
        let view_public = PublicKey(p.pod("view_public_key")?);
        let view_secret = SecretKey(p.pod("view_secret_key")?);
        Ok((
            creation_timestamp,
            spend_public,
            spend_secret,
            view_public,
            view_secret,
        ))
    })()
    .map_err(|_| FileError::WrongPassword)?;
    let (creation_timestamp, spend_public, spend_secret, view_public, view_secret) = keys;
    check_view(&view_public, &view_secret)?;
    let spend_secret_key = check_spend(&spend_public, &spend_secret)?;

    let records = vec![GreenRecord {
        spend_public_key: spend_public,
        spend_secret_key,
        creation_timestamp,
    }];
    let details = read_v1_details(&mut p);
    Ok(wallet(view_public, view_secret, records, details))
}

/// A `WalletLegacyTransaction` with the transfer range it refers to.
struct LegacyTx {
    first_transfer_id: u64,
    transfer_count: u64,
    tx: GreenTransaction,
}

/// `has_details` and `WalletUserTransactionsCache` (transactions, transfers; the unconfirmed
/// transactions and the cache after them are not read), converted as
/// `WalletSerializerV1::addWalletV1Details`.
fn read_v1_details(p: &mut Reader<'_>) -> Details {
    if !p.bool("has_details")? {
        return Ok(None);
    }
    let count = checked_count(p, "transactions", MIN_LEGACY_TRANSACTION)?;
    let mut txs = Vec::with_capacity(count);
    for _ in 0..count {
        // serialize(WalletLegacyTransaction&)
        let first_transfer_id = p.u64("first_transfer_id")?;
        let transfer_count = p.u64("transfer_count")?;
        let total_amount = p.i64("total_amount")?;
        let fee = p.u64("fee")?;
        let hash = Hash(p.pod("hash")?);
        let is_base = p.bool("is_coinbase")?;
        let block_height = read_block_height(p)?;
        let timestamp = p.u64("timestamp")?;
        let unlock_time = p.u64("unlock_time")?;
        let extra = p.string("extra")?;
        txs.push(LegacyTx {
            first_transfer_id,
            transfer_count,
            // convert(const WalletLegacyTransaction&): sentTime is always 0 on load
            tx: GreenTransaction {
                state: transaction_state::SUCCEEDED,
                timestamp,
                block_height,
                hash,
                total_amount,
                fee,
                creation_time: 0,
                unlock_time,
                extra,
                is_base,
            },
        });
    }
    let count = checked_count(p, "transfers", MIN_LEGACY_TRANSFER)?;
    let mut legacy_transfers = Vec::with_capacity(count);
    for _ in 0..count {
        let address = String::from_utf8_lossy(&p.string("address")?).into_owned();
        let amount = p.i64("amount")?;
        legacy_transfers.push((address, amount));
    }

    let mut transactions = Vec::with_capacity(txs.len());
    let mut transfers = Vec::with_capacity(legacy_transfers.len());
    for (id, t) in txs.into_iter().enumerate() {
        if t.first_transfer_id != WALLET_LEGACY_INVALID_TRANSFER_ID && t.transfer_count != 0 {
            let end = t
                .first_transfer_id
                .checked_add(t.transfer_count)
                .filter(|&end| end <= legacy_transfers.len() as u64)
                .ok_or(BinaryError {
                    what: "transfer_count",
                    offset: p.position(),
                    reason: "transfers outside the transfer list",
                })?;
            for (address, amount) in &legacy_transfers[t.first_transfer_id as usize..end as usize] {
                transfers.push(GreenTransfer {
                    transaction_id: id as u64,
                    address: address.clone(),
                    amount: *amount,
                    transfer_type: transfer_type::USUAL,
                });
            }
        }
        transactions.push(t.tx);
    }
    Ok(Some((transactions, transfers)))
}

// ---------------------------------------------------------------------------------------------
// versions 2 to 5: WalletSerializerV1

/// The stream of encrypted strings (`deserializeEncrypted` followed by `incIv`).
struct Ciphers<'a, 'k> {
    r: Reader<'a>,
    key: &'k [u8; KEY_SIZE],
    iv: [u8; IV_SIZE],
}

impl Ciphers<'_, '_> {
    /// The next field, decrypted.
    fn next(&mut self, what: &'static str) -> Result<Zeroizing<Vec<u8>>, BinaryError> {
        let cipher = self.r.string(what)?;
        let plain = Zeroizing::new(chacha8::chacha8(&cipher, self.key, &self.iv));
        inc_iv(&mut self.iv);
        Ok(plain)
    }

    /// The next field, decrypted and read with `f`.
    fn field<T>(
        &mut self,
        what: &'static str,
        f: impl FnOnce(&mut Reader<'_>) -> Result<T, BinaryError>,
    ) -> Result<T, BinaryError> {
        let plain = self.next(what)?;
        f(&mut Reader::new(&plain))
    }

    /// A count read from its own field, checked against the bytes left in the stream.
    fn count(&mut self, what: &'static str, min_size: u64) -> Result<usize, BinaryError> {
        let offset = self.r.position();
        let count = self.field(what, |p| p.u64(what))?;
        if count.saturating_mul(min_size) > self.r.remaining() as u64 {
            return Err(BinaryError {
                what,
                offset,
                reason: "count exceeds the data",
            });
        }
        Ok(count as usize)
    }
}

fn open_v2_to_v5(
    bytes: &[u8],
    key: &[u8; KEY_SIZE],
    version: u8,
) -> Result<GreenWallet, FileError> {
    let mut r = Reader::new(bytes);
    r.u32("version").map_err(|e| corrupted(&e))?;
    let iv: [u8; IV_SIZE] = r.pod("chacha_iv").map_err(|e| corrupted(&e))?;
    let mut c = Ciphers { r, key, iv };

    // loadKeys, checkKeys
    let view_public = c
        .field("public_key", |p| p.pod("public_key").map(PublicKey))
        .map_err(|_| FileError::WrongPassword)?;
    let view_secret = c
        .field("secret_key", |p| p.pod("secret_key").map(SecretKey))
        .map_err(|_| FileError::WrongPassword)?;
    check_view(&view_public, &view_secret)?;

    // loadWallets
    let count = c
        .count("wallets_count", MIN_RECORD_CIPHER)
        .map_err(|e| corrupted(&e))?;
    let mut records = Vec::with_capacity(count);
    let mut tracking = None;
    for _ in 0..count {
        let (public, secret, creation_timestamp) = c
            .field("wallet", |p| {
                let public = PublicKey(p.pod("spend_public_key")?);
                let secret = SecretKey(p.pod("spend_secret_key")?);
                p.u64("pending_balance")?;
                p.u64("actual_balance")?;
                let creation_timestamp = p.u64("creation_timestamp")?;
                Ok((public, secret, creation_timestamp))
            })
            .map_err(|e| corrupted(&e))?;
        let is_tracking = secret.is_zero();
        match tracking {
            None => tracking = Some(is_tracking),
            Some(t) if t != is_tracking => return Err(FileError::MixedTrackingRecords),
            Some(_) => {}
        }
        records.push(GreenRecord {
            spend_public_key: public,
            spend_secret_key: check_spend(&public, &secret)?,
            creation_timestamp,
        });
    }

    let details = read_v1_stream_details(&mut c, version);
    Ok(wallet(view_public, view_secret, records, details))
}

/// `loadFlags`, then with `details` `loadTransactions` and `loadTransfers`, and for versions
/// below 5 `updateTransfersSign`.
fn read_v1_stream_details(c: &mut Ciphers<'_, '_>, version: u8) -> Details {
    let details = c.field("details", |p| p.bool("details"))?;
    c.field("cache", |p| p.bool("cache"))?;
    if !details {
        return Ok(None);
    }

    let count = c.count("transactions_count", MIN_TRANSACTION_CIPHER)?;
    let mut transactions = Vec::with_capacity(count);
    for _ in 0..count {
        // serialize(WalletTransactionDto&)
        transactions.push(c.field("transaction", |p| {
            Ok(GreenTransaction {
                state: p.u8("state")?,
                timestamp: p.u64("timestamp")?,
                block_height: read_block_height(p)?,
                hash: Hash(p.pod("hash")?),
                total_amount: p.i64("total_amount")?,
                fee: p.u64("fee")?,
                creation_time: p.u64("creation_time")?,
                unlock_time: p.u64("unlock_time")?,
                extra: p.string("extra")?,
                is_base: false,
            })
        })?);
    }

    let count = c.count("transfers_count", MIN_TRANSFER_CIPHERS)?;
    let mut transfers = Vec::with_capacity(count);
    for _ in 0..count {
        let transaction_id = c.field("transaction_id", |p| p.u64("transaction_id"))?;
        let transfer = c.field("transfer", |p| {
            let address = String::from_utf8_lossy(&p.string("address")?).into_owned();
            // WalletTransferDto::amount is a uint64_t holding the int64_t
            let amount = p.u64("amount")? as i64;
            let transfer_type = if version > 2 {
                p.u8("type")?
            } else {
                transfer_type::USUAL
            };
            Ok(GreenTransfer {
                transaction_id,
                address,
                amount,
                transfer_type,
            })
        })?;
        transfers.push(transfer);
    }

    if version < 5 {
        // updateTransfersSign: older files kept the destinations negative and the wallet's
        // own entries positive; C++ keeps only the destinations, with a positive amount
        transfers.retain(|t| t.amount < 0);
        for t in &mut transfers {
            t.amount = t.amount.wrapping_neg();
        }
    }
    Ok(Some((transactions, transfers)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::green::binary::Writer;

    /// A fixed test key pair: secret `[n, 0, …]` reduced, public from it.
    fn key_pair(n: u8) -> (PublicKey, SecretKey) {
        let mut s = [0u8; 32];
        s[0] = n;
        let secret = SecretKey(s);
        let public = xkr_crypto::secret_key_to_public_key(&secret).expect("valid scalar");
        (public, secret)
    }

    fn write_legacy_tx(w: &mut Writer, first: u64, count: u64, amount: i64, hash: u8) {
        w.varint(first);
        w.varint(count);
        w.i64(amount);
        w.varint(10); // fee
        w.bytes(&[hash; 32]);
        w.bool(false);
        w.varint(1234); // block height
        w.varint(1_600_000_000); // timestamp
        w.varint(0); // unlock time
        w.string(b"");
    }

    /// `WalletLegacySerializer::serialize`.
    fn v1_file(password: &str, tracking: bool, details: bool) -> Vec<u8> {
        let (spend_public, spend_secret) = key_pair(7);
        let (view_public, view_secret) = key_pair(9);
        let mut p = Writer::new();
        p.varint(1_550_000_000);
        p.bytes(&spend_public.0);
        p.bytes(if tracking {
            &[0u8; 32]
        } else {
            &spend_secret.0
        });
        p.bytes(&view_public.0);
        p.bytes(&view_secret.0);
        p.bool(details);
        if details {
            p.varint(2);
            write_legacy_tx(&mut p, 0, 2, -500, 1);
            write_legacy_tx(&mut p, u64::MAX, 0, 300, 2);
            p.varint(2);
            p.string(b"addrA");
            p.i64(-400);
            p.string(b"addrB");
            p.i64(-100);
            p.varint(0); // unconfirmed
        }
        p.string(b"cache blob");
        let plain = p.into_bytes();
        let iv = [3u8; IV_SIZE];
        let cipher = chacha8::chacha8(&plain, &chacha8::generate_chacha8_key(password), &iv);
        let mut w = Writer::new();
        w.varint(1);
        w.bytes(&iv);
        w.string(&cipher);
        w.into_bytes()
    }

    /// `WalletSerializerV1::save` as far as the reader goes, plus a cache part it skips.
    fn v2_to_v5_file(password: &str, version: u8, records: &[(u8, bool)]) -> Vec<u8> {
        let key = chacha8::generate_chacha8_key(password);
        let mut iv = [5u8; IV_SIZE];
        let mut w = Writer::new();
        w.varint(u64::from(version));
        w.bytes(&iv);
        let mut put = |w: &mut Writer, plain: Writer| {
            w.string(&chacha8::chacha8(&plain.into_bytes(), &key, &iv));
            inc_iv(&mut iv);
        };
        let one = |f: &dyn Fn(&mut Writer)| {
            let mut p = Writer::new();
            f(&mut p);
            p
        };
        let (view_public, view_secret) = key_pair(9);
        put(&mut w, one(&|p: &mut Writer| p.bytes(&view_public.0)));
        put(&mut w, one(&|p: &mut Writer| p.bytes(&view_secret.0)));
        put(
            &mut w,
            one(&|p: &mut Writer| p.varint(records.len() as u64)),
        );
        for &(n, tracking) in records {
            let (public, secret) = key_pair(n);
            put(
                &mut w,
                one(&|p: &mut Writer| {
                    p.bytes(&public.0);
                    p.bytes(if tracking { &[0u8; 32] } else { &secret.0 });
                    p.varint(0);
                    p.varint(0);
                    p.varint(1_550_000_000 + u64::from(n));
                }),
            );
        }
        put(&mut w, one(&|p: &mut Writer| p.bool(true))); // details
        put(&mut w, one(&|p: &mut Writer| p.bool(true))); // cache
        put(&mut w, one(&|p: &mut Writer| p.varint(1)));
        put(
            &mut w,
            one(&|p: &mut Writer| {
                p.varint(0);
                p.varint(1_600_000_000);
                p.varint(1234);
                p.bytes(&[1u8; 32]);
                p.i64(-500);
                p.varint(10);
                p.varint(1_600_000_000);
                p.varint(0);
                p.string(b"");
            }),
        );
        put(&mut w, one(&|p: &mut Writer| p.varint(2)));
        for (address, amount) in [(&b"dest"[..], -490i64), (&b"own"[..], 490)] {
            put(&mut w, one(&|p: &mut Writer| p.varint(0)));
            put(
                &mut w,
                one(&|p: &mut Writer| {
                    p.string(address);
                    p.varint(amount as u64);
                    if version > 2 {
                        p.varint(u64::from(transfer_type::USUAL));
                    }
                }),
            );
        }
        put(&mut w, one(&|p: &mut Writer| p.varint(0))); // balances etc., not read
        w.into_bytes()
    }

    #[test]
    fn version_1_keys_and_history() {
        let file = v1_file("pw", false, true);
        assert_eq!(legacy_version(&file), Some(1));
        let w = open_legacy(&file, "pw").unwrap();
        assert_eq!(w.view.public_key, key_pair(9).0);
        assert_eq!(w.records.len(), 1);
        assert_eq!(w.records[0].spend_public_key, key_pair(7).0);
        assert_eq!(w.records[0].spend_secret_key, Some(key_pair(7).1));
        assert_eq!(w.records[0].creation_timestamp, 1_550_000_000);
        assert_eq!(w.view.creation_timestamp, 1_550_000_000);
        let h = w.history.unwrap();
        assert_eq!(h.save_level, SAVE_KEYS_AND_TRANSACTIONS);
        assert_eq!(h.cached_keys.len(), 1);
        assert_eq!(h.transactions.len(), 2);
        assert_eq!(h.transactions[0].total_amount, -500);
        assert_eq!(h.transactions[1].hash, Hash([2u8; 32]));
        let transfers: Vec<_> = h
            .transfers
            .iter()
            .map(|t| (t.transaction_id, t.address.as_str(), t.amount))
            .collect();
        assert_eq!(transfers, [(0, "addrA", -400), (0, "addrB", -100)]);
    }

    #[test]
    fn version_1_tracking_and_no_details() {
        let w = open_legacy(&v1_file("", true, false), "").unwrap();
        assert!(w.is_tracking());
        assert!(w.history.is_none());
        assert!(w.cache_error.is_none());
    }

    #[test]
    fn version_1_wrong_password() {
        let file = v1_file("pw", false, true);
        assert!(matches!(
            open_legacy(&file, "nope"),
            Err(FileError::WrongPassword)
        ));
    }

    #[test]
    fn versions_2_to_5() {
        for version in 2..=5 {
            let file = v2_to_v5_file("pw", version, &[(7, false), (11, false)]);
            assert_eq!(legacy_version(&file), Some(version));
            let w = open_legacy(&file, "pw").unwrap();
            assert_eq!(w.records.len(), 2, "v{version}");
            assert_eq!(w.records[1].spend_secret_key, Some(key_pair(11).1));
            assert_eq!(w.view.creation_timestamp, 1_550_000_007);
            let h = w.history.unwrap();
            assert_eq!(h.transactions.len(), 1);
            let transfers: Vec<_> = h
                .transfers
                .iter()
                .map(|t| (t.address.as_str(), t.amount))
                .collect();
            if version < 5 {
                assert_eq!(transfers, [("dest", 490)], "v{version}");
            } else {
                assert_eq!(transfers, [("dest", -490), ("own", 490)], "v{version}");
            }
            assert!(matches!(
                open_legacy(&file, "x"),
                Err(FileError::WrongPassword)
            ));
        }
    }

    #[test]
    fn tracking_and_mixed_records() {
        let w = open_legacy(&v2_to_v5_file("", 5, &[(7, true), (11, true)]), "").unwrap();
        assert!(w.is_tracking());
        assert!(matches!(
            open_legacy(&v2_to_v5_file("", 5, &[(7, false), (11, true)]), ""),
            Err(FileError::MixedTrackingRecords)
        ));
    }

    #[test]
    fn damaged_details_keep_the_keys() {
        let mut file = v2_to_v5_file("pw", 5, &[(7, false)]);
        file.truncate(file.len() - 40);
        let w = open_legacy(&file, "pw").unwrap();
        assert_eq!(w.records.len(), 1);
        assert!(w.history.is_none());
        assert!(w.cache_error.is_some());
    }

    #[test]
    fn not_legacy() {
        assert_eq!(legacy_version(b""), None);
        assert_eq!(legacy_version(&[6u8; 100]), None);
        assert_eq!(legacy_version(&[1u8; 20]), None);
        assert_eq!(legacy_version(&[3u8; 20]), None);
        let mut file = v1_file("pw", false, false);
        file.push(0);
        assert_eq!(legacy_version(&file), None);
    }

    #[test]
    fn converts_in_place_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.wallet");
        let old = v2_to_v5_file("pw", 4, &[(7, false)]);
        std::fs::write(&path, &old).unwrap();

        let w = convert_file(&path, "pw", &mut xkr_crypto::os_rng()).unwrap();
        assert_eq!(std::fs::read(backup_path(&path)).unwrap(), old);
        let new = crate::green::open_file(&path, "pw").unwrap();
        assert_eq!(new.records, w.records);
        assert_eq!(new.view, w.view);
        assert_eq!(new.history.unwrap().transfers, w.history.unwrap().transfers);

        // a different backup is never overwritten
        std::fs::write(&path, &old).unwrap();
        std::fs::write(backup_path(&path), b"other").unwrap();
        assert!(matches!(
            convert_file(&path, "pw", &mut xkr_crypto::os_rng()),
            Err(FileError::InvalidWalletFilename(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), old);
    }
}
