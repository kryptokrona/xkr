//! A share: the C++ `PoolBlock` (`src/pool_block.h`).
//!
//! A pool block is a complete Kryptokrona block (the main-chain part) followed by the sidechain
//! part, without a length prefix between them. The main-chain part carries the PPLNS payout as
//! its coinbase and commits to the sidechain through the coinbase's extra nonce (the last 32
//! bytes are the sidechain id); the sidechain part names the miner wallet, the parent share,
//! uncles, height and difficulties. [`crate::parse`] reads one, [`crate::serialize`] writes one.

use xkr_types::{Hash, PublicKey, SecretKey};

use crate::difficulty::Difficulty;
use crate::wallet::MinerWallet;

/// `MAX_BLOCK_SIZE`: 128 KiB minus the 5-byte `BLOCK_RESPONSE` header.
pub const MAX_BLOCK_SIZE: usize = 128 * 1024 - 5;
/// `MAX_OUTPUT_VALUE`: amounts live in a 56-bit field.
pub const MAX_OUTPUT_VALUE: u64 = (1 << 56) - 1;
/// `MAX_CUMULATIVE_DIFFICULTY` (`{13019633956666736640, 1710}`).
pub const MAX_CUMULATIVE_DIFFICULTY: Difficulty = Difficulty::new(13_019_633_956_666_736_640, 1710);
/// `MAX_SIDECHAIN_HEIGHT`.
pub const MAX_SIDECHAIN_HEIGHT: u64 = 31_556_952_000;
/// `CRYPTONOTE_MAX_BLOCK_NUMBER`.
pub const CRYPTONOTE_MAX_BLOCK_NUMBER: u64 = 500_000_000;
/// `MERGE_MINING_MAX_CHAINS`.
pub const MERGE_MINING_MAX_CHAINS: u64 = 256;
/// `LOG2_MERGE_MINING_MAX_CHAINS`: the longest merkle proof.
pub const LOG2_MERGE_MINING_MAX_CHAINS: u64 = 8;
/// `MAX_UNCLES_PER_BLOCK`.
pub const MAX_UNCLES_PER_BLOCK: u64 = 5;
/// `HASHING_BLOB_MIN_SIZE`.
pub const HASHING_BLOB_MIN_SIZE: usize = 76;
/// `HASHING_BLOB_MAX_SIZE`.
pub const HASHING_BLOB_MAX_SIZE: usize = 128;
/// `NONCE_SIZE`.
pub const NONCE_SIZE: usize = 4;
/// `EXTRA_NONCE_SIZE`.
pub const EXTRA_NONCE_SIZE: usize = 4;
/// Size of the coinbase extra nonce field: the extra nonce and the sidechain merkle root.
pub const COINBASE_EXTRA_NONCE_FIELD: u64 = (EXTRA_NONCE_SIZE + 32) as u64;
/// Size of the coinbase extra: pubkey tag + key, nonce tag + size + field.
pub const COINBASE_EXTRA_SIZE: u64 = 1 + 32 + 1 + 1 + COINBASE_EXTRA_NONCE_FIELD;
/// `TXIN_GEN`.
pub const TXIN_GEN: u8 = 0xff;
/// `TXOUT_TO_KEY` (`coin::TXOUT_TO_KEY`).
pub const TXOUT_TO_KEY: u8 = 2;
/// `TXOUT_TO_TAGGED_KEY`.
pub const TXOUT_TO_TAGGED_KEY: u8 = 3;
/// `TX_EXTRA_TAG_PUBKEY`.
pub const TX_EXTRA_TAG_PUBKEY: u8 = 1;
/// `TX_EXTRA_NONCE`.
pub const TX_EXTRA_NONCE: u8 = 2;
/// `TX_EXTRA_MERGE_MINING_TAG`.
pub const TX_EXTRA_MERGE_MINING_TAG: u8 = 3;
/// `coin::PARENT_MM_TAG_DEPTH`.
pub const PARENT_MM_TAG_DEPTH: u64 = 0;

/// One coinbase output (`PoolBlock::TxOutput`: 56-bit amount, view tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxOutput {
    /// Amount in atomic units (at most [`MAX_OUTPUT_VALUE`]).
    pub reward: u64,
    /// View tag (version-2 coinbases only; 0 otherwise).
    pub view_tag: u8,
}

/// Local bookkeeping that is not part of the share (`reset_offchain_data`).
#[allow(clippy::struct_excessive_bools)] // mirrors the C++ flags one to one
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OffchainState {
    /// Shares built on top (children count 1, includers of an uncle count their distance).
    pub depth: u64,
    /// Verification finished.
    pub verified: bool,
    /// Verification failed (permanent).
    pub invalid: bool,
    /// Sent to peers.
    pub broadcasted: bool,
    /// Should be sent to peers once verified.
    pub want_broadcast: bool,
    /// Local time it was created or received (seconds).
    pub local_timestamp: u64,
    /// When it arrived from a peer (seconds, 0 for own shares).
    pub received_timestamp: u64,
    /// Proof-of-work hash, once computed.
    pub pow_hash: Option<Hash>,
    /// Difficulty of a share built on this one, once computed.
    pub cached_next_difficulty: Option<Difficulty>,
}

/// A share (`PoolBlock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolBlock {
    // ---- main-chain part
    /// Block major version.
    pub major_version: u8,
    /// Block minor version.
    pub minor_version: u8,
    /// The main-chain block this share builds on.
    pub prev_id: Hash,
    /// Block timestamp (in the parent block header).
    pub timestamp: u64,
    /// Nonce (in the parent block header).
    pub nonce: u32,
    /// Coinbase unlock time as received (written back as `txin_gen_height + unlock window`).
    pub unlock_time: u64,
    /// Coinbase input height = main-chain height of the block.
    pub txin_gen_height: u64,
    /// Coinbase outputs (amount, view tag).
    pub outputs: Vec<TxOutput>,
    /// One-time keys of the coinbase outputs.
    pub eph_public_keys: Vec<PublicKey>,
    /// Coinbase transaction public key `R`.
    pub txkey_pub: PublicKey,
    /// The 4-byte extra nonce of the coinbase.
    pub extra_nonce: u32,
    /// The coinbase's sidechain merkle root (equal to [`Self::sidechain_id`]).
    pub merkle_root: Hash,
    /// Transaction hashes; index 0 is the coinbase slot (zero until the coinbase hash is
    /// computed, `m_transactions`).
    pub transactions: Vec<Hash>,

    // ---- sidechain part
    /// Payout wallet of the share's miner.
    pub miner_wallet: MinerWallet,
    /// Seed of the coinbase key pair.
    pub txkey_sec_seed: Hash,
    /// Parent share id (zero for the genesis share).
    pub parent: Hash,
    /// Uncle share ids, ascending in hash order.
    pub uncles: Vec<Hash>,
    /// Sidechain height.
    pub sidechain_height: u64,
    /// Share difficulty.
    pub difficulty: Difficulty,
    /// Cumulative difficulty of the sidechain up to this share.
    pub cumulative_difficulty: Difficulty,
    /// Merge-mining merkle proof (always empty on the Kryptokrona sidechain).
    pub merkle_proof: Vec<Hash>,
    /// Merge-mining extra data `(chain id, data)`, ascending in hash order of the ids.
    pub merge_mining_extra: Vec<(Hash, Vec<u8>)>,
    /// `sidechainExtraBuf`: software id, software version, random, extra nonce.
    pub sidechain_extra: [u32; 4],

    // ---- derived
    /// Coinbase transaction secret key (from [`crate::keys::get_tx_keys`]).
    pub txkey_sec: SecretKey,
    /// The share id (`m_sidechainId`).
    pub sidechain_id: Hash,
    /// Local state.
    pub offchain: OffchainState,
}

/// The "already received" key: id, nonce and extra nonce (`PoolBlock::full_id`).
pub type FullId = [u8; 32 + NONCE_SIZE + EXTRA_NONCE_SIZE];

impl PoolBlock {
    /// `get_full_id()`: `id || nonce || extra_nonce` (little-endian integers).
    #[must_use]
    pub fn full_id(&self) -> FullId {
        let mut key = [0u8; 32 + NONCE_SIZE + EXTRA_NONCE_SIZE];
        key[..32].copy_from_slice(&self.sidechain_id.0);
        key[32..36].copy_from_slice(&self.nonce.to_le_bytes());
        key[36..].copy_from_slice(&self.extra_nonce.to_le_bytes());
        key
    }

    /// Sum of the coinbase output amounts.
    #[must_use]
    pub fn total_reward(&self) -> u64 {
        self.outputs.iter().map(|o| o.reward).sum()
    }

    /// The software id the share's miner announced (`sidechainExtraBuf[0]`).
    #[must_use]
    pub const fn software_id(&self) -> u32 {
        self.sidechain_extra[0]
    }

    /// The software version the share's miner announced (`sidechainExtraBuf[1]`).
    #[must_use]
    pub const fn software_version(&self) -> u32 {
        self.sidechain_extra[1]
    }
}
