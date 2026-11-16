//! The decrypted cache of a `WalletGreen` container, up to the transfers.
//!
//! Mirrors `src/wallet/wallet_serialization_v2.cpp` `WalletSerializerV2::load`, in order:
//! 1. `saveLevel` (`WalletSaveLevel`, `include/iwallet.h`);
//! 2. the key list (`loadKeyListAndBalances`), with balances only at `SAVE_ALL`;
//! 3. at `SAVE_KEYS_AND_TRANSACTIONS` or `SAVE_ALL`: the transactions
//!    (`WalletTransactionDtoV2`) and the transfers (`WalletTransferDtoV2` per transaction id);
//! 4. at `SAVE_ALL`: the transfers-synchronizer blob, unlock jobs and uncommitted
//!    transactions, then `extra`.
//!
//! Part 4 is not read: the outputs it caches are found again by a rescan from the creation
//! timestamps, which C++ also does when the cache cannot be used. [`encode_cache`] writes
//! parts 1 to 3.

use xkr_types::{Hash, PublicKey};

use super::binary::{BinaryError, Reader};

/// C++ `WalletSaveLevel::SAVE_KEYS_ONLY`.
pub const SAVE_KEYS_ONLY: u8 = 0;
/// C++ `WalletSaveLevel::SAVE_KEYS_AND_TRANSACTIONS`.
pub const SAVE_KEYS_AND_TRANSACTIONS: u8 = 1;
/// C++ `WalletSaveLevel::SAVE_ALL`.
pub const SAVE_ALL: u8 = 2;

/// C++ `WALLET_UNCONFIRMED_TRANSACTION_HEIGHT` (`include/iwallet.h`).
pub const WALLET_UNCONFIRMED_TRANSACTION_HEIGHT: u32 = u32::MAX;

/// C++ `WalletTransactionState` values (`include/iwallet.h`).
pub mod transaction_state {
    /// `SUCCEEDED`
    pub const SUCCEEDED: u8 = 0;
    /// `FAILED`
    pub const FAILED: u8 = 1;
    /// `CANCELLED`
    pub const CANCELLED: u8 = 2;
    /// `CREATED` (a delayed transaction)
    pub const CREATED: u8 = 3;
    /// `DELETED`
    pub const DELETED: u8 = 4;
}

/// C++ `WalletTransferType` values (`include/iwallet.h`).
pub mod transfer_type {
    /// `USUAL`
    pub const USUAL: u8 = 0;
    /// `DONATION`
    pub const DONATION: u8 = 1;
    /// `CHANGE`
    pub const CHANGE: u8 = 2;
}

/// What the cache holds up to the transfers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GreenHistory {
    /// C++ `WalletSaveLevel` the cache was written with.
    pub save_level: u8,
    /// Spend public keys in the cache, in order, with balances at `SAVE_ALL`. C++ compares
    /// them with the records to find added and deleted addresses.
    pub cached_keys: Vec<GreenCachedKey>,
    /// Transactions; the index in this list is the transaction id the transfers refer to.
    pub transactions: Vec<GreenTransaction>,
    /// Transfers (destinations and own amounts) per transaction id.
    pub transfers: Vec<GreenTransfer>,
}

/// One entry of the cached key list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GreenCachedKey {
    /// Spend public key of the address.
    pub spend_public_key: PublicKey,
    /// `(actualBalance, pendingBalance)`, present at `SAVE_ALL`.
    pub balances: Option<(u64, u64)>,
}

/// C++ `WalletTransactionDtoV2`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GreenTransaction {
    /// A [`transaction_state`] value.
    pub state: u8,
    /// Block timestamp, 0 while unconfirmed.
    pub timestamp: u64,
    /// Block height, [`WALLET_UNCONFIRMED_TRANSACTION_HEIGHT`] while unconfirmed.
    pub block_height: u32,
    /// Transaction hash.
    pub hash: Hash,
    /// Net amount for the wallet (negative for sent).
    pub total_amount: i64,
    /// Fee.
    pub fee: u64,
    /// When the wallet created or first saw the transaction.
    pub creation_time: u64,
    /// Unlock time.
    pub unlock_time: u64,
    /// The transaction's `extra` field.
    pub extra: Vec<u8>,
    /// True for a coinbase transaction.
    pub is_base: bool,
}

/// C++ `WalletTransferDtoV2` with its transaction id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GreenTransfer {
    /// Index into [`GreenHistory::transactions`].
    pub transaction_id: u64,
    /// Destination or own address (base58).
    pub address: String,
    /// Signed amount (C++ stores the `int64_t` as `uint64_t`).
    pub amount: i64,
    /// A [`transfer_type`] value.
    pub transfer_type: u8,
}

/// Smallest encoded size of a cached key, a transaction and a transfer; used to reject counts
/// that cannot fit before allocating.
const MIN_KEY_SIZE: u64 = 32;
const MIN_TRANSACTION_SIZE: u64 = 1 + 1 + 1 + 32 + 1 + 1 + 1 + 1 + 1 + 1;
const MIN_TRANSFER_SIZE: u64 = 1 + 1 + 1 + 1;

/// Reads the cache (the decrypted `encryptedContainer`) up to the transfers.
pub fn parse_cache(cache: &[u8]) -> Result<GreenHistory, BinaryError> {
    let mut r = Reader::new(cache);
    let save_level = r.u8("saveLevel")?;

    let wallet_count = checked_count(&mut r, "walletCount", MIN_KEY_SIZE)?;
    let mut cached_keys = Vec::with_capacity(wallet_count);
    for _ in 0..wallet_count {
        let spend_public_key = PublicKey(r.pod("spendPublicKey")?);
        let balances = if save_level == SAVE_ALL {
            Some((r.u64("actualBalance")?, r.u64("pendingBalance")?))
        } else {
            None
        };
        cached_keys.push(GreenCachedKey {
            spend_public_key,
            balances,
        });
    }

    let mut transactions = Vec::new();
    let mut transfers = Vec::new();
    if save_level == SAVE_KEYS_AND_TRANSACTIONS || save_level == SAVE_ALL {
        let count = checked_count(&mut r, "transactionCount", MIN_TRANSACTION_SIZE)?;
        transactions.reserve(count);
        for _ in 0..count {
            transactions.push(read_transaction(&mut r)?);
        }
        let count = checked_count(&mut r, "transferCount", MIN_TRANSFER_SIZE)?;
        transfers.reserve(count);
        for _ in 0..count {
            let transaction_id = r.u64("transactionId")?;
            let address = String::from_utf8_lossy(&r.string("address")?).into_owned();
            let amount = r.u64("amount")? as i64;
            let transfer_type = r.u8("type")?;
            transfers.push(GreenTransfer {
                transaction_id,
                address,
                amount,
                transfer_type,
            });
        }
    }

    Ok(GreenHistory {
        save_level,
        cached_keys,
        transactions,
        transfers,
    })
}

/// C++ `serialize(WalletTransactionDtoV2&)`.
fn read_transaction(r: &mut Reader<'_>) -> Result<GreenTransaction, BinaryError> {
    let state = r.u8("state")?;
    let timestamp = r.u64("timestamp")?;
    let block_height = read_block_height(r)?;
    let hash = Hash(r.pod("hash")?);
    let total_amount = r.i64("totalAmount")?;
    let fee = r.u64("fee")?;
    let creation_time = r.u64("creationTime")?;
    let unlock_time = r.u64("unlockTime")?;
    let extra = r.string("extra")?;
    let is_base = r.bool("isBase")?;
    Ok(GreenTransaction {
        state,
        timestamp,
        block_height,
        hash,
        total_amount,
        fee,
        creation_time,
        unlock_time,
        extra,
        is_base,
    })
}

/// C++ `serializeBlockHeight` (input side): a `u64` varint; `u64::MAX` means
/// [`WALLET_UNCONFIRMED_TRANSACTION_HEIGHT`], other values above `u32::MAX` are an error.
fn read_block_height(r: &mut Reader<'_>) -> Result<u32, BinaryError> {
    let offset = r.position();
    let height = r.u64("blockHeight")?;
    if height == u64::MAX {
        return Ok(WALLET_UNCONFIRMED_TRANSACTION_HEIGHT);
    }
    u32::try_from(height).map_err(|_| BinaryError {
        what: "blockHeight",
        offset,
        reason: "wrong value",
    })
}

fn checked_count(
    r: &mut Reader<'_>,
    what: &'static str,
    min_size: u64,
) -> Result<usize, BinaryError> {
    let offset = r.position();
    let count = r.u64(what)?;
    if count.saturating_mul(min_size) > r.remaining() as u64 {
        return Err(BinaryError {
            what,
            offset,
            reason: "count exceeds the data",
        });
    }
    Ok(count as usize)
}

/// C++ `WalletSerializerV2::save` for a cache without the synchronizer part: the save level,
/// the key list, at [`SAVE_KEYS_AND_TRANSACTIONS`] the transactions and the transfers, then
/// an empty `extra` (`m_extra`).
///
/// The save level written is [`SAVE_KEYS_ONLY`] or [`SAVE_KEYS_AND_TRANSACTIONS`]; a history
/// read at [`SAVE_ALL`] is written at [`SAVE_KEYS_AND_TRANSACTIONS`] (no balances, no
/// synchronizer data). C++ `WalletGreen::load` reads such a cache and rescans the outputs
/// from the creation timestamps of the addresses.
#[must_use]
pub fn encode_cache(history: &GreenHistory) -> Vec<u8> {
    use super::binary::Writer;
    let level = history.save_level.min(SAVE_KEYS_AND_TRANSACTIONS);
    let mut w = Writer::new();
    w.varint(u64::from(level));
    w.varint(history.cached_keys.len() as u64);
    for k in &history.cached_keys {
        w.bytes(&k.spend_public_key.0);
    }
    if level == SAVE_KEYS_AND_TRANSACTIONS {
        w.varint(history.transactions.len() as u64);
        for t in &history.transactions {
            w.varint(u64::from(t.state));
            w.varint(t.timestamp);
            // C++ serializeBlockHeight writes the uint32_t as is
            w.varint(u64::from(t.block_height));
            w.bytes(&t.hash.0);
            w.i64(t.total_amount);
            w.varint(t.fee);
            w.varint(t.creation_time);
            w.varint(t.unlock_time);
            w.string(&t.extra);
            w.bool(t.is_base);
        }
        w.varint(history.transfers.len() as u64);
        for t in &history.transfers {
            w.varint(t.transaction_id);
            w.string(t.address.as_bytes());
            w.i64(t.amount);
            w.varint(u64::from(t.transfer_type));
        }
    }
    // extra (WalletGreen m_extra)
    w.string(b"");
    w.into_bytes()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample() -> GreenHistory {
        GreenHistory {
            save_level: SAVE_KEYS_AND_TRANSACTIONS,
            cached_keys: vec![
                GreenCachedKey {
                    spend_public_key: PublicKey([1; 32]),
                    balances: None,
                },
                GreenCachedKey {
                    spend_public_key: PublicKey([2; 32]),
                    balances: None,
                },
            ],
            transactions: vec![
                GreenTransaction {
                    state: transaction_state::SUCCEEDED,
                    timestamp: 1_600_000_000,
                    block_height: 123_456,
                    hash: Hash([3; 32]),
                    total_amount: -1_010,
                    fee: 10,
                    creation_time: 1_599_999_990,
                    unlock_time: 0,
                    extra: vec![1, 0xaa, 0xbb],
                    is_base: false,
                },
                GreenTransaction {
                    state: transaction_state::CANCELLED,
                    block_height: WALLET_UNCONFIRMED_TRANSACTION_HEIGHT,
                    hash: Hash([4; 32]),
                    total_amount: 5_000,
                    is_base: true,
                    ..GreenTransaction::default()
                },
            ],
            transfers: vec![
                GreenTransfer {
                    transaction_id: 0,
                    address: "SEKReDestination".to_owned(),
                    amount: 1_000,
                    transfer_type: transfer_type::USUAL,
                },
                GreenTransfer {
                    transaction_id: 0,
                    address: "SEKReOwn".to_owned(),
                    amount: -1_010,
                    transfer_type: transfer_type::CHANGE,
                },
            ],
        }
    }

    #[test]
    fn roundtrip_keys_and_transactions() {
        let h = sample();
        assert_eq!(parse_cache(&encode_cache(&h)).unwrap(), h);
    }

    #[test]
    fn keys_only_has_no_transactions() {
        let h = GreenHistory {
            save_level: SAVE_KEYS_ONLY,
            cached_keys: vec![GreenCachedKey {
                spend_public_key: PublicKey([9; 32]),
                balances: None,
            }],
            ..GreenHistory::default()
        };
        assert_eq!(parse_cache(&encode_cache(&h)).unwrap(), h);
    }

    #[test]
    fn save_all_reads_balances_and_stops_before_the_synchronizer() {
        let mut w = super::super::binary::Writer::new();
        w.varint(u64::from(SAVE_ALL));
        w.varint(1);
        w.bytes(&[7; 32]);
        w.varint(500);
        w.varint(20);
        w.varint(0); // transactions
        w.varint(0); // transfers
        w.string(b"synchronizer data, not parsed");
        let h = parse_cache(&w.into_bytes()).unwrap();
        assert_eq!(h.cached_keys[0].balances, Some((500, 20)));
        assert!(matches!(h.transactions.as_slice(), []));
    }

    #[test]
    fn save_all_is_written_without_balances() {
        let mut h = sample();
        h.save_level = SAVE_ALL;
        for k in &mut h.cached_keys {
            k.balances = Some((5, 6));
        }
        let back = parse_cache(&encode_cache(&h)).unwrap();
        assert_eq!(back.save_level, SAVE_KEYS_AND_TRANSACTIONS);
        assert!(back.cached_keys.iter().all(|k| k.balances.is_none()));
        assert_eq!(back.transactions, h.transactions);
        assert_eq!(back.transfers, h.transfers);
    }

    #[test]
    fn block_height_rules() {
        let mut w = super::super::binary::Writer::new();
        w.varint(u64::MAX);
        w.varint(u64::from(u32::MAX) + 1);
        let data = w.into_bytes();
        let mut r = Reader::new(&data);
        assert_eq!(
            read_block_height(&mut r).unwrap(),
            WALLET_UNCONFIRMED_TRANSACTION_HEIGHT
        );
        assert_eq!(read_block_height(&mut r).unwrap_err().reason, "wrong value");
    }

    #[test]
    fn truncated_cache_fails() {
        let data = encode_cache(&sample());
        for cut in [0, 1, 2, 40, data.len() / 2, data.len() - 2] {
            assert!(parse_cache(&data[..cut]).is_err(), "cut at {cut}");
        }
        // absurd counts
        let mut w = super::super::binary::Writer::new();
        w.varint(1);
        w.varint(u64::MAX);
        assert_eq!(
            parse_cache(&w.into_bytes()).unwrap_err().reason,
            "count exceeds the data"
        );
    }
}
