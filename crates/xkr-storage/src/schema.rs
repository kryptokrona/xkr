//! Table layout, schema version 2 (`docs/storage.md`).
//!
//! Every key is a fixed-width redb key type (`u32`, `u64`, tuples of integers, `[u8; 32]`),
//! which redb encodes little-endian and compares numerically/lexicographically, so height ranges
//! and `(amount, index)` ranges are contiguous B-tree scans. Fixed-size values are flat
//! `#[repr(C)]` structs without padding stored as their raw bytes through `bytemuck`
//! ([`pod_value!`]); a table whose key and value are both fixed-width has no per-entry
//! overhead in redb's leaves. Blobs are `&[u8]`.
//!
//! | table | key | value | serves |
//! |---|---|---|---|
//! | [`BLOCK_INFOS`] | `height: u32` | [`BlockInfoRow`] (88 B) | headers, difficulty/size windows |
//! | [`BLOCK_HASH_TO_HEIGHT`] | `hash: [u8;32]` | `height: u32` | hash lookups, sparse chain |
//! | [`HOT_BLOCKS`] | `height: u32` | block record (`codec`), uncompressed | the newest blocks (not yet sealed) |
//! | [`HOT_SIGS`] | `height: u32` | the block's ring signatures, concatenated | the newest blocks; dropped by pruning |
//! | [`BODY_PARTS`] | `chunk << 16 \| part: u64` | part of a zstd-compressed 64-block chunk of records | sealed blocks |
//! | [`SIG_PARTS`] | `chunk << 16 \| part: u64` | part of a chunk's concatenated signatures (raw) | sealed blocks; dropped by pruning |
//! | [`TX_HASH_TO_LOC`] | `tx hash: [u8;32]` | [`TxLocRow`] (8 B) | `/get_transactions_status`, `MISSING_TXS` |
//! | [`KEY_IMAGES`] | `key image: [u8;32]` | `()` | double-spend checks |
//! | [`OUTPUTS`] | `(amount: u64, global_index: u32)` | [`OutputRow`] (40 B) | ring members, `/getrandom_outs`, key restoration of stripped blobs |
//! | [`UNLOCK_OVERFLOW`] | `(amount: u64, global_index: u32)` | `unlock_time: u64` | the rare `unlock_time >= u32::MAX` |
//! | [`OUTPUT_COUNTS`] | `amount: u64` | `count: u64` | next global index, random-output ranges |
//! | [`PAYMENT_IDS`] | `(payment_id: [u8;32], height: u32, tx_index: u32)` | `()` | `/get_transaction_hashes_by_payment_id` |
//! | [`TIMESTAMPS`] | `(timestamp: u64, height: u32)` | `()` | wallet sync start by timestamp, `/queryblockslite` day lookup, `/get_blocks_hashes_by_timestamps` |
//! | [`ALT_BLOCKS`] | `hash: [u8;32]` | [`AltBlockHeader`] + framed zstd(raw block) | alternative chains |
//! | [`ALT_BLOCK_HEIGHTS`] | `(height: u64, hash: [u8;32])` | `()` | expiring alt blocks |
//! | [`META`] | `&str` | `u64` | `schema_version`, `top_height`, `pruned_below`, `pruning_seed`, `sealed_below`, `seal_lag` |
//! | [`META_BLOBS`] | `&str` | bytes | `zstd_dictionary`, `zstd_dictionary_prev` |
//!
//! Blocks below `META["sealed_below"]` (a multiple of [`CHUNK_BLOCKS`]) live in
//! [`BODY_PARTS`]/[`SIG_PARTS`]; the newer ones in [`HOT_BLOCKS`]/[`HOT_SIGS`]. A chunk is sealed
//! once its last block is `seal_lag` blocks below the tip, so reorgs normally only touch hot rows.
//!
//! Transaction index 0 within a block is the miner transaction (inside the block blob);
//! `1..=n` follow `Block::tx_hashes`. Global output indexes are per amount: the n-th output of
//! amount `a` ever created has global index `n`.

use bytemuck::{Pod, Zeroable};
use redb::TableDefinition;

#[cfg(target_endian = "big")]
compile_error!(
    "xkr-storage stores bytemuck values in host byte order and assumes a little-endian host"
);

/// Bump on any incompatible layout change; [`crate::Store::open`] refuses other versions.
/// Version 1 was the first per-transaction layout (`docs/storage.md` "v1").
pub const SCHEMA_VERSION: u64 = 2;

/// Consecutive blocks compressed together into one sealed chunk.
pub const CHUNK_BLOCKS: u64 = 64;

/// Default distance (in blocks) between the tip and the newest sealed block. Larger than the
/// node's default `max_reorg_depth` (1000), so reorgs never unseal a chunk in practice.
pub const DEFAULT_SEAL_LAG: u64 = 1_024;

/// Bytes per [`BODY_PARTS`]/[`SIG_PARTS`] value. A leaf holding one such pair (4 B header,
/// 4 B value length, 8 B key, value) is at most 4096 B: every part fills exactly one
/// order-0 page, instead of a large value rounded up to a power-of-two page run.
pub const PART_LEN: usize = 4_064;

/// `META` keys.
pub mod meta {
    pub const SCHEMA_VERSION: &str = "schema_version";
    /// Height of the top block; absent while the chain is empty.
    pub const TOP_HEIGHT: &str = "top_height";
    /// Signatures below this height were pruned: those of every stripe the
    /// [`PRUNING_SEED`] does not keep, or all of them without a seed
    /// ([`crate::pruning::PruneState`]). A multiple of [`crate::pruning::STRIPE_SPAN`] for stripe
    /// pruning, of [`super::CHUNK_BLOCKS`] otherwise; absent = 0.
    pub const PRUNED_BELOW: &str = "pruned_below";
    /// [`crate::pruning::PruningSeed`] of a stripe-pruning node (`stripes << 8 | stripe`);
    /// absent or 0 = no stripe kept below [`PRUNED_BELOW`] (a full node while that is 0).
    /// Added to schema 2 without a version bump: older files simply have no entry.
    pub const PRUNING_SEED: &str = "pruning_seed";
    /// Blocks below this height are sealed into chunks (a multiple of [`super::CHUNK_BLOCKS`]).
    pub const SEALED_BELOW: &str = "sealed_below";
    /// Seal lag of this database (default [`super::DEFAULT_SEAL_LAG`]).
    pub const SEAL_LAG: &str = "seal_lag";
    /// `META_BLOBS` key of the zstd dictionary new chunks are written with.
    pub const ZSTD_DICTIONARY: &str = "zstd_dictionary";
    /// `META_BLOBS` key of the previous dictionary while a re-compression is in progress.
    pub const ZSTD_DICTIONARY_PREV: &str = "zstd_dictionary_prev";
}

pub const BLOCK_INFOS: TableDefinition<u32, BlockInfoRow> = TableDefinition::new("block_infos");
pub const BLOCK_HASH_TO_HEIGHT: TableDefinition<[u8; 32], u32> =
    TableDefinition::new("block_hash_to_height");
pub const HOT_BLOCKS: TableDefinition<u32, &[u8]> = TableDefinition::new("hot_blocks");
pub const HOT_SIGS: TableDefinition<u32, &[u8]> = TableDefinition::new("hot_sigs");
pub const BODY_PARTS: TableDefinition<u64, &[u8]> = TableDefinition::new("body_parts");
pub const SIG_PARTS: TableDefinition<u64, &[u8]> = TableDefinition::new("sig_parts");
pub const TX_HASH_TO_LOC: TableDefinition<[u8; 32], TxLocRow> =
    TableDefinition::new("tx_hash_to_loc");
pub const KEY_IMAGES: TableDefinition<[u8; 32], ()> = TableDefinition::new("key_images");
pub const OUTPUTS: TableDefinition<(u64, u32), OutputRow> = TableDefinition::new("outputs");
pub const UNLOCK_OVERFLOW: TableDefinition<(u64, u32), u64> =
    TableDefinition::new("unlock_overflow");
pub const OUTPUT_COUNTS: TableDefinition<u64, u64> = TableDefinition::new("output_counts");
pub const PAYMENT_IDS: TableDefinition<([u8; 32], u32, u32), ()> =
    TableDefinition::new("payment_ids");
pub const TIMESTAMPS: TableDefinition<(u64, u32), ()> = TableDefinition::new("timestamps");
pub const ALT_BLOCKS: TableDefinition<[u8; 32], &[u8]> = TableDefinition::new("alt_blocks");
pub const ALT_BLOCK_HEIGHTS: TableDefinition<(u64, [u8; 32]), ()> =
    TableDefinition::new("alt_block_heights");
pub const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
pub const META_BLOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("meta_blobs");

/// [`BlockInfo::flags`] bit: the block's signatures were pruned. Derived from
/// `META["pruned_below"]` and `META["pruning_seed"]` when a row is read
/// ([`crate::pruning::PruneState::dropped`]); not stored.
pub const BLOCK_FLAG_PRUNED: u32 = 1;

/// `OutputRow::unlock` value meaning "see [`UNLOCK_OVERFLOW`]".
pub const UNLOCK_ESCAPE: u32 = u32::MAX;

/// Block header data as the rest of the node sees it (consensus windows, RPC). 104 bytes.
///
/// Superset of C++ `CachedBlockInfo` (`blockHash`, `timestamp`, `cumulativeDifficulty`,
/// `alreadyGeneratedCoins`, `alreadyGeneratedTransactions`, `blockSize`), plus the
/// block's own difficulty, running chain size and tx count. Stored as [`BlockInfoRow`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BlockInfo {
    pub hash: [u8; 32],
    pub timestamp: u64,
    /// Difficulty this block was mined at.
    pub difficulty: u64,
    /// Low 64 bits of the cumulative difficulty (C++ keeps only a `uint64_t`).
    pub cumulative_difficulty_lo: u64,
    /// High 64 bits of the cumulative difficulty (must be 0: storage keeps 64 bits, like C++).
    pub cumulative_difficulty_hi: u64,
    /// `alreadyGeneratedCoins` after this block.
    pub already_generated_coins: u64,
    /// `alreadyGeneratedTransactions` after this block (including miner txs).
    pub cumulative_tx_count: u64,
    /// CryptoNote "cumulative block size" of this block (miner tx + txs), the value in the
    /// median window and the penalty formula (C++ `blockSize`).
    pub block_size: u64,
    /// Sum of `block_size` from genesis through this block.
    pub cumulative_size: u64,
    /// Transactions in the block excluding the miner tx.
    pub tx_count: u32,
    /// [`BLOCK_FLAG_PRUNED`], ...
    pub flags: u32,
}

impl BlockInfo {
    #[must_use]
    pub const fn cumulative_difficulty(&self) -> u128 {
        ((self.cumulative_difficulty_hi as u128) << 64) | self.cumulative_difficulty_lo as u128
    }

    pub const fn set_cumulative_difficulty(&mut self, cd: u128) {
        self.cumulative_difficulty_lo = cd as u64;
        self.cumulative_difficulty_hi = (cd >> 64) as u64;
    }

    #[must_use]
    pub const fn is_pruned(&self) -> bool {
        self.flags & BLOCK_FLAG_PRUNED != 0
    }
}

/// One row of [`BLOCK_INFOS`]. 88 bytes, no padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct BlockInfoRow {
    pub hash: [u8; 32],
    pub timestamp: u64,
    pub difficulty: u64,
    pub cumulative_difficulty: u64,
    pub already_generated_coins: u64,
    pub cumulative_tx_count: u64,
    pub cumulative_size: u64,
    pub block_size: u32,
    pub tx_count: u32,
}

impl BlockInfoRow {
    /// The on-disk form of `info`; `None` if a field does not fit (cumulative difficulty above
    /// `u64`, a block above 4 GiB).
    #[must_use]
    pub fn from_info(info: &BlockInfo) -> Option<Self> {
        if info.cumulative_difficulty_hi != 0 {
            return None;
        }
        Some(Self {
            hash: info.hash,
            timestamp: info.timestamp,
            difficulty: info.difficulty,
            cumulative_difficulty: info.cumulative_difficulty_lo,
            already_generated_coins: info.already_generated_coins,
            cumulative_tx_count: info.cumulative_tx_count,
            cumulative_size: info.cumulative_size,
            block_size: u32::try_from(info.block_size).ok()?,
            tx_count: info.tx_count,
        })
    }

    /// Back to [`BlockInfo`]; `pruned` sets [`BLOCK_FLAG_PRUNED`].
    #[must_use]
    pub const fn to_info(&self, pruned: bool) -> BlockInfo {
        BlockInfo {
            hash: self.hash,
            timestamp: self.timestamp,
            difficulty: self.difficulty,
            cumulative_difficulty_lo: self.cumulative_difficulty,
            cumulative_difficulty_hi: 0,
            already_generated_coins: self.already_generated_coins,
            cumulative_tx_count: self.cumulative_tx_count,
            block_size: self.block_size as u64,
            cumulative_size: self.cumulative_size,
            tx_count: self.tx_count,
            flags: if pruned { BLOCK_FLAG_PRUNED } else { 0 },
        }
    }
}

/// One row of [`OUTPUTS`]. 40 bytes, no padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct OutputRow {
    /// One-time output public key.
    pub key: [u8; 32],
    /// Height of the block containing the creating transaction.
    pub height: u32,
    /// The creating transaction's `unlock_time`, or [`UNLOCK_ESCAPE`] when it does not fit
    /// (the exact value is then in [`UNLOCK_OVERFLOW`]).
    pub unlock: u32,
}

/// One row of [`TX_HASH_TO_LOC`]. 8 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct TxLocRow {
    pub height: u32,
    /// 0 = miner tx (inside the block blob), `1..=n` = position in `Block::tx_hashes` + 1.
    pub tx_index: u32,
}

/// An output as returned by [`crate::Reader::outputs`]: what ring checks and decoy selection
/// need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputInfo {
    /// One-time output public key.
    pub key: [u8; 32],
    /// The creating transaction's `unlock_time` (height if < `CRYPTONOTE_MAX_BLOCK_NUMBER`,
    /// else a timestamp) — consensus checks it for every ring member.
    pub unlock_time: u64,
    /// Height of the block containing the creating transaction.
    pub height: u64,
}

/// Where a transaction lives (`Reader::tx_location`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxLoc {
    pub height: u64,
    /// 0 = miner tx (inside the block blob), `1..=n` = position in `Block::tx_hashes` + 1.
    pub tx_index: u32,
}

/// Fixed header in front of the raw block in an [`ALT_BLOCKS`] value. 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct AltBlockHeader {
    pub height: u64,
    pub cumulative_difficulty_lo: u64,
    pub cumulative_difficulty_hi: u64,
    pub block_size: u64,
}

/// Implements `redb::Value` for a flat `bytemuck::Pod` struct, stored as its raw bytes.
macro_rules! pod_value {
    ($t:ty, $name:literal) => {
        impl redb::Value for $t {
            type SelfType<'a>
                = $t
            where
                Self: 'a;
            type AsBytes<'a>
                = &'a [u8]
            where
                Self: 'a;

            fn fixed_width() -> Option<usize> {
                Some(core::mem::size_of::<$t>())
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
            where
                Self: 'a,
            {
                bytemuck::pod_read_unaligned(data)
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'b,
            {
                bytemuck::bytes_of(value)
            }

            fn type_name() -> redb::TypeName {
                redb::TypeName::new($name)
            }
        }
    };
}

// `BlockInfo` keeps the v1 type name: `stats` reads v1 databases' `block_infos` with it.
pod_value!(BlockInfo, "xkr_storage::BlockInfo");
pod_value!(BlockInfoRow, "xkr_storage::BlockInfoRow");
pod_value!(OutputRow, "xkr_storage::OutputRow");
pod_value!(TxLocRow, "xkr_storage::TxLocRow");

const _: () = assert!(core::mem::size_of::<BlockInfo>() == 104);
const _: () = assert!(core::mem::size_of::<BlockInfoRow>() == 88);
const _: () = assert!(core::mem::size_of::<OutputRow>() == 40);
const _: () = assert!(core::mem::size_of::<TxLocRow>() == 8);
const _: () = assert!(core::mem::size_of::<AltBlockHeader>() == 32);
const _: () = assert!(4 + 4 + 8 + PART_LEN <= 4096);

/// Height as stored in keys and rows.
pub(crate) fn h32(height: u64) -> crate::Result<u32> {
    u32::try_from(height).map_err(|_| crate::Error::Corrupt("height exceeds u32"))
}

/// A height bound for ranges over `u32` keys (heights beyond `u32` do not exist).
pub(crate) fn clamp32(height: u64) -> u32 {
    u32::try_from(height).unwrap_or(u32::MAX)
}

/// `BODY_PARTS`/`SIG_PARTS` key of `part` of `chunk`.
pub(crate) const fn part_key(chunk: u64, part: u64) -> u64 {
    (chunk << 16) | part
}
