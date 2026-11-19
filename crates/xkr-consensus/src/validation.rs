//! Block and transaction validation as pure functions over caller-supplied chain data.
//!
//! Mirrors `Core::addBlock`, `Core::validateBlock`, `Core::validateTransaction` and
//! `Core::validateSemantic` (`src/cryptonote_core/core.cpp`) and
//! `Currency::checkProofOfWork{,V1,V2}` (`currency.cpp`). Storage lookups (ring members, spent
//! key images, chain history) are inputs: see [`ChainContext`] and [`ResolvedInput`].
//!
//! Error enums mirror `block_validation_errors.h` and `transaction_validation_errors.h`.

use std::collections::HashSet;
use std::hash::BuildHasher;

use xkr_crypto::tree_hash::tree_hash_from_branch;
use xkr_crypto::{
    CheckedKeyImage, check_hash, check_key, check_ring_signature_checked, clsag_verify_checked,
};
use xkr_serialization::{
    DecodeError, MAX_PARENT_BLOCK_SIZE, ParentBlockMode, aux_block_header_hash, block_long_hash,
    encode_parent_block, encode_transaction,
};
use xkr_types::{Block, Hash, KeyImage, PublicKey, Transaction, TxInput, merge_mining_tag};

use crate::difficulty::difficulty_for_next_block;
use crate::forks::{ForkSchedule, RingSizeRule};
use crate::params::{
    BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT, BLOCK_MAJOR_VERSION_7, BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW,
    BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3, CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT,
    CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3, CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4,
    CRYPTONOTE_MAX_TX_SIZE, CRYPTONOTE_REWARD_BLOCKS_WINDOW, LWMA_2_DIFFICULTY_BLOCK_INDEX,
    LWMA_2_DIFFICULTY_BLOCK_INDEX_V2, LWMA_2_DIFFICULTY_BLOCK_INDEX_V3, MAX_EXTRA_SIZE_BLOCK,
    MAX_EXTRA_SIZE_POOL, NetworkParams, TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT,
};
use crate::reward::{
    get_block_reward, is_fusion_transaction, max_block_cumulative_size,
    max_transaction_allowed_size, median, minimum_fee, mixin_limits, validate_mixins,
};
use crate::version::block_major_version_for_height;

/// C++ `error::BlockValidationError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockError {
    /// `WRONG_VERSION`.
    #[error("wrong block major version {got}, expected {expected}")]
    WrongVersion {
        /// Version for the block's height.
        expected: u8,
        /// Version in the block.
        got: u8,
    },
    /// `PARENT_BLOCK_SIZE_TOO_BIG` (wire parent block > 2048 bytes).
    #[error("parent block of {0} bytes exceeds 2048")]
    ParentBlockSizeTooBig(usize),
    /// `PARENT_BLOCK_WRONG_VERSION` (v2 block with parent major > 1).
    #[error("parent block major version {0} not allowed for a v2 block")]
    ParentBlockWrongVersion(u8),
    /// `TIMESTAMP_TOO_FAR_IN_FUTURE`.
    #[error("timestamp {timestamp} later than {limit}")]
    TimestampTooFarInFuture {
        /// Block timestamp.
        timestamp: u64,
        /// Adjusted time + future limit.
        limit: u64,
    },
    /// `TIMESTAMP_TOO_FAR_IN_PAST`.
    #[error("timestamp {timestamp} below the median {median} of recent blocks")]
    TimestampTooFarInPast {
        /// Block timestamp.
        timestamp: u64,
        /// Median of the check window.
        median: u64,
    },
    /// `CUMULATIVE_BLOCK_SIZE_TOO_BIG`.
    #[error("cumulative block size {size} too big (limit {limit})")]
    CumulativeBlockSizeTooBig {
        /// Coinbase + transactions size.
        size: u64,
        /// Limit that was exceeded.
        limit: u64,
    },
    /// `DIFFICULTY_OVERHEAD` (next difficulty computed as 0).
    #[error("difficulty overhead")]
    DifficultyOverhead,
    /// `BLOCK_REWARD_MISMATCH`.
    #[error("miner reward {got} != expected {expected}")]
    BlockRewardMismatch {
        /// Reward from `getBlockReward`.
        expected: u64,
        /// Sum of coinbase outputs.
        got: u64,
    },
    /// `CHECKPOINT_BLOCK_HASH_MISMATCH`.
    #[error("block hash does not match the checkpoint")]
    CheckpointBlockHashMismatch,
    /// `PROOF_OF_WORK_TOO_WEAK` (also merge-mining proof failures).
    #[error("proof of work too weak")]
    ProofOfWorkTooWeak,
    /// `TRANSACTION_ABSENT_IN_POOL`.
    #[error("transaction absent in pool")]
    TransactionAbsentInPool,
    /// `TRANSACTION_DUPLICATES`.
    #[error("duplicate transactions in block")]
    TransactionDuplicates,
    /// `TRANSACTION_INCONSISTENCY`.
    #[error("block transactions do not match its transaction hashes")]
    TransactionInconsistency,
}

/// C++ `error::TransactionValidationError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[allow(missing_docs)]
pub enum TxError {
    #[error("empty inputs")]
    EmptyInputs,
    #[error("unknown input type")]
    InputUnknownType,
    #[error("input with empty output usage")]
    InputEmptyOutputUsage,
    #[error("key image not in the prime-order subgroup")]
    InputInvalidDomainKeyImages,
    #[error("identical key images in transaction")]
    InputIdenticalKeyImages,
    #[error("identical output indexes in input")]
    InputIdenticalOutputIndexes,
    #[error("key image already spent")]
    InputKeyImageAlreadySpent,
    #[error("invalid global output index")]
    InputInvalidGlobalIndex,
    #[error("spend of a locked output")]
    InputSpendLockedOut,
    #[error("invalid ring signature")]
    InputInvalidSignatures,
    #[error("wrong signatures count")]
    InputWrongSignaturesCount,
    #[error("inputs amount overflow")]
    InputsAmountOverflow,
    #[error("wrong input count")]
    InputWrongCount,
    #[error("unexpected input type")]
    InputUnexpectedType,
    #[error("coinbase input has the wrong block index")]
    BaseInputWrongBlockIndex,
    #[error("zero amount output")]
    OutputZeroAmount,
    #[error("output key is not a valid point")]
    OutputInvalidKey,
    #[error("invalid required signatures count")]
    OutputInvalidRequiredSignaturesCount,
    #[error("unknown output type")]
    OutputUnknownType,
    #[error("outputs amount overflow")]
    OutputsAmountOverflow,
    #[error("outputs exceed inputs")]
    WrongAmount,
    #[error("wrong transaction unlock time")]
    WrongTransactionUnlockTime,
    #[error("invalid mixin")]
    InvalidMixin,
    #[error("extra too large")]
    ExtraTooLarge,
    #[error("coinbase transaction carries signatures")]
    BaseInvalidSignaturesCount,
    #[error("ring size differs from signature count")]
    InputInvalidSignaturesCount,
    /// Fork 14 (`docs/tx-v2.md`): the transaction version is not allowed at this height
    /// (version 2 before the fork, version 1 after the grace period, coinbase not version 2
    /// after the fork).
    #[error("transaction version not allowed at this height")]
    WrongTransactionVersion,
}

/// Any reason `Core::addBlock` rejects a block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConsensusError {
    /// A `BlockValidationError`.
    #[error(transparent)]
    Block(#[from] BlockError),
    /// A `TransactionValidationError`; `tx_index` is `None` for the coinbase.
    #[error("transaction {tx_index:?}: {error}")]
    Transaction {
        /// Index into the block's transaction list (`None` = miner transaction).
        tx_index: Option<usize>,
        /// The error.
        error: TxError,
    },
    /// `AddBlockErrorCode::DESERIALIZATION_FAILED` (transaction larger than
    /// `CRYPTONOTE_MAX_TX_SIZE` or undecodable / unserializable).
    #[error("deserialization failed: {0}")]
    DeserializationFailed(String),
}

impl From<DecodeError> for ConsensusError {
    fn from(e: DecodeError) -> Self {
        Self::DeserializationFailed(e.to_string())
    }
}

fn tx_err(tx_index: Option<usize>) -> impl Fn(TxError) -> ConsensusError {
    move |error| ConsensusError::Transaction { tx_index, error }
}

/// C++ `Currency::blockFutureTimeLimit(height)`.
#[must_use]
pub const fn block_future_time_limit(height: u32) -> u64 {
    if height as u64 >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V2 {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V4
    } else if height as u64 >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT_V3
    } else {
        CRYPTONOTE_BLOCK_FUTURE_TIME_LIMIT
    }
}

/// C++ `Currency::timestampCheckWindow(height)`.
#[must_use]
pub const fn timestamp_check_window(height: u32) -> usize {
    if height as u64 >= LWMA_2_DIFFICULTY_BLOCK_INDEX_V3 {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3
    } else {
        BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW
    }
}

/// Chain state a block is validated against. All slices are oldest-first and end at the
/// previous block (`height - 1`).
#[derive(Debug, Clone, Copy)]
pub struct ChainContext<'a> {
    /// Network parameters.
    pub params: &'a NetworkParams,
    /// Height of the block being validated (`previousBlockIndex + 1`).
    pub height: u32,
    /// Hash of the previous block (only used by callers; kept for symmetry with C++).
    pub prev_hash: Hash,
    /// Timestamps of the last `timestamp_check_window(height)` blocks **including** genesis
    /// (`getLastTimestamps(window, prev, addGenesisBlock)`). Longer slices are fine: only the
    /// tail is used.
    pub recent_timestamps: &'a [u64],
    /// Timestamps for [`crate::difficulty::difficulty_window`] (genesis excluded).
    pub difficulty_timestamps: &'a [u64],
    /// Cumulative difficulties for the same window.
    pub difficulty_cumulative: &'a [u64],
    /// Cumulative sizes of the last `CRYPTONOTE_REWARD_BLOCKS_WINDOW` blocks including genesis
    /// (`getLastBlocksSizes(100, prev, addGenesisBlock)`). Longer slices are fine.
    pub recent_block_sizes: &'a [u64],
    /// `getAlreadyGeneratedCoins(prev)`.
    pub already_generated_coins: u64,
    /// Node wall clock (`Core::getAdjustedTime`, plain `time(NULL)` in C++).
    pub adjusted_time: u64,
    /// Genesis block hash (merge-mining aux path, `checkProofOfWorkV2`).
    pub genesis_hash: Hash,
    /// `Checkpoints::isInCheckpointZone(height)`: skips PoW, spent and ring-signature checks.
    pub in_checkpoint_zone: bool,
    /// Checkpoint hash for `height`, if any (`Checkpoints::checkBlock`).
    pub checkpoint: Option<Hash>,
}

/// Why a ring member lookup failed (`ExtractOutputKeysResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingLookupError {
    /// `INVALID_GLOBAL_INDEX`.
    InvalidGlobalIndex,
    /// `OUTPUT_LOCKED` (see [`crate::reward::is_spend_time_unlocked`]).
    OutputLocked,
}

/// Storage answers for one transaction input (ignored for coinbase inputs).
#[derive(Debug, Clone, Copy)]
pub struct ResolvedInput<'a> {
    /// `IBlockchainCache::checkIfSpent(keyImage, previousBlockIndex)`.
    pub key_image_spent: bool,
    /// Output keys for `absolute_output_indexes()` of this input's amount
    /// (`extractKeyOutputKeys`), or why they could not be produced.
    pub ring: Result<&'a [PublicKey], RingLookupError>,
}

/// One non-coinbase transaction of a block, decoded and resolved by the caller.
#[derive(Debug, Clone, Copy)]
pub struct BlockTransaction<'a> {
    /// Decoded transaction.
    pub tx: &'a Transaction,
    /// `cn_fast_hash` of the blob.
    pub hash: Hash,
    /// Hash of the prefix (ring signature message).
    pub prefix_hash: Hash,
    /// Length of the transaction blob.
    pub blob_size: usize,
    /// One entry per input.
    pub inputs: &'a [ResolvedInput<'a>],
}

/// Values computed while accepting a block (what `IBlockchainCache::pushBlock` stores).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedBlock {
    /// Difficulty the block had to meet.
    pub difficulty: u64,
    /// Coinbase + transactions size.
    pub cumulative_size: u64,
    /// Sum of transaction fees.
    pub fees: u64,
    /// Expected (and actual) miner reward.
    pub reward: u64,
    /// Coins created (`emissionChange`).
    pub emission_change: i64,
}

/// C++ `Core::validateSemantic(transaction, fee, blockIndex)`; returns the fee.
///
/// Transaction version 2 (`docs/tx-v2.md`) adds one format rule: its outputs are tagged keys,
/// and those of versions 0/1 are plain keys (`OUTPUT_UNKNOWN_TYPE` otherwise; the decoder
/// already guarantees it for decoded transactions, so version-1 behaviour is unchanged). Which
/// versions a height accepts is [`check_transaction_version`].
pub fn validate_semantic(tx: &Transaction) -> Result<u64, TxError> {
    validate_semantic_checked(tx).map(|(fee, _)| fee)
}

/// [`validate_semantic`], also returning the checked key images, one per input in input order
/// (for [`validate_transaction_checked`] and the ring signatures).
pub fn validate_semantic_checked(tx: &Transaction) -> Result<(u64, Vec<CheckedKeyImage>), TxError> {
    let p = &tx.prefix;
    if p.inputs.is_empty() {
        return Err(TxError::EmptyInputs);
    }
    let v2 = tx.is_v2();
    let mut outputs_sum: u64 = 0;
    for o in &p.outputs {
        if o.amount == 0 {
            return Err(TxError::OutputZeroAmount);
        }
        if o.view_tag().is_some() != v2 {
            return Err(TxError::OutputUnknownType);
        }
        if !check_key(o.key()) {
            return Err(TxError::OutputInvalidKey);
        }
        outputs_sum = outputs_sum
            .checked_add(o.amount)
            .ok_or(TxError::OutputsAmountOverflow)?;
    }
    let mut inputs_sum: u64 = 0;
    let mut images: HashSet<KeyImage> = HashSet::with_capacity(p.inputs.len());
    let mut checked = Vec::with_capacity(p.inputs.len());
    for input in &p.inputs {
        let TxInput::Key {
            amount,
            output_indexes,
            key_image,
        } = input
        else {
            return Err(TxError::InputUnknownType);
        };
        if !images.insert(*key_image) {
            return Err(TxError::InputIdenticalKeyImages);
        }
        if output_indexes.is_empty() {
            return Err(TxError::InputEmptyOutputUsage);
        }
        // scalarmultKey(keyImage, L) == I
        let Some(image) = CheckedKeyImage::new(key_image) else {
            return Err(TxError::InputInvalidDomainKeyImages);
        };
        checked.push(image);
        // packed indexes: the first may be zero, the deltas may not
        if output_indexes[1..].contains(&0) {
            return Err(TxError::InputIdenticalOutputIndexes);
        }
        inputs_sum = inputs_sum
            .checked_add(*amount)
            .ok_or(TxError::InputsAmountOverflow)?;
    }
    if outputs_sum > inputs_sum {
        return Err(TxError::WrongAmount);
    }
    Ok((inputs_sum - outputs_sum, checked))
}

/// The fork-14 version rules for a non-coinbase transaction in the block at `height`
/// (`docs/tx-v2.md`): the version must be allowed by the fork table
/// ([`ForkSchedule::allows_tx_version`]: version 1 before fork 14 and during the grace period,
/// version 2 after the fork), and the key inputs of a version-2 transaction must satisfy the
/// table's [`RingSizeRule`] (`INVALID_MIXIN` otherwise). Mainnet without fork 14 accepts versions
/// 0 and 1 only, which is what the C++ deserializer accepts.
pub fn check_transaction_version(
    forks: &ForkSchedule,
    height: u64,
    tx: &Transaction,
) -> Result<(), TxError> {
    if !forks.allows_tx_version(height, tx.prefix.version) {
        return Err(TxError::WrongTransactionVersion);
    }
    if tx.is_v2()
        && let RingSizeRule::Range { min, max } = forks.fork14_params().v2_ring_size
    {
        for input in &tx.prefix.inputs {
            if let TxInput::Key { output_indexes, .. } = input
                && !(usize::from(min)..=usize::from(max)).contains(&output_indexes.len())
            {
                return Err(TxError::InvalidMixin);
            }
        }
    }
    Ok(())
}

/// C++ `Core::validateTransaction(tx, state, cache, fee, blockIndex)` with
/// `blockIndex = previous block index`. `seen_key_images` is the block-wide
/// `TransactionValidatorState::spentKeyImages`. Returns the fee.
///
/// Version-2 transactions (`docs/tx-v2.md`) are checked the same way, except that every key
/// input must carry a CLSAG signature with one response per ring member (always, not only from
/// `TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT`) that verifies against the resolved ring
/// ([`xkr_crypto::clsag_verify`]). Key images are the same `x * H_p(P)` in both versions, so
/// the spent set (and `seen_key_images`) catches double spends across versions.
pub fn validate_transaction<S: BuildHasher>(
    tx: &BlockTransaction<'_>,
    previous_block_index: u32,
    in_checkpoint_zone: bool,
    seen_key_images: &mut HashSet<KeyImage, S>,
) -> Result<u64, TxError> {
    let (fee, key_images) = validate_semantic_checked(tx.tx)?;
    validate_transaction_checked(
        tx,
        &key_images,
        previous_block_index,
        in_checkpoint_zone,
        seen_key_images,
    )?;
    Ok(fee)
}

/// The part of [`validate_transaction`] after [`validate_semantic_checked`], with the key images
/// that call returned for `tx`: every input's key image must be the checked one at its index
/// (`InputInvalidDomainKeyImages` otherwise).
pub fn validate_transaction_checked<S: BuildHasher>(
    tx: &BlockTransaction<'_>,
    key_images: &[CheckedKeyImage],
    previous_block_index: u32,
    in_checkpoint_zone: bool,
    seen_key_images: &mut HashSet<KeyImage, S>,
) -> Result<(), TxError> {
    for (i, input) in tx.tx.prefix.inputs.iter().enumerate() {
        let TxInput::Key { key_image, .. } = input else {
            return Err(TxError::InputUnknownType);
        };
        let Some(image) = key_images.get(i).filter(|c| c.key_image() == key_image) else {
            return Err(TxError::InputInvalidDomainKeyImages);
        };
        if !seen_key_images.insert(*key_image) {
            return Err(TxError::InputKeyImageAlreadySpent);
        }
        if in_checkpoint_zone {
            continue;
        }
        let resolved = tx.inputs.get(i).ok_or(TxError::InputInvalidGlobalIndex)?;
        if resolved.key_image_spent {
            return Err(TxError::InputKeyImageAlreadySpent);
        }
        let ring = match resolved.ring {
            Ok(keys) => keys,
            Err(RingLookupError::InvalidGlobalIndex) => {
                return Err(TxError::InputInvalidGlobalIndex);
            }
            Err(RingLookupError::OutputLocked) => return Err(TxError::InputSpendLockedOut),
        };
        if tx.tx.is_v2() {
            let Some(sig) = tx.tx.clsag_signatures.get(i) else {
                return Err(TxError::InputInvalidSignaturesCount);
            };
            if sig.s.len() != ring.len() {
                return Err(TxError::InputInvalidSignaturesCount);
            }
            if !clsag_verify_checked(&tx.prefix_hash, ring, image, sig) {
                return Err(TxError::InputInvalidSignatures);
            }
            continue;
        }
        let sigs = tx.tx.signatures.get(i).map_or(&[][..], Vec::as_slice);
        if u64::from(previous_block_index) >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT
            && ring.len() != sigs.len()
        {
            return Err(TxError::InputInvalidSignaturesCount);
        }
        if !check_ring_signature_checked(&tx.prefix_hash, image, ring, sigs) {
            return Err(TxError::InputInvalidSignatures);
        }
    }
    Ok(())
}

/// C++ `Core::validateBlock(cachedBlock, cache, minerReward)`: header, parent block, timestamp
/// and coinbase checks. Returns the miner reward (sum of coinbase outputs).
///
/// Fork 14 (`docs/tx-v2.md`): the coinbase must be version 2 with tagged outputs and no
/// signatures in the blocks after the fork, and version 0/1 before it
/// ([`ForkSchedule::allows_coinbase_version`]; `WRONG_TRANSACTION_VERSION`).
pub fn validate_block_header_and_coinbase(
    block: &Block,
    ctx: &ChainContext<'_>,
) -> Result<u64, ConsensusError> {
    let previous_block_index = ctx.height.wrapping_sub(1);
    let expected = block_major_version_for_height(ctx.params, block.height());
    if expected != block.header.major_version {
        return Err(BlockError::WrongVersion {
            expected,
            got: block.header.major_version,
        }
        .into());
    }

    if block.header.major_version >= 2 {
        let Some(pb) = &block.parent_block else {
            return Err(ConsensusError::DeserializationFailed(
                "missing parent block".into(),
            ));
        };
        if block.header.major_version == 2 && pb.major_version > 1 {
            return Err(BlockError::ParentBlockWrongVersion(pb.major_version).into());
        }
        let size = encode_parent_block(block, ParentBlockMode::Wire)
            .map_err(|e| ConsensusError::DeserializationFailed(e.to_string()))?
            .len();
        if size > MAX_PARENT_BLOCK_SIZE {
            return Err(BlockError::ParentBlockSizeTooBig(size).into());
        }
    }

    let limit = ctx.adjusted_time.wrapping_add(block_future_time_limit(
        previous_block_index.wrapping_add(1),
    ));
    if block.header.timestamp > limit {
        return Err(BlockError::TimestampTooFarInFuture {
            timestamp: block.header.timestamp,
            limit,
        }
        .into());
    }

    let window = timestamp_check_window(previous_block_index.wrapping_add(1));
    let ts = ctx.recent_timestamps;
    if ts.len() >= window {
        let m = median(&ts[ts.len() - window..]);
        if block.header.timestamp < m {
            return Err(BlockError::TimestampTooFarInPast {
                timestamp: block.header.timestamp,
                median: m,
            }
            .into());
        }
    }

    let coinbase = tx_err(None);
    let miner = &block.miner_tx;
    if miner.prefix.inputs.len() != 1 {
        return Err(coinbase(TxError::InputWrongCount));
    }
    let TxInput::Coinbase { height } = &miner.prefix.inputs[0] else {
        return Err(coinbase(TxError::InputUnexpectedType));
    };
    if *height != previous_block_index.wrapping_add(1) {
        return Err(coinbase(TxError::BaseInputWrongBlockIndex));
    }
    let unlock =
        u64::from(previous_block_index) + 1 + u64::from(ctx.params.mined_money_unlock_window);
    if miner.prefix.unlock_time != unlock {
        return Err(coinbase(TxError::WrongTransactionUnlockTime));
    }
    if u64::from(block.height()) >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT
        && !miner.signatures.is_empty()
    {
        return Err(coinbase(TxError::BaseInvalidSignaturesCount));
    }
    let forks = ctx.params.forks();
    if !forks.allows_coinbase_version(u64::from(previous_block_index) + 1, miner.prefix.version) {
        return Err(coinbase(TxError::WrongTransactionVersion));
    }
    let v2 = miner.is_v2();
    if v2 && (!miner.signatures.is_empty() || !miner.clsag_signatures.is_empty()) {
        return Err(coinbase(TxError::BaseInvalidSignaturesCount));
    }
    let mut reward: u64 = 0;
    for o in &miner.prefix.outputs {
        if o.amount == 0 {
            return Err(coinbase(TxError::OutputZeroAmount));
        }
        if o.view_tag().is_some() != v2 {
            return Err(coinbase(TxError::OutputUnknownType));
        }
        if !check_key(o.key()) {
            return Err(coinbase(TxError::OutputInvalidKey));
        }
        reward = reward
            .checked_add(o.amount)
            .ok_or_else(|| coinbase(TxError::OutputsAmountOverflow))?;
    }
    Ok(reward)
}

/// The `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT` rule of `Core::addBlock` (from height 1 750 000): the
/// template's `tx_hashes` must be unique, the hashes of the supplied transactions must be
/// unique, each must appear in `tx_hashes`, and both lists must be identical in order.
pub fn check_block_transaction_hashes(
    height: u32,
    tx_hashes: &[Hash],
    supplied: &[Hash],
) -> Result<(), BlockError> {
    if u64::from(height) < BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT {
        return Ok(());
    }
    let unique = |hs: &[Hash]| {
        let mut set = HashSet::with_capacity(hs.len());
        hs.iter().all(|h| set.insert(*h))
    };
    if !unique(tx_hashes) || !unique(supplied) {
        return Err(BlockError::TransactionDuplicates);
    }
    if supplied.iter().any(|h| !tx_hashes.contains(h)) || tx_hashes != supplied {
        return Err(BlockError::TransactionInconsistency);
    }
    Ok(())
}

/// The mixin rule of `Core::addBlock`: all transactions must satisfy the limits at `height`,
/// or (grace period for soft forks) the limits at `height - unlock_window` when
/// `height > unlock_window`.
pub fn check_block_mixins(
    params: &NetworkParams,
    height: u32,
    txs: &[Transaction],
) -> Result<(), TxError> {
    if validate_mixins(txs, u64::from(height)).is_ok() {
        return Ok(());
    }
    let mut window = height;
    if window > params.mined_money_unlock_window {
        window -= params.mined_money_unlock_window;
    }
    validate_mixins(txs, u64::from(window)).map_err(|_| TxError::InvalidMixin)
}

/// C++ `Currency::checkProofOfWork(block, difficulty)`: `check_hash(long_hash, difficulty)`
/// and, for major >= 2, the merge-mining proof: the parent miner transaction must carry a
/// merge-mining tag whose merkle root equals
/// `tree_hash_from_branch(blockchain_branch, aux_header_hash, path = genesis_hash)`.
pub fn check_proof_of_work(
    block: &Block,
    difficulty: u64,
    genesis_hash: &Hash,
) -> Result<(), BlockError> {
    let major = block.header.major_version;
    if !(1..=BLOCK_MAJOR_VERSION_7).contains(&major) {
        return Err(BlockError::ProofOfWorkTooWeak);
    }
    let long_hash = block_long_hash(block).map_err(|_| BlockError::ProofOfWorkTooWeak)?;
    check_proof_of_work_with_hash(block, &long_hash, difficulty, genesis_hash)
}

/// [`check_proof_of_work`] with the long hash (`block_long_hash`) computed by the caller, so a
/// sync pipeline can compute the expensive slow hashes of a whole batch in parallel before the
/// (sequential) difficulty of each block is known.
pub fn check_proof_of_work_with_hash(
    block: &Block,
    long_hash: &Hash,
    difficulty: u64,
    genesis_hash: &Hash,
) -> Result<(), BlockError> {
    let weak = BlockError::ProofOfWorkTooWeak;
    let major = block.header.major_version;
    if !(1..=BLOCK_MAJOR_VERSION_7).contains(&major) {
        return Err(weak);
    }
    if !check_hash(long_hash, difficulty) {
        return Err(weak);
    }
    if major == 1 {
        return Ok(());
    }
    let pb = block.parent_block.as_ref().ok_or_else(|| weak.clone())?;
    let tag = merge_mining_tag(&pb.base_transaction.prefix.extra).ok_or_else(|| weak.clone())?;
    if pb.blockchain_branch.len() > 8 * 32 {
        return Err(weak);
    }
    let aux = aux_block_header_hash(block).map_err(|_| weak.clone())?;
    let root = tree_hash_from_branch(&pb.blockchain_branch, &aux, Some(genesis_hash));
    if root != tag.merkle_root {
        return Err(weak);
    }
    Ok(())
}

/// Everything `Core::addBlock` checks between "block decoded" and "block stored", in the same
/// order: transaction sizes, cumulative size limit, [`validate_block_header_and_coinbase`],
/// difficulty, [`check_block_transaction_hashes`], [`check_block_mixins`], every
/// [`validate_transaction`] (after the fork-14 [`check_transaction_version`]), reward, then
/// checkpoint or [`check_proof_of_work`].
///
/// `block_hash` is only compared against `ctx.checkpoint`.
pub fn validate_block(
    block: &Block,
    block_hash: &Hash,
    txs: &[BlockTransaction<'_>],
    ctx: &ChainContext<'_>,
) -> Result<AcceptedBlock, ConsensusError> {
    let previous_block_index = ctx.height.wrapping_sub(1);

    // Core::extractTransactions
    let mut txs_size: u64 = 0;
    for t in txs {
        if t.blob_size > CRYPTONOTE_MAX_TX_SIZE {
            return Err(ConsensusError::DeserializationFailed(format!(
                "transaction of {} bytes",
                t.blob_size
            )));
        }
        txs_size += t.blob_size as u64;
    }
    let coinbase_size = encode_transaction(&block.miner_tx)
        .map_err(|e| ConsensusError::DeserializationFailed(e.to_string()))?
        .len();
    let cumulative_size = coinbase_size as u64 + txs_size;
    let size_limit = max_block_cumulative_size(u64::from(previous_block_index) + 1);
    if cumulative_size > size_limit {
        return Err(BlockError::CumulativeBlockSizeTooBig {
            size: cumulative_size,
            limit: size_limit,
        }
        .into());
    }

    let miner_reward = validate_block_header_and_coinbase(block, ctx)?;

    let difficulty = difficulty_for_next_block(
        ctx.params,
        ctx.height,
        ctx.difficulty_timestamps,
        ctx.difficulty_cumulative,
    );
    if difficulty == 0 {
        return Err(BlockError::DifficultyOverhead.into());
    }

    let supplied: Vec<Hash> = txs.iter().map(|t| t.hash).collect();
    check_block_transaction_hashes(block.height(), &block.tx_hashes, &supplied)?;

    let decoded: Vec<Transaction> = txs.iter().map(|t| t.tx.clone()).collect();
    check_block_mixins(ctx.params, block.height(), &decoded).map_err(tx_err(None))?;

    let forks = ctx.params.forks();
    let mut seen = HashSet::new();
    let mut fees: u64 = 0;
    for (i, t) in txs.iter().enumerate() {
        check_transaction_version(&forks, u64::from(ctx.height), t.tx).map_err(tx_err(Some(i)))?;
        let fee = validate_transaction(t, previous_block_index, ctx.in_checkpoint_zone, &mut seen)
            .map_err(tx_err(Some(i)))?;
        fees = fees.wrapping_add(fee);
    }

    let sizes = ctx.recent_block_sizes;
    let window = sizes.len().min(CRYPTONOTE_REWARD_BLOCKS_WINDOW);
    let size_median = median(&sizes[sizes.len() - window..]);
    let (reward, emission_change) = get_block_reward(
        block.header.major_version,
        size_median,
        cumulative_size,
        ctx.already_generated_coins,
        fees,
    )
    .map_err(|e| BlockError::CumulativeBlockSizeTooBig {
        size: e.size,
        limit: 2 * e.median,
    })?;
    if miner_reward != reward {
        return Err(BlockError::BlockRewardMismatch {
            expected: reward,
            got: miner_reward,
        }
        .into());
    }

    if ctx.in_checkpoint_zone {
        if let Some(cp) = ctx.checkpoint
            && cp != *block_hash
        {
            return Err(BlockError::CheckpointBlockHashMismatch.into());
        }
    } else {
        check_proof_of_work(block, difficulty, &ctx.genesis_hash)?;
    }

    Ok(AcceptedBlock {
        difficulty,
        cumulative_size,
        fees,
        reward,
        emission_change,
    })
}

// ---------------------------------------------------------------------------------------------
// Relay / template policy (not consensus rules for received blocks)
// ---------------------------------------------------------------------------------------------

/// Why the C++ pool (`Core::isTransactionValidForPool`) would refuse a transaction before or
/// after the consensus checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PoolPolicyError {
    /// Mixin outside the limits at the current top block index.
    #[error("invalid mixin")]
    Mixin,
    /// `extra.size() >= MAX_EXTRA_SIZE_POOL` (2200; Hugin messages fit below this).
    #[error("extra of {0} bytes too large for the pool")]
    ExtraTooLarge(usize),
    /// Larger than `2 * median - CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE`.
    #[error("transaction of {size} bytes larger than {max}")]
    TooBig {
        /// Blob size.
        size: u64,
        /// Allowed maximum.
        max: u64,
    },
    /// Fee below `MINIMUM_FEE` and not a fusion transaction.
    #[error("fee {0} too small and not a fusion transaction")]
    FeeTooSmall(u64),
}

/// The non-consensus parts of C++ `Core::isTransactionValidForPool` (the consensus part is
/// [`validate_transaction`] against the top block). `top_block_index` is
/// `getTopBlockIndex()`, `block_median_size` the node's cached median.
pub fn check_pool_policy(
    tx: &Transaction,
    blob_size: usize,
    fee: u64,
    top_block_index: u32,
    block_median_size: u64,
) -> Result<(), PoolPolicyError> {
    if crate::reward::validate_mixin(tx, mixin_limits(u64::from(top_block_index))).is_err() {
        return Err(PoolPolicyError::Mixin);
    }
    if tx.prefix.extra.len() as u64 >= MAX_EXTRA_SIZE_POOL {
        return Err(PoolPolicyError::ExtraTooLarge(tx.prefix.extra.len()));
    }
    let max = max_transaction_allowed_size(block_median_size);
    if blob_size as u64 > max {
        return Err(PoolPolicyError::TooBig {
            size: blob_size as u64,
            max,
        });
    }
    let fusion = fee == 0 && is_fusion_transaction(tx, blob_size, top_block_index);
    if !fusion && fee < minimum_fee() {
        return Err(PoolPolicyError::FeeTooSmall(fee));
    }
    Ok(())
}

/// C++ `Core::validateBlockTemplateTransaction`: transactions with `extra.size() >=
/// MAX_EXTRA_SIZE_BLOCK` (128) are kept out of block templates (Hugin messages stay in the
/// pool), and the mixin must fit the limits at `height`. Not checked on received blocks.
#[must_use]
pub fn is_valid_for_block_template(tx: &Transaction, height: u64) -> bool {
    (tx.prefix.extra.len() as u64) < MAX_EXTRA_SIZE_BLOCK
        && crate::reward::validate_mixin(tx, mixin_limits(height)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_limits() {
        assert_eq!(block_future_time_limit(619_999), 7200);
        assert_eq!(block_future_time_limit(620_000), 270);
        assert_eq!(block_future_time_limit(700_000), 540);
        assert_eq!(timestamp_check_window(799_999), 60);
        assert_eq!(timestamp_check_window(800_000), 11);
    }

    #[test]
    fn shuffle_check() {
        let a = Hash([1; 32]);
        let b = Hash([2; 32]);
        assert!(check_block_transaction_hashes(1_749_999, &[a, a], &[b]).is_ok());
        assert_eq!(
            check_block_transaction_hashes(1_750_000, &[a, a], &[a, a]),
            Err(BlockError::TransactionDuplicates)
        );
        assert_eq!(
            check_block_transaction_hashes(1_750_000, &[a, b], &[b, a]),
            Err(BlockError::TransactionInconsistency)
        );
        assert_eq!(
            check_block_transaction_hashes(1_750_000, &[a, b], &[a]),
            Err(BlockError::TransactionInconsistency)
        );
        assert!(check_block_transaction_hashes(1_750_000, &[a, b], &[a, b]).is_ok());
    }

    #[test]
    fn semantic_rules() {
        use xkr_types::{TxOutput, TxOutputTarget};
        let mut tx = Transaction::default();
        assert_eq!(validate_semantic(&tx), Err(TxError::EmptyInputs));
        tx.prefix.inputs.push(TxInput::Coinbase { height: 1 });
        assert_eq!(validate_semantic(&tx), Err(TxError::InputUnknownType));
        // identity point is a valid output key encoding
        let mut id = [0u8; 32];
        id[0] = 1;
        tx.prefix.inputs.clear();
        tx.prefix.outputs.push(TxOutput {
            amount: 0,
            target: TxOutputTarget::Key(PublicKey(id)),
        });
        tx.prefix.inputs.push(TxInput::Key {
            amount: 5,
            output_indexes: vec![3, 0],
            key_image: KeyImage(id),
        });
        assert_eq!(validate_semantic(&tx), Err(TxError::OutputZeroAmount));
        tx.prefix.outputs[0].amount = 4;
        assert_eq!(
            validate_semantic(&tx),
            Err(TxError::InputIdenticalOutputIndexes)
        );
        if let TxInput::Key { output_indexes, .. } = &mut tx.prefix.inputs[0] {
            *output_indexes = vec![0, 1];
        }
        assert_eq!(validate_semantic(&tx), Ok(1));
        tx.prefix.outputs[0].amount = 6;
        assert_eq!(validate_semantic(&tx), Err(TxError::WrongAmount));
    }

    /// A one-input version-2 spend of ring member `real` of `ring` (amount 1000, fee 100).
    fn v2_spend(
        ring: &[PublicKey],
        secret: &xkr_types::SecretKey,
        real: usize,
    ) -> (Transaction, Hash) {
        use xkr_crypto::{clsag_sign, generate_key_image, generate_keys, os_rng};
        use xkr_serialization::transaction_prefix_hash;
        use xkr_types::{TransactionPrefix, TxOutput, TxOutputTarget};
        let mut random = os_rng();
        let key_image = generate_key_image(&ring[real], secret);
        let prefix = TransactionPrefix {
            version: 2,
            unlock_time: 0,
            inputs: vec![TxInput::Key {
                amount: 1000,
                output_indexes: xkr_types::absolute_to_relative(
                    &(0..ring.len() as u32).collect::<Vec<_>>(),
                ),
                key_image,
            }],
            outputs: vec![TxOutput {
                amount: 900,
                target: TxOutputTarget::TaggedKey {
                    key: generate_keys(&mut random).0,
                    view_tag: 7,
                },
            }],
            extra: vec![],
        };
        let prefix_hash = transaction_prefix_hash(&prefix);
        let sig = clsag_sign(&prefix_hash, ring, secret, real, &mut random).unwrap();
        (
            Transaction {
                prefix,
                signatures: Vec::new(),
                clsag_signatures: vec![sig],
            },
            prefix_hash,
        )
    }

    #[test]
    fn v2_transactions_are_checked_with_clsag() {
        use xkr_crypto::{generate_key_image, generate_keys, generate_ring_signature, os_rng};
        use xkr_serialization::{decode_transaction, transaction_prefix_hash};
        use xkr_types::{TxOutput, TxOutputTarget};

        let mut random = os_rng();
        let keys: Vec<_> = (0..3).map(|_| generate_keys(&mut random)).collect();
        let ring: Vec<PublicKey> = keys.iter().map(|k| k.0).collect();
        let (tx, prefix_hash) = v2_spend(&ring, &keys[1].1, 1);
        let blob = encode_transaction(&tx).unwrap();
        assert_eq!(decode_transaction(&blob).unwrap(), tx);
        assert_eq!(validate_semantic(&tx), Ok(100));

        let resolved = [ResolvedInput {
            key_image_spent: false,
            ring: Ok(&ring),
        }];
        let entry = |tx: &Transaction| -> Result<u64, TxError> {
            let bt = BlockTransaction {
                tx,
                hash: Hash::ZERO,
                prefix_hash,
                blob_size: blob.len(),
                inputs: &resolved,
            };
            validate_transaction(&bt, 100, false, &mut HashSet::new())
        };
        assert_eq!(entry(&tx), Ok(100));

        // altered response / challenge, wrong signature count
        let mut bad = tx.clone();
        bad.clsag_signatures[0].s[2].0[5] ^= 1;
        assert_eq!(entry(&bad), Err(TxError::InputInvalidSignatures));
        let mut bad = tx.clone();
        bad.clsag_signatures[0].c1.0[0] ^= 1;
        assert_eq!(entry(&bad), Err(TxError::InputInvalidSignatures));
        let mut bad = tx.clone();
        bad.clsag_signatures[0].s.pop();
        assert_eq!(entry(&bad), Err(TxError::InputInvalidSignaturesCount));
        let mut bad = tx.clone();
        bad.clsag_signatures.clear();
        assert_eq!(entry(&bad), Err(TxError::InputInvalidSignaturesCount));
        // a different prefix (the fee changed) no longer matches the signature
        let mut bad = tx.clone();
        bad.prefix.outputs[0].amount = 800;
        let bt = BlockTransaction {
            tx: &bad,
            hash: Hash::ZERO,
            prefix_hash: transaction_prefix_hash(&bad.prefix),
            blob_size: blob.len(),
            inputs: &resolved,
        };
        assert_eq!(
            validate_transaction(&bt, 100, false, &mut HashSet::new()),
            Err(TxError::InputInvalidSignatures)
        );
        // untagged output in a version-2 transaction, tagged output in version 1
        let mut bad = tx.clone();
        bad.prefix.outputs[0].target = TxOutputTarget::Key(*tx.prefix.outputs[0].key());
        assert_eq!(validate_semantic(&bad), Err(TxError::OutputUnknownType));
        let mut bad = tx.clone();
        bad.prefix.version = 1;
        assert_eq!(validate_semantic(&bad), Err(TxError::OutputUnknownType));
        // spent in the chain
        let spent = [ResolvedInput {
            key_image_spent: true,
            ring: Ok(&ring),
        }];
        let bt = BlockTransaction {
            tx: &tx,
            hash: Hash::ZERO,
            prefix_hash,
            blob_size: blob.len(),
            inputs: &spent,
        };
        assert_eq!(
            validate_transaction(&bt, 100, false, &mut HashSet::new()),
            Err(TxError::InputKeyImageAlreadySpent)
        );

        // A version-1 spend of the same output in the same block: the key image is the same,
        // so the block-wide set rejects the second spend whatever the order.
        let key_image = generate_key_image(&ring[1], &keys[1].1);
        let mut v1 = tx.clone();
        v1.prefix.version = 1;
        v1.prefix.outputs = vec![TxOutput {
            amount: 900,
            target: TxOutputTarget::Key(generate_keys(&mut random).0),
        }];
        v1.clsag_signatures.clear();
        let v1_hash = transaction_prefix_hash(&v1.prefix);
        v1.signatures = vec![
            generate_ring_signature(&v1_hash, &key_image, &ring, &keys[1].1, 1, &mut random)
                .unwrap(),
        ];
        let v1_entry = BlockTransaction {
            tx: &v1,
            hash: Hash::ZERO,
            prefix_hash: v1_hash,
            blob_size: 100,
            inputs: &resolved,
        };
        let v2_entry = BlockTransaction {
            tx: &tx,
            hash: Hash::ZERO,
            prefix_hash,
            blob_size: blob.len(),
            inputs: &resolved,
        };
        for (first, second) in [(&v1_entry, &v2_entry), (&v2_entry, &v1_entry)] {
            let mut seen = HashSet::new();
            assert_eq!(validate_transaction(first, 100, false, &mut seen), Ok(100));
            assert_eq!(
                validate_transaction(second, 100, false, &mut seen),
                Err(TxError::InputKeyImageAlreadySpent)
            );
        }
    }

    #[test]
    fn transaction_versions_follow_the_fork_table() {
        use crate::forks::{Fork14Params, RingSizeRule};
        use crate::params::{MAINNET, TESTNET};
        use xkr_crypto::{generate_keys, os_rng};

        let mut random = os_rng();
        let keys: Vec<_> = (0..3).map(|_| generate_keys(&mut random)).collect();
        let ring: Vec<PublicKey> = keys.iter().map(|k| k.0).collect();
        let (v2, _) = v2_spend(&ring, &keys[0].1, 0);
        let mut v1 = v2.clone();
        v1.prefix.version = 1;
        let mut v0 = v2.clone();
        v0.prefix.version = 0;

        // Mainnet: fork 14 at 2 750 000 with 28 800 blocks of grace.
        let mainnet = MAINNET.forks();
        for h in [0, 2_617_000, 2_700_001, 2_750_000] {
            assert_eq!(check_transaction_version(&mainnet, h, &v1), Ok(()));
            assert_eq!(check_transaction_version(&mainnet, h, &v0), Ok(()));
            assert_eq!(
                check_transaction_version(&mainnet, h, &v2),
                Err(TxError::WrongTransactionVersion)
            );
        }
        for h in [2_750_001, 2_778_800] {
            assert_eq!(check_transaction_version(&mainnet, h, &v1), Ok(()));
            assert_eq!(check_transaction_version(&mainnet, h, &v2), Ok(()));
        }
        for h in [2_778_801, u64::from(u32::MAX)] {
            assert_eq!(
                check_transaction_version(&mainnet, h, &v1),
                Err(TxError::WrongTransactionVersion)
            );
            assert_eq!(check_transaction_version(&mainnet, h, &v2), Ok(()));
        }
        // An unscheduled fork 14: version 1 only, at any height.
        let unscheduled = MAINNET.with_fork14_height(None).forks();
        for h in [0, 2_617_000, u64::from(u32::MAX)] {
            assert_eq!(check_transaction_version(&unscheduled, h, &v1), Ok(()));
            assert_eq!(check_transaction_version(&unscheduled, h, &v0), Ok(()));
            assert_eq!(
                check_transaction_version(&unscheduled, h, &v2),
                Err(TxError::WrongTransactionVersion)
            );
        }

        let params = TESTNET
            .with_fork14_height(Some(50))
            .with_fork14_params(Fork14Params {
                grace_blocks: 10,
                v2_ring_size: RingSizeRule::Range { min: 2, max: 2 },
            });
        let s = params.forks();
        assert_eq!(
            check_transaction_version(&s, 50, &v2),
            Err(TxError::WrongTransactionVersion)
        );
        // ring size 3 against a fixed ring size of 2
        assert_eq!(
            check_transaction_version(&s, 51, &v2),
            Err(TxError::InvalidMixin)
        );
        let (v2_pair, _) = v2_spend(&ring[..2], &keys[0].1, 0);
        assert_eq!(check_transaction_version(&s, 51, &v2_pair), Ok(()));
        assert_eq!(check_transaction_version(&s, 60, &v1), Ok(()));
        assert_eq!(
            check_transaction_version(&s, 61, &v1),
            Err(TxError::WrongTransactionVersion)
        );
        assert_eq!(
            check_transaction_version(&s, 61, &v0),
            Err(TxError::WrongTransactionVersion)
        );
        assert_eq!(check_transaction_version(&s, 61, &v2_pair), Ok(()));
    }

    #[test]
    fn coinbase_version_follows_the_fork_table() {
        use crate::params::TESTNET;
        use xkr_types::{BlockHeader, ParentBlock, TransactionPrefix, TxOutput, TxOutputTarget};

        let params = TESTNET.with_fork14_height(Some(5));
        let block_at = |height: u32, version: u8, tagged: bool| {
            let key = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng()).0;
            let target = if tagged {
                TxOutputTarget::TaggedKey { key, view_tag: 1 }
            } else {
                TxOutputTarget::Key(key)
            };
            let mut extra = vec![0x03, 33, 0x00];
            extra.extend_from_slice(&[0u8; 32]);
            let base = Transaction {
                prefix: TransactionPrefix {
                    version: 1,
                    extra,
                    ..TransactionPrefix::default()
                },
                ..Transaction::default()
            };
            Block {
                header: BlockHeader {
                    major_version: params.forks().block_major_version_for(u64::from(height)),
                    minor_version: 0,
                    timestamp: 0,
                    prev_hash: Hash::ZERO,
                    nonce: 0,
                },
                parent_block: Some(ParentBlock {
                    major_version: 1,
                    minor_version: 0,
                    prev_hash: Hash::ZERO,
                    transaction_count: 1,
                    base_transaction_branch: vec![],
                    base_transaction: base,
                    blockchain_branch: vec![],
                }),
                miner_tx: Transaction {
                    prefix: TransactionPrefix {
                        version,
                        unlock_time: u64::from(height) + 1,
                        inputs: vec![TxInput::Coinbase { height }],
                        outputs: vec![TxOutput { amount: 1, target }],
                        extra: vec![],
                    },
                    ..Transaction::default()
                },
                tx_hashes: vec![],
            }
        };
        let ctx = |height: u32| ChainContext {
            params: &params,
            height,
            prev_hash: Hash::ZERO,
            recent_timestamps: &[],
            difficulty_timestamps: &[],
            difficulty_cumulative: &[],
            recent_block_sizes: &[],
            already_generated_coins: 0,
            adjusted_time: 0,
            genesis_hash: Hash::ZERO,
            in_checkpoint_zone: false,
            checkpoint: None,
        };
        let wrong = Err(ConsensusError::Transaction {
            tx_index: None,
            error: TxError::WrongTransactionVersion,
        });
        // height 5 = F: v5 block, version-1 coinbase
        assert_eq!(
            validate_block_header_and_coinbase(&block_at(5, 1, false), &ctx(5)),
            Ok(1)
        );
        assert_eq!(
            validate_block_header_and_coinbase(&block_at(5, 2, true), &ctx(5)),
            wrong
        );
        // height 6 = F + 1: v7 block, version-2 coinbase with tagged outputs
        assert_eq!(block_at(6, 2, true).header.major_version, 7);
        assert_eq!(
            validate_block_header_and_coinbase(&block_at(6, 2, true), &ctx(6)),
            Ok(1)
        );
        assert_eq!(
            validate_block_header_and_coinbase(&block_at(6, 1, false), &ctx(6)),
            wrong
        );
        assert_eq!(
            validate_block_header_and_coinbase(&block_at(6, 2, false), &ctx(6)),
            Err(ConsensusError::Transaction {
                tx_index: None,
                error: TxError::OutputUnknownType,
            })
        );
    }
}
