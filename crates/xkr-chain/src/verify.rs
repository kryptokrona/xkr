//! Stages 2 and 3 of the pipeline: sequential context checks and parallel ring signatures.
//!
//! Stage 2 walks the batch in height order. For each block it checks everything that needs
//! chain state (`Core::addBlock` / `Core::validateBlock` / the input half of
//! `Core::validateTransaction`): header and coinbase against the timestamp window, the
//! difficulty, size and reward, the checkpoint, the proof of work (with the slow hash from
//! stage 1), spent key images and ring members. Outputs and key images of the blocks already
//! accepted in this batch live in an [`Overlay`] on top of the storage snapshot. Ring
//! signatures are not checked here; they become [`SigJob`]s.
//!
//! Stage 3 verifies all [`SigJob`]s of the batch in parallel with rayon (Cuprate's
//! `batch_verifier`): CryptoNote ring signatures for transaction versions 0/1, CLSAG for
//! version 2 (fork 14, `docs/tx-v2.md`), one job per input either way.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use xkr_consensus::params::TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT;
use xkr_consensus::reward::{
    get_block_reward, is_spend_time_unlocked, max_block_cumulative_size, median, mixin_limits,
    validate_mixin,
};
use xkr_consensus::validation::check_block_transaction_hashes;
use xkr_consensus::validation::validate_block_header_and_coinbase;
use xkr_consensus::{
    BlockError, ChainContext, ConsensusError, NetworkParams, RingLookupError, TxError,
    check_proof_of_work_with_hash, check_transaction_version, difficulty_for_next_block,
};
use xkr_crypto::{check_ring_signature_checked, clsag_verify_checked};
use xkr_serialization::block_long_hash;
use xkr_storage::Reader;
use xkr_txpool::canonical_ring_indexes;
use xkr_types::{Block, Hash, KeyImage, PublicKey, TxInput};

use crate::checkpoints::Checkpoints;
use crate::error::{ChainError, RejectReason};
use crate::headers::ContextData;
use crate::prepare::{PreparedBlock, PreparedTx};

/// Values computed while accepting a block (what `IBlockchainCache::pushBlock` stores).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderVerdict {
    /// Difficulty the block met.
    pub difficulty: u64,
    /// Coinbase + transaction sizes.
    pub cumulative_size: u64,
    /// Coins created (`emissionChange`, reinterpreted like C++).
    pub emission_change: i64,
}

fn tx_error(tx_index: Option<usize>, error: TxError) -> RejectReason {
    RejectReason::Consensus(ConsensusError::Transaction { tx_index, error })
}

fn block_error(e: BlockError) -> RejectReason {
    RejectReason::Consensus(ConsensusError::Block(e))
}

/// The mixin rule of `Core::addBlock` (see `xkr_consensus::validation::check_block_mixins`),
/// over prepared transactions.
fn check_mixins(params: &NetworkParams, height: u32, txs: &[PreparedTx]) -> Result<(), TxError> {
    let ok_at = |h: u32| {
        let limits = mixin_limits(u64::from(h));
        txs.iter().all(|t| validate_mixin(&t.tx, limits).is_ok())
    };
    if ok_at(height) {
        return Ok(());
    }
    let mut window = height;
    if window > params.mined_money_unlock_window {
        window -= params.mined_money_unlock_window;
    }
    if ok_at(window) {
        Ok(())
    } else {
        Err(TxError::InvalidMixin)
    }
}

/// The consensus view of `ctx` for `block` (shared by [`check_header`] and
/// [`check_header_cheap`]).
fn chain_context<'a>(
    params: &'a NetworkParams,
    block: &Block,
    h32: u32,
    ctx: &'a ContextData,
    genesis: &Hash,
    now: u64,
) -> ChainContext<'a> {
    ChainContext {
        params,
        height: h32,
        prev_hash: block.header.prev_hash,
        recent_timestamps: &ctx.recent_timestamps,
        difficulty_timestamps: &ctx.difficulty_timestamps,
        difficulty_cumulative: &ctx.difficulty_cumulative,
        recent_block_sizes: &ctx.recent_block_sizes,
        already_generated_coins: ctx.already_generated_coins,
        adjusted_time: now,
        genesis_hash: *genesis,
        in_checkpoint_zone: false,
        checkpoint: None,
    }
}

/// The checks of [`check_header`] that need only the block template and its context: major
/// version for the height (fork table), parent block, timestamp window, coinbase, and a
/// non-zero difficulty, which is returned. No transaction and no slow hash is needed, so
/// `Chain::add_block` runs this before preparing the block; [`check_header`] repeats it in the
/// full C++ order afterwards, so the outcome for a valid block is unchanged.
pub fn check_header_cheap(
    params: &NetworkParams,
    block: &Block,
    ctx: &ContextData,
    genesis: &Hash,
    now: u64,
) -> Result<u64, RejectReason> {
    let h32 = u32::try_from(ctx.height).map_err(|_| RejectReason::decode("height overflow"))?;
    let cc = chain_context(params, block, h32, ctx, genesis, now);
    validate_block_header_and_coinbase(block, &cc)?;
    let difficulty = difficulty_for_next_block(
        params,
        h32,
        &ctx.difficulty_timestamps,
        &ctx.difficulty_cumulative,
    );
    if difficulty == 0 {
        return Err(block_error(BlockError::DifficultyOverhead));
    }
    Ok(difficulty)
}

/// Every check of `Core::addBlock` that does not look at transaction inputs, in C++ order:
/// size, header/coinbase/timestamps, difficulty, transaction list consistency, mixins, reward,
/// checkpoint, proof of work.
#[allow(clippy::too_many_arguments)]
pub fn check_header(
    params: &NetworkParams,
    checkpoints: &Checkpoints,
    p: &PreparedBlock,
    ctx: &ContextData,
    genesis: &Hash,
    now: u64,
    check_pow: bool,
) -> Result<HeaderVerdict, RejectReason> {
    let height = ctx.height;
    let h32 = u32::try_from(height).map_err(|_| RejectReason::decode("height overflow"))?;
    let cumulative_size = p.cumulative_size();
    let limit = max_block_cumulative_size(height);
    if cumulative_size > limit {
        return Err(block_error(BlockError::CumulativeBlockSizeTooBig {
            size: cumulative_size,
            limit,
        }));
    }

    let cc = chain_context(params, &p.block, h32, ctx, genesis, now);
    let miner_reward = validate_block_header_and_coinbase(&p.block, &cc)?;

    let difficulty = difficulty_for_next_block(
        params,
        h32,
        &ctx.difficulty_timestamps,
        &ctx.difficulty_cumulative,
    );
    if difficulty == 0 {
        return Err(block_error(BlockError::DifficultyOverhead));
    }

    let supplied: Vec<Hash> = p.txs.iter().map(|t| t.hash).collect();
    check_block_transaction_hashes(h32, &p.block.tx_hashes, &supplied).map_err(block_error)?;
    check_mixins(params, h32, &p.txs).map_err(|e| tx_error(None, e))?;
    // Fork 14: transaction versions allowed at this height, version-2 ring sizes.
    let forks = params.forks();
    for (i, t) in p.txs.iter().enumerate() {
        check_transaction_version(&forks, height, &t.tx).map_err(|e| tx_error(Some(i), e))?;
    }

    let sizes = &ctx.recent_block_sizes;
    let size_median = median(sizes);
    let (reward, emission_change) = get_block_reward(
        p.block.header.major_version,
        size_median,
        cumulative_size,
        ctx.already_generated_coins,
        p.fees(),
    )
    .map_err(|e| {
        block_error(BlockError::CumulativeBlockSizeTooBig {
            size: e.size,
            limit: 2 * e.median,
        })
    })?;
    if miner_reward != reward {
        return Err(block_error(BlockError::BlockRewardMismatch {
            expected: reward,
            got: miner_reward,
        }));
    }

    if !checkpoints.check(height, &p.hash) {
        return Err(RejectReason::Checkpoint(height));
    }
    if check_pow {
        let long_hash = match p.long_hash {
            Some(h) => h,
            None => block_long_hash(&p.block)
                .map_err(|_| block_error(BlockError::ProofOfWorkTooWeak))?,
        };
        check_proof_of_work_with_hash(&p.block, &long_hash, difficulty, genesis)
            .map_err(block_error)?;
    }
    Ok(HeaderVerdict {
        difficulty,
        cumulative_size,
        emission_change,
    })
}

/// Outputs and key images of blocks accepted earlier in the batch, on top of a storage
/// snapshot (which does not change while the batch is verified).
#[derive(Debug, Default)]
pub struct Overlay {
    /// Output count per amount in the snapshot (lazily read).
    base_counts: HashMap<u64, u64>,
    /// Outputs created in the batch, per amount, in global-index order: `(key, unlock_time)`.
    outputs: HashMap<u64, Vec<(PublicKey, u64)>>,
    /// Key images spent in the batch.
    key_images: HashSet<KeyImage>,
}

impl Overlay {
    fn base_count(&mut self, reader: &Reader, amount: u64) -> Result<u64, ChainError> {
        if let Some(c) = self.base_counts.get(&amount) {
            return Ok(*c);
        }
        let c = reader.output_count(amount)?;
        self.base_counts.insert(amount, c);
        Ok(c)
    }

    /// Whether `ki` is spent in the snapshot or the batch.
    pub fn is_spent(&self, reader: &Reader, ki: &KeyImage) -> Result<bool, ChainError> {
        Ok(self.key_images.contains(ki) || reader.is_key_image_spent(ki)?)
    }

    /// `extractKeyOutputKeys(amount, prev_index, indexes)` over snapshot + batch, with the
    /// semantics of the C++ main chain (`DatabaseBlockchainCache::extractKeyOutputs`): the ring
    /// is the outputs at `indexes` sorted by global index, each index once
    /// ([`xkr_txpool::canonical_ring_indexes`], whatever order `indexes` come in), and an index
    /// without an output is left out; the shorter ring then fails the signature count or the
    /// signature, as in C++. Every member must be unlocked at `prev_index`
    /// ([`RingLookupError::OutputLocked`] otherwise, checked in ring order).
    pub fn resolve_ring(
        &mut self,
        reader: &Reader,
        amount: u64,
        indexes: &[u32],
        prev_index: u32,
        now: u64,
    ) -> Result<Result<Vec<PublicKey>, RingLookupError>, ChainError> {
        let base = self.base_count(reader, amount)?;
        let ring = canonical_ring_indexes(indexes.to_vec());
        // Ascending: the members in the snapshot first, then those created in the batch.
        let split = ring.partition_point(|i| u64::from(*i) < base);
        let stored: Vec<u64> = ring[..split].iter().map(|i| u64::from(*i)).collect();
        let mut members: Vec<(PublicKey, u64)> = reader
            .ring_keys(amount, &stored)?
            .into_iter()
            .map(|(_, key, unlock, _)| (key, unlock))
            .collect();
        if let Some(batch) = self.outputs.get(&amount) {
            members.extend(ring[split..].iter().filter_map(|i| {
                usize::try_from(u64::from(*i) - base)
                    .ok()
                    .and_then(|j| batch.get(j))
                    .copied()
            }));
        }
        let mut keys = Vec::with_capacity(members.len());
        for (key, unlock) in members {
            if !is_spend_time_unlocked(unlock, prev_index, now) {
                return Ok(Err(RingLookupError::OutputLocked));
            }
            keys.push(key);
        }
        Ok(Ok(keys))
    }

    /// Records the outputs (miner transaction first, then the transactions in order, matching
    /// the global index assignment of `xkr_storage::ops::push_blocks`) and key images of an
    /// accepted block.
    pub fn add_block(&mut self, reader: &Reader, p: &PreparedBlock) -> Result<(), ChainError> {
        let prefixes =
            std::iter::once(&p.block.miner_tx.prefix).chain(p.txs.iter().map(|t| &t.tx.prefix));
        for prefix in prefixes {
            for o in &prefix.outputs {
                self.base_count(reader, o.amount)?;
                self.outputs
                    .entry(o.amount)
                    .or_default()
                    .push((*o.key(), prefix.unlock_time));
            }
        }
        for t in &p.txs {
            for input in &t.tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    self.key_images.insert(*key_image);
                }
            }
        }
        Ok(())
    }
}

/// One ring signature to verify in stage 3.
#[derive(Debug, Clone)]
pub struct SigJob {
    /// Index of the block in the batch.
    pub block: usize,
    /// Index of the transaction in the block.
    pub tx: usize,
    /// Index of the input.
    pub input: usize,
    /// Resolved ring member keys.
    pub ring: Vec<PublicKey>,
}

/// The input half of `Core::validateTransaction` for every transaction of a block
/// (`TransactionValidatorState` across the block, spent key images, ring members, signature
/// count). Pushes one [`SigJob`] per input. `Ok(Err(_))` is a rejection.
pub fn check_inputs(
    p: &PreparedBlock,
    block_index: usize,
    prev_index: u32,
    now: u64,
    reader: &Reader,
    overlay: &mut Overlay,
    jobs: &mut Vec<SigJob>,
) -> Result<Result<(), RejectReason>, ChainError> {
    let mut seen: HashSet<KeyImage> = HashSet::new();
    for (ti, t) in p.txs.iter().enumerate() {
        for (ii, input) in t.tx.prefix.inputs.iter().enumerate() {
            let TxInput::Key {
                amount, key_image, ..
            } = input
            else {
                return Ok(Err(tx_error(Some(ti), TxError::InputUnknownType)));
            };
            if !seen.insert(*key_image) || overlay.is_spent(reader, key_image)? {
                return Ok(Err(tx_error(Some(ti), TxError::InputKeyImageAlreadySpent)));
            }
            // Ring order: ascending global indexes without duplicates
            // (`xkr_txpool::ring_global_indexes`).
            let ring = match overlay.resolve_ring(
                reader,
                *amount,
                &input.absolute_output_indexes(),
                prev_index,
                now,
            )? {
                Ok(r) => r,
                Err(RingLookupError::InvalidGlobalIndex) => {
                    return Ok(Err(tx_error(Some(ti), TxError::InputInvalidGlobalIndex)));
                }
                Err(RingLookupError::OutputLocked) => {
                    return Ok(Err(tx_error(Some(ti), TxError::InputSpendLockedOut)));
                }
            };
            // Version 2: one CLSAG response per ring member, always (`docs/tx-v2.md`).
            let v2 = t.tx.is_v2();
            let sig_count = if v2 {
                t.tx.clsag_signatures
                    .get(ii)
                    .map_or(usize::MAX, |s| s.s.len())
            } else {
                t.tx.signatures.get(ii).map_or(0, Vec::len)
            };
            if (v2 || u64::from(prev_index) >= TRANSACTION_SIGNATURE_COUNT_VALIDATION_HEIGHT)
                && ring.len() != sig_count
            {
                return Ok(Err(tx_error(
                    Some(ti),
                    TxError::InputInvalidSignaturesCount,
                )));
            }
            jobs.push(SigJob {
                block: block_index,
                tx: ti,
                input: ii,
                ring,
            });
        }
    }
    Ok(Ok(()))
}

/// Stage 3: verifies every job in parallel; returns the first failing `(block, tx)` in batch
/// order.
#[must_use]
pub fn verify_signatures(blocks: &[PreparedBlock], jobs: &[SigJob]) -> Option<(usize, usize)> {
    jobs.par_iter()
        .filter(|j| {
            let Some(t) = blocks.get(j.block).and_then(|b| b.txs.get(j.tx)) else {
                return true;
            };
            let Some(TxInput::Key { key_image, .. }) = t.tx.prefix.inputs.get(j.input) else {
                return true;
            };
            let Some(image) = t
                .key_images
                .get(j.input)
                .filter(|c| c.key_image() == key_image)
            else {
                return true;
            };
            if t.tx.is_v2() {
                return t
                    .tx
                    .clsag_signatures
                    .get(j.input)
                    .is_none_or(|sig| !clsag_verify_checked(&t.prefix_hash, &j.ring, image, sig));
            }
            let sigs = t.tx.signatures.get(j.input).map_or(&[][..], Vec::as_slice);
            !check_ring_signature_checked(&t.prefix_hash, image, &j.ring, sigs)
        })
        .map(|j| (j.block, j.tx))
        .min()
}

/// Error for a failed signature.
#[must_use]
pub fn signature_error(tx: usize) -> RejectReason {
    tx_error(Some(tx), TxError::InputInvalidSignatures)
}
