//! Stage 1 of the pipeline: everything about a block that does not depend on chain state.
//!
//! Decoding, hashing, the proof-of-work slow hash and `validateSemantic` of every transaction.
//! These are pure functions of the block bytes, so a whole batch runs in parallel on rayon
//! (Cuprate's "unchanged by context" verification, `batch_prepare_main_chain_blocks`).
//!
//! Stage 1 is split in two so the cheap checks come first: [`check_raw_sizes`] bounds the raw bytes
//! before anything is decoded, [`decode_raw`] decodes only the block template and its id, and
//! [`DecodedBlock::prepare`] then does the expensive part (every transaction, then the slow hash).
//! A single relayed block has its slow hash computed and its proof of work checked on the template
//! first (`Chain::add_block`), then [`DecodedBlock::prepare_with_long_hash`] does the transactions.

use xkr_consensus::params::CRYPTONOTE_MAX_TX_SIZE;
use xkr_consensus::{BlockError, ConsensusError, validate_semantic_checked};
use xkr_crypto::{CheckedKeyImage, cn_fast_hash};
use xkr_serialization::{
    MAX_PARENT_BLOCK_SIZE, block_hash, block_long_hash, decode_block, decode_transaction,
    encode_transaction, transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_storage::VerifiedTx;
use xkr_types::{Block, Hash, RawBlock, Transaction, payment_id};

use crate::error::RejectReason;

/// When to compute the proof-of-work hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowMode {
    /// Always (default).
    Full,
    /// `--fast-sync`: not for blocks at or below this height (the last checkpoint).
    SkipUpTo(u64),
}

impl PowMode {
    /// Whether the slow hash is needed for a block at `height`.
    #[must_use]
    pub fn needed(self, height: u64) -> bool {
        match self {
            Self::Full => true,
            Self::SkipUpTo(last) => height > last,
        }
    }
}

/// A decoded, hashed, semantically valid transaction.
#[derive(Debug, Clone)]
pub struct PreparedTx {
    /// `cn_fast_hash(blob)`.
    pub hash: Hash,
    /// Ring-signature message.
    pub prefix_hash: Hash,
    /// Decoded transaction.
    pub tx: Transaction,
    /// Serialized transaction.
    pub blob: Vec<u8>,
    /// Length of the prefix inside `blob`.
    pub prefix_len: usize,
    /// Inputs minus outputs.
    pub fee: u64,
    /// The inputs' key images, checked by `validate_semantic`, in input order.
    pub key_images: Vec<CheckedKeyImage>,
    /// Payment id from extra.
    pub payment_id: Option<[u8; 32]>,
}

impl PreparedTx {
    /// Decodes and checks one transaction blob (`Core::extractTransactions` + `validateSemantic`).
    /// `index` is only used for the error.
    pub fn new(blob: Vec<u8>, index: usize) -> Result<Self, RejectReason> {
        if blob.len() > CRYPTONOTE_MAX_TX_SIZE {
            return Err(RejectReason::decode(format!(
                "transaction of {} bytes",
                blob.len()
            )));
        }
        let tx = decode_transaction(&blob).map_err(RejectReason::decode)?;
        let (fee, key_images) = validate_semantic_checked(&tx).map_err(|error| {
            RejectReason::Consensus(ConsensusError::Transaction {
                tx_index: Some(index),
                error,
            })
        })?;
        let hash = transaction_hash_from_blob(&blob);
        let prefix_hash = transaction_prefix_hash(&tx.prefix);
        let sig_bytes = tx.signature_section_len();
        let prefix_len = blob
            .len()
            .checked_sub(sig_bytes)
            .ok_or_else(|| RejectReason::decode("signatures longer than the transaction"))?;
        let payment_id = payment_id(&tx.prefix.extra).map(|h| h.0);
        Ok(Self {
            hash,
            prefix_hash,
            tx,
            blob,
            prefix_len,
            fee,
            key_images,
            payment_id,
        })
    }

    /// The storage form.
    #[must_use]
    pub fn into_verified(self) -> VerifiedTx {
        VerifiedTx {
            hash: self.hash,
            tx: self.tx,
            blob: self.blob,
            prefix_len: self.prefix_len,
            payment_id: self.payment_id,
        }
    }
}

/// A block after stage 1.
#[derive(Debug, Clone)]
pub struct PreparedBlock {
    /// Block id.
    pub hash: Hash,
    /// Decoded block.
    pub block: Block,
    /// The block blob as received.
    pub blob: Vec<u8>,
    /// Hash of the miner transaction.
    pub miner_tx_hash: Hash,
    /// Serialized size of the miner transaction.
    pub coinbase_size: u64,
    /// Proof-of-work hash, unless skipped by [`PowMode`].
    pub long_hash: Option<Hash>,
    /// Transactions in the order supplied.
    pub txs: Vec<PreparedTx>,
}

impl PreparedBlock {
    /// Height claimed by the coinbase.
    #[must_use]
    pub fn claimed_height(&self) -> u64 {
        u64::from(self.block.height())
    }

    /// Coinbase size plus every transaction blob (C++ `cumulativeBlockSize`).
    #[must_use]
    pub fn cumulative_size(&self) -> u64 {
        self.coinbase_size + self.txs.iter().map(|t| t.blob.len() as u64).sum::<u64>()
    }

    /// Sum of fees.
    #[must_use]
    pub fn fees(&self) -> u64 {
        self.txs.iter().fold(0u64, |acc, t| acc.wrapping_add(t.fee))
    }

    /// The raw block again (alternative-block storage).
    #[must_use]
    pub fn to_raw(&self) -> RawBlock {
        RawBlock {
            block: self.blob.clone(),
            transactions: self.txs.iter().map(|t| t.blob.clone()).collect(),
        }
    }
}

/// Bytes a block blob may take beyond the cumulative size it is charged with: the header
/// fields (major and minor version, timestamp, previous id, nonce: at most 56 bytes), the
/// merge-mining parent block (at most [`MAX_PARENT_BLOCK_SIZE`] bytes by consensus, plus up to
/// 7 bytes of a non-minimal transaction-count varint) and the
/// transaction-hash count. The transaction hashes themselves (32 bytes each) are covered by the
/// transactions' own sizes: every valid transaction is longer than 32 bytes.
pub const MAX_BLOCK_BLOB_OVERHEAD: u64 = 2 * MAX_PARENT_BLOCK_SIZE as u64;

/// The consensus size rule applied to the raw bytes before anything is decoded.
///
/// C++ `Core::addBlock` refuses a block whose cumulative size (miner transaction plus every
/// transaction blob) exceeds `max_block_cumulative_size` (`BLOCK_TOO_BIG`). So the transaction
/// blobs together, and the block blob minus [`MAX_BLOCK_BLOB_OVERHEAD`], cannot exceed that
/// limit in any block the network accepts; `max_cumulative_size` must be the limit of the
/// highest height the caller could accept (the limit grows with the height).
pub fn check_raw_sizes(raw: &RawBlock, max_cumulative_size: u64) -> Result<(), RejectReason> {
    let txs = raw
        .transactions
        .iter()
        .fold(0u64, |acc, t| acc.saturating_add(t.len() as u64));
    let block = raw.block.len() as u64;
    if txs > max_cumulative_size
        || block > max_cumulative_size.saturating_add(MAX_BLOCK_BLOB_OVERHEAD)
    {
        return Err(RejectReason::Consensus(ConsensusError::Block(
            BlockError::CumulativeBlockSizeTooBig {
                size: block.saturating_add(txs),
                limit: max_cumulative_size,
            },
        )));
    }
    Ok(())
}

/// A raw block whose template decoded (the first half of stage 1): its id and header are
/// known, its transactions are still raw and no slow hash was computed.
#[derive(Debug, Clone)]
pub struct DecodedBlock {
    /// Block id.
    pub hash: Hash,
    /// Decoded block template.
    pub block: Block,
    blob: Vec<u8>,
    transactions: Vec<Vec<u8>>,
}

/// Decodes the block template of `raw` and computes its id; checks that it carries as many
/// transactions as it lists. Cheap: no transaction is decoded and no slow hash computed.
pub fn decode_raw(raw: RawBlock) -> Result<DecodedBlock, PrepareError> {
    let RawBlock {
        block: blob,
        transactions,
    } = raw;
    let block = decode_block(&blob).map_err(|e| PrepareError {
        hash: None,
        reason: RejectReason::decode(e),
    })?;
    let hash = block_hash(&block).map_err(|e| PrepareError {
        hash: None,
        reason: RejectReason::decode(e),
    })?;
    if transactions.len() != block.tx_hashes.len() {
        return Err(PrepareError {
            hash: Some(hash),
            reason: RejectReason::TxCountMismatch {
                hashes: block.tx_hashes.len(),
                txs: transactions.len(),
            },
        });
    }
    Ok(DecodedBlock {
        hash,
        block,
        blob,
        transactions,
    })
}

impl DecodedBlock {
    /// The second half of stage 1: the miner transaction hash, every transaction (decode,
    /// `validateSemantic`) and then, if `pow` needs it, the slow hash.
    pub fn prepare(self, pow: PowMode) -> Result<PreparedBlock, PrepareError> {
        let compute = pow.needed(u64::from(self.block.height()));
        self.finish(compute, None)
    }

    /// [`Self::prepare`] for a block whose slow hash was already computed (and its proof of
    /// work checked) from the template: only the miner transaction hash and the transactions.
    pub fn prepare_with_long_hash(self, long_hash: Hash) -> Result<PreparedBlock, PrepareError> {
        self.finish(false, Some(long_hash))
    }

    /// Transactions, then the slow hash if `compute` (else `known`).
    fn finish(self, compute: bool, known: Option<Hash>) -> Result<PreparedBlock, PrepareError> {
        let Self {
            hash,
            block,
            blob,
            transactions,
        } = self;
        let err = |reason| PrepareError {
            hash: Some(hash),
            reason,
        };
        let coinbase =
            encode_transaction(&block.miner_tx).map_err(|e| err(RejectReason::decode(e)))?;
        let miner_tx_hash = cn_fast_hash(&coinbase);
        let mut txs = Vec::with_capacity(transactions.len());
        for (i, t) in transactions.into_iter().enumerate() {
            txs.push(PreparedTx::new(t, i).map_err(err)?);
        }
        let long_hash = if compute {
            // C++ maps any failure of getBlockLongHash to "proof of work too weak".
            Some(block_long_hash(&block).map_err(|_| {
                err(RejectReason::Consensus(ConsensusError::Block(
                    BlockError::ProofOfWorkTooWeak,
                )))
            })?)
        } else {
            known
        };
        Ok(PreparedBlock {
            hash,
            block,
            blob,
            miner_tx_hash,
            coinbase_size: coinbase.len() as u64,
            long_hash,
            txs,
        })
    }
}

/// A block that failed stage 1.
#[derive(Debug, Clone)]
pub struct PrepareError {
    /// Block id, if the block decoded.
    pub hash: Option<Hash>,
    /// Why.
    pub reason: RejectReason,
}

/// Stage 1 for one raw block ([`decode_raw`] then [`DecodedBlock::prepare`]).
pub fn prepare_block(raw: RawBlock, pow: PowMode) -> Result<PreparedBlock, PrepareError> {
    decode_raw(raw)?.prepare(pow)
}
