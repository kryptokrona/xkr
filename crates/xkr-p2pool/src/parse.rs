//! Reading a share: `PoolBlock::deserialize` (`src/pool_block_parser.inl`, with the fork 14
//! coinbase of the Kryptokrona patches), field by field in the C++ order, accepting exactly the
//! blobs the C++ p2pool accepts.
//!
//! The share id is computed over the received bytes with five regions zeroed (nonce, aux hash,
//! coinbase extra nonce, coinbase merkle root, `sidechainExtraBuf[3]`) and the consensus id
//! appended. With a single merge-mined chain the merkle proof must be empty and the coinbase
//! merkle root must equal that id.
//!
//! Compact blobs (`BLOCK_BROADCAST_COMPACT`: transactions given as indexes into the parent's
//! list) are read when the parent's transactions are known; pruned blobs (outputs left out) are
//! not accepted.

use xkr_types::{Hash, PublicKey};

use crate::block::{
    COINBASE_EXTRA_NONCE_FIELD, CRYPTONOTE_MAX_BLOCK_NUMBER, LOG2_MERGE_MINING_MAX_CHAINS,
    MAX_BLOCK_SIZE, MAX_CUMULATIVE_DIFFICULTY, MAX_OUTPUT_VALUE, MAX_SIDECHAIN_HEIGHT,
    MAX_UNCLES_PER_BLOCK, MERGE_MINING_MAX_CHAINS, OffchainState, PARENT_MM_TAG_DEPTH, PoolBlock,
    TX_EXTRA_MERGE_MINING_TAG, TX_EXTRA_NONCE, TX_EXTRA_TAG_PUBKEY, TXIN_GEN, TXOUT_TO_KEY,
    TXOUT_TO_TAGGED_KEY, TxOutput,
};
use crate::difficulty::Difficulty;
use crate::forks::{COINBASE_TX_VERSION_2, ForkHeights, coinbase_tx_version};
use crate::keys::{check_keys, get_tx_keys};
use crate::order::hash_less;
use crate::serialize::{IdRegions, id_hash};
use crate::varint::read_varint;
use crate::wallet::MinerWallet;

/// `HARDFORK_SUPPORTED_VERSION`: the highest block major version the parser takes.
pub const MAX_MAJOR_VERSION: u8 = 16;
/// Highest block minor version the parser takes.
pub const MAX_MINOR_VERSION: u8 = 127;

/// Why a blob is not a share. Each variant is one check of `PoolBlock::deserialize`, in the
/// order they are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Longer than `MAX_BLOCK_SIZE`.
    #[error("blob larger than the block size limit")]
    TooLarge,
    /// The blob ended early.
    #[error("unexpected end of the blob at {0}")]
    Truncated(usize),
    /// A varint is malformed (non-canonical, too long or too large).
    #[error("malformed varint at {0}")]
    Varint(usize),
    /// A byte has another value than the format requires.
    #[error("unexpected byte at {0}")]
    UnexpectedByte(usize),
    /// Major version above 16.
    #[error("major version {0} is not supported")]
    MajorVersion(u8),
    /// Minor version above 127.
    #[error("minor version {0} is out of range")]
    MinorVersion(u8),
    /// Merge-mining tag depth other than 0.
    #[error("merge-mining tag depth is not 0")]
    MergeMiningDepth,
    /// Unlock height above `CRYPTONOTE_MAX_BLOCK_NUMBER`.
    #[error("coinbase unlock height out of range")]
    UnlockHeight,
    /// Coinbase height above `CRYPTONOTE_MAX_BLOCK_NUMBER`.
    #[error("coinbase height out of range")]
    TxinGenHeight,
    /// The major version is not the one of the coinbase height.
    #[error("major version {major} does not match height {height} (expected {expected})")]
    MajorVersionForHeight {
        /// The share's major version.
        major: u8,
        /// The coinbase height.
        height: u64,
        /// The major version of that height.
        expected: u8,
    },
    /// More outputs than the remaining bytes can hold.
    #[error("too many outputs")]
    TooManyOutputs,
    /// An output amount above `MAX_OUTPUT_VALUE`.
    #[error("output amount too large")]
    OutputValue,
    /// The output amounts overflow.
    #[error("output amounts overflow")]
    RewardOverflow,
    /// No outputs (a pruned blob).
    #[error("pruned blobs are not accepted")]
    Pruned,
    /// The outputs pay nothing.
    #[error("zero coinbase reward")]
    ZeroReward,
    /// The coinbase extra nonce field is not 36 bytes.
    #[error("coinbase extra nonce field has the wrong size")]
    ExtraNonceSize,
    /// The coinbase extra size does not match its content.
    #[error("coinbase extra size mismatch")]
    ExtraSize,
    /// More transactions than fit in a block.
    #[error("too many transactions")]
    TooManyTransactions,
    /// Larger than `MAX_BLOCK_SIZE` once compact transactions are expanded.
    #[error("expanded blob larger than the block size limit")]
    ExpandedTooLarge,
    /// A miner key is not a curve point.
    #[error("invalid miner wallet")]
    MinerWallet,
    /// A compact blob whose parent is unknown.
    #[error("parent of a compact blob is unknown")]
    CompactParentUnknown,
    /// A compact transaction index outside the parent's list.
    #[error("compact transaction index out of range")]
    CompactIndex,
    /// More than `MAX_UNCLES_PER_BLOCK` uncles.
    #[error("too many uncles")]
    TooManyUncles,
    /// Sidechain height above `MAX_SIDECHAIN_HEIGHT`.
    #[error("sidechain height out of range")]
    SidechainHeight,
    /// The difficulty differs from the one the parent requires.
    #[error("difficulty differs from the parent's next difficulty")]
    Difficulty,
    /// Cumulative difficulty above `MAX_CUMULATIVE_DIFFICULTY`.
    #[error("cumulative difficulty out of range")]
    CumulativeDifficultyTooLarge,
    /// Difficulty above the cumulative difficulty.
    #[error("difficulty above the cumulative difficulty")]
    CumulativeDifficultyTooSmall,
    /// The coinbase key is not the one the tx key seed gives.
    #[error("coinbase transaction key does not match the seed")]
    TxKey,
    /// The derived key pair does not check out.
    #[error("coinbase transaction key pair invalid")]
    TxKeyPair,
    /// Merkle proof longer than 8.
    #[error("merkle proof too long")]
    MerkleProofSize,
    /// More than 256 merge-mining extra entries.
    #[error("too many merge-mining extra entries")]
    MergeMiningExtraCount,
    /// Merge-mining extra ids not strictly ascending.
    #[error("merge-mining extra ids out of order")]
    MergeMiningExtraOrder,
    /// Bytes after the sidechain part.
    #[error("trailing bytes after the share")]
    TrailingBytes,
    /// The merkle proof does not lead from the share id to the coinbase merkle root.
    #[error("merkle root does not match the share id")]
    MerkleRoot,
}

/// What the parser needs to know about the sidechain (`const SideChain&`).
pub trait SidechainView {
    /// The consensus id appended to every share before hashing.
    fn consensus_id(&self) -> &Hash;
    /// The fork heights that decide the major version of a coinbase height.
    fn fork_heights(&self) -> ForkHeights;
    /// `get_cached_next_difficulty(parent)`: the difficulty a child of `parent` must have, if
    /// already known.
    fn cached_next_difficulty(&self, _parent: &Hash) -> Option<Difficulty> {
        None
    }
    /// The transaction list (`m_transactions`, coinbase slot first) of a known share, for
    /// compact blobs.
    fn transactions_of(&self, _id: &Hash) -> Option<Vec<Hash>> {
        None
    }
}

/// A sidechain view with only the consensus id and fork heights (no shares known).
#[derive(Debug, Clone, Copy)]
pub struct StaticView {
    /// Consensus id.
    pub consensus_id: Hash,
    /// Fork heights.
    pub forks: ForkHeights,
}

impl SidechainView for StaticView {
    fn consensus_id(&self) -> &Hash {
        &self.consensus_id
    }
    fn fork_heights(&self) -> ForkHeights {
        self.forks
    }
}

/// A cursor over the blob with the C++ read primitives.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8, ParseError> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or(ParseError::Truncated(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn expect(&mut self, value: u8) -> Result<(), ParseError> {
        let at = self.pos;
        if self.byte()? == value {
            Ok(())
        } else {
            Err(ParseError::UnexpectedByte(at))
        }
    }

    fn varint(&mut self) -> Result<u64, ParseError> {
        let at = self.pos;
        read_varint(self.data, &mut self.pos, 64).ok_or(ParseError::Varint(at))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.data.len())
            .ok_or(ParseError::Truncated(self.pos))?;
        let data = self.data;
        let s = &data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn hash(&mut self) -> Result<Hash, ParseError> {
        let mut h = [0u8; 32];
        h.copy_from_slice(self.bytes(32)?);
        Ok(Hash(h))
    }

    fn u32_le(&mut self) -> Result<u32, ParseError> {
        let mut w = [0u8; 4];
        w.copy_from_slice(self.bytes(4)?);
        Ok(u32::from_le_bytes(w))
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}

/// Reads a full share (`deserialize(data, size, sidechain, compact = false, allow_pruned =
/// false)`).
pub fn parse_pool_block(data: &[u8], view: &dyn SidechainView) -> Result<PoolBlock, ParseError> {
    parse(data, view, false)
}

/// Reads a compact share (`deserialize(…, compact = true, allow_pruned = false)`).
pub fn parse_compact_pool_block(
    data: &[u8],
    view: &dyn SidechainView,
) -> Result<PoolBlock, ParseError> {
    parse(data, view, true)
}

fn parse(data: &[u8], view: &dyn SidechainView, compact: bool) -> Result<PoolBlock, ParseError> {
    if data.len() > MAX_BLOCK_SIZE {
        return Err(ParseError::TooLarge);
    }
    let mut c = Cursor { data, pos: 0 };

    // ---- outer block header: major, minor, prev id
    let major_version = c.byte()?;
    if major_version > MAX_MAJOR_VERSION {
        return Err(ParseError::MajorVersion(major_version));
    }
    let minor_version = c.byte()?;
    if minor_version > MAX_MINOR_VERSION {
        return Err(ParseError::MinorVersion(minor_version));
    }
    let prev_id = c.hash()?;

    // ---- parent block header
    c.expect(0)?;
    c.expect(0)?;
    let timestamp = c.varint()?;
    for _ in 0..32 {
        c.expect(0)?;
    }
    let nonce_offset = c.pos;
    let nonce = c.u32_le()?;
    c.expect(1)?;

    // ---- parent coinbase: version, unlock time, inputs, outputs all 0; extra = merge-mining tag
    for _ in 0..4 {
        c.expect(0)?;
    }
    let _parent_extra_size = c.varint()?;
    c.expect(TX_EXTRA_MERGE_MINING_TAG)?;
    let _mm_field_size = c.varint()?;
    if c.varint()? != PARENT_MM_TAG_DEPTH {
        return Err(ParseError::MergeMiningDepth);
    }
    let aux_hash_offset = c.pos;
    let _aux_hash = c.hash()?;

    // ---- real coinbase
    let coinbase_version = coinbase_tx_version(major_version);
    let tagged = coinbase_version >= COINBASE_TX_VERSION_2;
    c.expect(coinbase_version)?;
    let unlock_time = c.varint()?;
    if unlock_time > CRYPTONOTE_MAX_BLOCK_NUMBER {
        return Err(ParseError::UnlockHeight);
    }
    c.expect(1)?;
    c.expect(TXIN_GEN)?;
    let txin_gen_height = c.varint()?;
    if txin_gen_height > CRYPTONOTE_MAX_BLOCK_NUMBER {
        return Err(ParseError::TxinGenHeight);
    }
    let expected = view.fork_heights().network_major_version(txin_gen_height);
    if major_version != expected {
        return Err(ParseError::MajorVersionForHeight {
            major: major_version,
            height: txin_gen_height,
            expected,
        });
    }

    let num_outputs = c.varint()?;
    if num_outputs == 0 {
        return Err(ParseError::Pruned);
    }
    let min_output_size: u64 = if tagged { 35 } else { 34 };
    if num_outputs > u64::MAX / min_output_size
        || (c.remaining() as u64) < num_outputs * min_output_size
    {
        return Err(ParseError::TooManyOutputs);
    }
    let n = num_outputs as usize;
    let mut outputs = Vec::with_capacity(n);
    let mut eph_public_keys = Vec::with_capacity(n);
    let mut total_reward = 0u64;
    for _ in 0..n {
        let reward = c.varint()?;
        if reward > MAX_OUTPUT_VALUE {
            return Err(ParseError::OutputValue);
        }
        total_reward = total_reward
            .checked_add(reward)
            .ok_or(ParseError::RewardOverflow)?;
        c.expect(if tagged {
            TXOUT_TO_TAGGED_KEY
        } else {
            TXOUT_TO_KEY
        })?;
        eph_public_keys.push(PublicKey(c.hash()?.0));
        let view_tag = if tagged { c.byte()? } else { 0 };
        outputs.push(TxOutput { reward, view_tag });
    }
    if total_reward == 0 {
        return Err(ParseError::ZeroReward);
    }

    let tx_extra_size = c.varint()?;
    let tx_extra_begin = c.pos;
    c.expect(TX_EXTRA_TAG_PUBKEY)?;
    let txkey_pub = PublicKey(c.hash()?.0);
    c.expect(TX_EXTRA_NONCE)?;
    if c.varint()? != COINBASE_EXTRA_NONCE_FIELD {
        return Err(ParseError::ExtraNonceSize);
    }
    let extra_nonce_offset = c.pos;
    let extra_nonce = c.u32_le()?;
    let merkle_root_offset = c.pos;
    let merkle_root = c.hash()?;
    if (c.pos - tx_extra_begin) as u64 != tx_extra_size {
        return Err(ParseError::ExtraSize);
    }

    // ---- other transactions
    let num_transactions = c.varint()?;
    if num_transactions > (MAX_BLOCK_SIZE / 32) as u64 {
        return Err(ParseError::TooManyTransactions);
    }
    let transactions_offset = c.pos;
    let nt = num_transactions as usize;
    let mut tx_list = Vec::with_capacity(nt);
    if compact {
        if c.remaining() < nt {
            return Err(ParseError::Truncated(c.pos));
        }
        for _ in 0..nt {
            let parent_index = c.varint()?;
            let id = if parent_index == 0 {
                c.hash()?
            } else {
                Hash::ZERO
            };
            tx_list.push((parent_index, id));
        }
    } else {
        if c.remaining() < nt * 32 {
            return Err(ParseError::Truncated(c.pos));
        }
        for _ in 0..nt {
            tx_list.push((0, c.hash()?));
        }
    }
    let transactions_actual_size = c.pos - transactions_offset;
    let transactions_size = nt * 32;
    let expanded_len = data.len() - transactions_actual_size + transactions_size;
    if expanded_len > MAX_BLOCK_SIZE {
        return Err(ParseError::ExpandedTooLarge);
    }

    // ---- sidechain part
    let spend = PublicKey(c.hash()?.0);
    let view_key = PublicKey(c.hash()?.0);
    let miner_wallet = MinerWallet::assign(spend, view_key).ok_or(ParseError::MinerWallet)?;
    let txkey_sec_seed = c.hash()?;
    let parent = c.hash()?;

    let mut transactions = Vec::with_capacity(nt + 1);
    transactions.push(Hash::ZERO);
    if compact {
        let parent_txs = view
            .transactions_of(&parent)
            .ok_or(ParseError::CompactParentUnknown)?;
        for (parent_index, id) in &tx_list {
            if *parent_index == 0 {
                transactions.push(*id);
            } else {
                let i = usize::try_from(*parent_index).map_err(|_| ParseError::CompactIndex)?;
                transactions.push(*parent_txs.get(i).ok_or(ParseError::CompactIndex)?);
            }
        }
    } else {
        transactions.extend(tx_list.iter().map(|(_, id)| *id));
    }

    let num_uncles = c.varint()?;
    if num_uncles > MAX_UNCLES_PER_BLOCK {
        return Err(ParseError::TooManyUncles);
    }
    if (c.remaining() as u64) < num_uncles * 32 {
        return Err(ParseError::Truncated(c.pos));
    }
    let mut uncles = Vec::with_capacity(num_uncles as usize);
    for _ in 0..num_uncles {
        uncles.push(c.hash()?);
    }

    let sidechain_height = c.varint()?;
    if sidechain_height > MAX_SIDECHAIN_HEIGHT {
        return Err(ParseError::SidechainHeight);
    }
    let diff_lo = c.varint()?;
    let diff_hi = c.varint()?;
    let difficulty = Difficulty::new(diff_lo, diff_hi);
    if view
        .cached_next_difficulty(&parent)
        .is_some_and(|d| !d.is_zero() && d != difficulty)
    {
        return Err(ParseError::Difficulty);
    }
    let cum_lo = c.varint()?;
    let cum_hi = c.varint()?;
    let cumulative_difficulty = Difficulty::new(cum_lo, cum_hi);
    if cumulative_difficulty > MAX_CUMULATIVE_DIFFICULTY {
        return Err(ParseError::CumulativeDifficultyTooLarge);
    }
    if difficulty > cumulative_difficulty {
        return Err(ParseError::CumulativeDifficultyTooSmall);
    }

    let (derived_pub, txkey_sec) = get_tx_keys(&txkey_sec_seed, &prev_id);
    if derived_pub != txkey_pub {
        return Err(ParseError::TxKey);
    }
    if !check_keys(&txkey_pub, &txkey_sec) {
        return Err(ParseError::TxKeyPair);
    }

    let merkle_proof_size = c.byte()?;
    if u64::from(merkle_proof_size) > LOG2_MERGE_MINING_MAX_CHAINS {
        return Err(ParseError::MerkleProofSize);
    }
    let mut merkle_proof = Vec::with_capacity(usize::from(merkle_proof_size));
    for _ in 0..merkle_proof_size {
        merkle_proof.push(c.hash()?);
    }

    let mm_count = c.varint()?;
    let mut merge_mining_extra: Vec<(Hash, Vec<u8>)> = Vec::new();
    if mm_count > 0 {
        if mm_count > MERGE_MINING_MAX_CHAINS {
            return Err(ParseError::MergeMiningExtraCount);
        }
        if (c.remaining() as u64) < mm_count * 33 {
            return Err(ParseError::Truncated(c.pos));
        }
        for _ in 0..mm_count {
            let chain_id = c.hash()?;
            if merge_mining_extra
                .last()
                .is_some_and(|(prev, _)| !hash_less(prev, &chain_id))
            {
                return Err(ParseError::MergeMiningExtraOrder);
            }
            let len = c.varint()?;
            if (c.remaining() as u64) < len {
                return Err(ParseError::Truncated(c.pos));
            }
            let bytes = c.bytes(len as usize)?.to_vec();
            merge_mining_extra.push((chain_id, bytes));
        }
    }

    let sidechain_extra = [c.u32_le()?, c.u32_le()?, c.u32_le()?, c.u32_le()?];
    let sidechain_extra_nonce_offset = c.pos - 4;
    if c.pos != data.len() {
        return Err(ParseError::TrailingBytes);
    }

    // ---- share id over the received bytes (compact transactions expanded first)
    let regions = IdRegions {
        nonce: nonce_offset,
        aux_hash: aux_hash_offset,
        extra_nonce: extra_nonce_offset,
        merkle_root: merkle_root_offset,
        sidechain_extra_nonce: sidechain_extra_nonce_offset - transactions_actual_size
            + transactions_size,
    };
    let sidechain_id = if compact {
        let mut full = Vec::with_capacity(expanded_len);
        full.extend_from_slice(&data[..transactions_offset]);
        for h in &transactions[1..] {
            full.extend_from_slice(&h.0);
        }
        full.extend_from_slice(&data[transactions_offset + transactions_actual_size..]);
        id_hash(&full, &regions, view.consensus_id())
    } else {
        id_hash(data, &regions, view.consensus_id())
    };

    // One merge-mined chain (slot 0 of 1): the root is the id itself, the proof must be empty.
    if !merkle_proof.is_empty() || sidechain_id.is_zero() || merkle_root != sidechain_id {
        return Err(ParseError::MerkleRoot);
    }

    Ok(PoolBlock {
        major_version,
        minor_version,
        prev_id,
        timestamp,
        nonce,
        unlock_time,
        txin_gen_height,
        outputs,
        eph_public_keys,
        txkey_pub,
        extra_nonce,
        merkle_root,
        transactions,
        miner_wallet,
        txkey_sec_seed,
        parent,
        uncles,
        sidechain_height,
        difficulty,
        cumulative_difficulty,
        merkle_proof,
        merge_mining_extra,
        sidechain_extra,
        txkey_sec,
        sidechain_id,
        offchain: OffchainState::default(),
    })
}
