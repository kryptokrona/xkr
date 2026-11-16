//! Block templates and mined blocks: C++ `Core::getBlockTemplate`, `Core::fillBlockTemplate`,
//! `Currency::constructMinerTx` and the pool half of `Core::submitBlock`.
//!
//! A template is built from **one** storage snapshot (height, previous hash, difficulty,
//! already generated coins, size median and the timestamp window agree even if a block arrives
//! meanwhile) plus the pool. It is byte-compatible with what the C++ daemon returns from
//! `getblocktemplate`:
//!
//! * major version of the next height from the fork table (6 after fork 13). The minor version
//!   differs on purpose: C++ writes 0 (both of its branches yield 0 because every upgrade height
//!   is defined), we write the supported fork index of the schedule (13, or 14 once fork 14 is
//!   scheduled) to signal fork readiness (`ForkSchedule::template_minor_version`,
//!   `docs/forks.md`). Consensus ignores the minor version, and p2pool already writes 16;
//! * for major >= 2 a merge-mining parent block with `transaction_count = 1`, empty branches
//!   and a parent miner transaction whose extra is a zero merge-mining tag
//!   (`03 21 00 00..00`). C++ sets the parent major version to `BLOCK_MINOR_VERSION_0` = 0 (it
//!   assigns `majorVersion` twice), so we do too. Miners must replace the tag's merkle root by
//!   the block's auxiliary header hash before hashing (C++ `MinerManager::adjustMergeMiningTag`);
//! * timestamp = now, raised to the median of the last 11 (60 below height 620 000)
//!   timestamps;
//! * the miner transaction: tx key `R` in extra, then the extra nonce (`02 len bytes`, the
//!   `reserve_size` zeros of `getblocktemplate`), reward split into at most 11 denominations,
//!   unlock time `height + CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW`, grown with zero padding until
//!   its size matches the size the reward was computed for (the two-phase loop of C++);
//! * after fork 14 (`docs/tx-v2.md`) the miner transaction is version 2 with tagged outputs
//!   (`view_tag(8rA, i)`), and pool transactions whose version the next block does not allow
//!   (version 1 after the grace period) are left out.

use xkr_consensus::params::{
    BLOCK_MINOR_VERSION_0, BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW, BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3,
    CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE, FUSION_TX_MAX_SIZE, LWMA_2_DIFFICULTY_BLOCK_INDEX,
};
use xkr_consensus::reward::{
    decompose_amount, default_dust_threshold, max_block_cumulative_size, median,
};
use xkr_consensus::{
    block_major_version_for_height, check_transaction_version, difficulty_for_next_block,
    get_block_reward,
};
use xkr_crypto::{derive_public_key, generate_key_derivation, generate_keys, os_rng, view_tag};
use xkr_serialization::{
    EncodeError, TRANSACTION_VERSION_2, aux_block_header_hash, decode_block, encode_block,
    encode_transaction,
};
use xkr_types::extra::{
    TX_EXTRA_MERGE_MINING_TAG, TX_EXTRA_NONCE, TX_EXTRA_NONCE_MAX_COUNT, TX_EXTRA_TAG_PUBKEY,
};
use xkr_types::{
    Block, BlockHeader, Hash, ParentBlock, PublicKey, RawBlock, SecretKey, Transaction,
    TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

use xkr_txpool::{PoolTxSummary, TemplateSelection};

use crate::chain::{Chain, unix_now};
use crate::error::ChainError;
use crate::headers::context_from_storage;

/// `maxOuts` of the `constructMinerTx` calls in `getBlockTemplate`.
const MINER_TX_MAX_OUTS: usize = 11;
/// `TRIES_COUNT` of `getBlockTemplate`.
const TEMPLATE_TRIES: usize = 10;

/// Where the block reward goes (the two public keys of an address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinerAddress {
    /// Public spend key.
    pub spend_public: PublicKey,
    /// Public view key.
    pub view_public: PublicKey,
}

/// A block template.
#[derive(Debug, Clone)]
pub struct BlockTemplate {
    /// The block (nonce 0).
    pub block: Block,
    /// `toBinaryArray(block)` (`blocktemplate_blob`).
    pub blob: Vec<u8>,
    /// Difficulty the block must meet.
    pub difficulty: u64,
    /// Height (block index) of the new block = top index + 1.
    pub height: u64,
    /// Offset of the extra nonce bytes in [`Self::blob`] (`reserved_offset`; 0 without a nonce).
    pub reserved_offset: usize,
    /// Blobs of the selected transactions, in `tx_hashes` order.
    pub transactions: Vec<Vec<u8>>,
    /// Secret key of the miner transaction (proves the payout; not needed to mine).
    pub tx_secret_key: SecretKey,
    /// Main-chain top the template builds on.
    pub prev_hash: Hash,
    /// Block reward (sum of the miner transaction outputs).
    pub reward: u64,
}

/// Why no template could be built.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    /// Storage failure.
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// `getDifficultyForNextBlock() == 0` ("difficulty overhead").
    #[error("difficulty overhead")]
    DifficultyOverhead,
    /// The extra nonce is longer than `TX_EXTRA_NONCE_MAX_COUNT` (255).
    #[error("extra nonce of {0} bytes, maximum 255")]
    NonceTooLarge(usize),
    /// `getBlockReward` refused the size ("Block is too big").
    #[error("block is too big for the reward function")]
    BlockTooBig,
    /// A miner output key could not be derived (invalid address key).
    #[error("invalid miner address key")]
    InvalidAddress,
    /// The miner transaction size did not converge (C++ `Failed to create_block_template`).
    #[error("miner transaction size did not converge: {0}")]
    SizeMismatch(String),
    /// Serialization failed.
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Why a mined block could not be assembled from the pool (`Core::submitBlock`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The blob is not a block (`DESERIALIZATION_FAILED`).
    #[error("block does not decode: {0}")]
    Decode(String),
    /// A listed transaction is not in the pool (`TRANSACTION_ABSENT_IN_POOL`).
    #[error("transaction {0} is not in the pool")]
    TransactionAbsent(Hash),
}

/// C++ `addTransactionPublicKeyToExtra` + `addExtraNonceToTransactionExtra`.
fn miner_extra(tx_public_key: &PublicKey, extra_nonce: &[u8]) -> Vec<u8> {
    let mut extra = Vec::with_capacity(35 + extra_nonce.len());
    extra.push(TX_EXTRA_TAG_PUBKEY);
    extra.extend_from_slice(&tx_public_key.0);
    if !extra_nonce.is_empty() {
        extra.push(TX_EXTRA_NONCE);
        extra.push(extra_nonce.len() as u8);
        extra.extend_from_slice(extra_nonce);
    }
    extra
}

/// The parameters of `Currency::constructMinerTx` that do not change between the tries.
#[derive(Debug, Clone, Copy)]
pub struct MinerTxParams<'a> {
    /// Block major version.
    pub major_version: u8,
    /// Height of the block.
    pub height: u64,
    /// Effective size median (`calculateCumulativeBlocksizeLimit(height) / 2`).
    pub median_size: u64,
    /// Coins generated before this block.
    pub already_generated_coins: u64,
    /// Sum of the transaction fees.
    pub fee: u64,
    /// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` of the network.
    pub unlock_window: u64,
    /// Recipient.
    pub miner: &'a MinerAddress,
    /// Transaction key pair `(R, r)`.
    pub tx_key: (&'a PublicKey, &'a SecretKey),
    /// Extra nonce (pool reserve).
    pub extra_nonce: &'a [u8],
    /// `maxOuts`.
    pub max_outs: usize,
    /// Transaction version: 1 (`CURRENT_TRANSACTION_VERSION`) before fork 14, 2 after
    /// (`ForkSchedule::coinbase_tx_version`; version 2 has tagged outputs).
    pub tx_version: u8,
}

/// C++ `Currency::constructMinerTx` for a block of cumulative size `current_block_size`.
pub fn construct_miner_tx(
    p: &MinerTxParams<'_>,
    current_block_size: u64,
) -> Result<Transaction, TemplateError> {
    if p.extra_nonce.len() > TX_EXTRA_NONCE_MAX_COUNT {
        return Err(TemplateError::NonceTooLarge(p.extra_nonce.len()));
    }
    let (reward, _) = get_block_reward(
        p.major_version,
        p.median_size,
        current_block_size,
        p.already_generated_coins,
        p.fee,
    )
    .map_err(|_| TemplateError::BlockTooBig)?;
    let h32 = u32::try_from(p.height).unwrap_or(u32::MAX);
    let mut amounts = decompose_amount(reward, default_dust_threshold(h32));
    let max_outs = p.max_outs.max(1);
    while amounts.len() > max_outs {
        let last = amounts.pop().unwrap_or(0);
        let n = amounts.len();
        amounts[n - 1] += last;
    }
    let derivation = generate_key_derivation(&p.miner.view_public, p.tx_key.1)
        .ok_or(TemplateError::InvalidAddress)?;
    let mut outputs = Vec::with_capacity(amounts.len());
    for (no, amount) in amounts.iter().enumerate() {
        let key = derive_public_key(&derivation, no as u64, &p.miner.spend_public)
            .ok_or(TemplateError::InvalidAddress)?;
        let target = if p.tx_version >= TRANSACTION_VERSION_2 {
            TxOutputTarget::TaggedKey {
                key,
                view_tag: view_tag(&derivation, no as u64),
            }
        } else {
            TxOutputTarget::Key(key)
        };
        outputs.push(TxOutput {
            amount: *amount,
            target,
        });
    }
    if amounts.iter().sum::<u64>() != reward {
        return Err(TemplateError::SizeMismatch(
            "output amounts do not add up to the reward".into(),
        ));
    }
    Ok(Transaction {
        prefix: TransactionPrefix {
            version: p.tx_version,
            unlock_time: p.height + p.unlock_window,
            inputs: vec![TxInput::Coinbase { height: h32 }],
            outputs,
            extra: miner_extra(p.tx_key.0, p.extra_nonce),
        },
        signatures: Vec::new(),
        clsag_signatures: Vec::new(),
    })
}

fn tx_size(tx: &Transaction) -> Result<u64, TemplateError> {
    Ok(encode_transaction(tx)?.len() as u64)
}

/// The two-phase miner transaction of `Core::getBlockTemplate`: the reward depends on the
/// block size, which depends on the miner transaction. Returns a miner transaction whose size
/// plus `transactions_size` is exactly the size its reward was computed for.
pub fn miner_tx_for_template(
    p: &MinerTxParams<'_>,
    transactions_size: u64,
) -> Result<Transaction, TemplateError> {
    // "make blocks coin-base tx looks close to real coinbase tx to get truthful blob size"
    let first = construct_miner_tx(p, transactions_size)?;
    let mut cumulative = transactions_size + tx_size(&first)?;
    for _ in 0..TEMPLATE_TRIES {
        let mut tx = construct_miner_tx(p, cumulative)?;
        let coinbase = tx_size(&tx)?;
        let room = cumulative - transactions_size;
        if coinbase > room {
            cumulative = transactions_size + coinbase;
            continue;
        }
        if coinbase < room {
            let delta = room - coinbase;
            tx.prefix
                .extra
                .extend(std::iter::repeat_n(0u8, delta as usize));
            // The extra length varint may have grown by one byte.
            if cumulative != transactions_size + tx_size(&tx)? {
                if cumulative + 1 != transactions_size + tx_size(&tx)? {
                    return Err(TemplateError::SizeMismatch(format!(
                        "cumulative size {cumulative} after padding by {delta}"
                    )));
                }
                tx.prefix.extra.pop();
                if cumulative != transactions_size + tx_size(&tx)? {
                    // -1 made the varint shorter again: grow the target instead.
                    cumulative += delta - 1;
                    continue;
                }
            }
        }
        if cumulative != transactions_size + tx_size(&tx)? {
            return Err(TemplateError::SizeMismatch(format!(
                "cumulative size {cumulative}"
            )));
        }
        return Ok(tx);
    }
    Err(TemplateError::SizeMismatch(format!(
        "no fit after {TEMPLATE_TRIES} tries"
    )))
}

/// The zero merge-mining tag C++ puts into the parent miner transaction of a template:
/// `TX_EXTRA_MERGE_MINING_TAG || varint(33) || varint(depth) || merkle_root`.
#[must_use]
pub fn merge_mining_tag_extra(depth: u8, merkle_root: &Hash) -> Vec<u8> {
    let mut extra = Vec::with_capacity(35);
    extra.push(TX_EXTRA_MERGE_MINING_TAG);
    extra.push(33);
    extra.push(depth);
    extra.extend_from_slice(&merkle_root.0);
    extra
}

/// C++ `MinerManager::adjustMergeMiningTag`: for major version >= 2, makes the parent miner
/// transaction's extra a depth-0 merge-mining tag committing to the block's auxiliary header
/// hash (which covers neither the timestamp nor the nonce, so it is computed once per
/// template). Without this the proof of work check fails for every nonce.
pub fn set_merge_mining_tag(block: &mut Block) -> Result<(), EncodeError> {
    if block.header.major_version < 2 {
        return Ok(());
    }
    let aux = aux_block_header_hash(block)?;
    if let Some(pb) = block.parent_block.as_mut() {
        pb.base_transaction.prefix.extra = merge_mining_tag_extra(0, &aux);
    }
    Ok(())
}

/// C++ `on_getblocktemplate`: offset of the reserved nonce bytes in the block blob = first
/// occurrence of the tx public key + 32 + 3 (pubkey tag, nonce tag, nonce length). `None` if the
/// key is not found or the reserve would run past the blob.
///
/// Note the C++ quirk, reproduced on purpose for pool compatibility: the pubkey *tag* precedes
/// the key, so counting it after the key overshoots by one byte and the reported offset points
/// at the second byte of the nonce (Monero adds 2 here). Pools and p2pool must be checked for
/// this when they move to xkrd; see `docs/mining.md`.
#[must_use]
pub fn reserved_offset(
    blob: &[u8],
    tx_public_key: &PublicKey,
    reserve_size: usize,
) -> Option<usize> {
    let pos = blob.windows(32).position(|w| w == tx_public_key.0)?;
    let offset = pos + 32 + 3;
    (offset + reserve_size <= blob.len()).then_some(offset)
}

impl Chain {
    /// C++ `fillBlockTemplate`: the pool transactions the next block at `height` would carry,
    /// in priority order (fusion first, then fee per byte, smaller blob, older), under the size
    /// limit derived from `median_size` and the transaction versions the fork table allows.
    #[must_use]
    pub fn select_template_transactions(&self, height: u64, median_size: u64) -> TemplateSelection {
        let params = self.params();
        let max_cumulative = max_block_cumulative_size(height);
        let max_total = ((125 * median_size) / 100)
            .min(max_cumulative)
            .saturating_sub(CRYPTONOTE_COINBASE_BLOB_RESERVED_SIZE as u64);
        let forks = params.forks();
        self.pool().fill_block_template_with(
            height,
            usize::try_from(median_size).unwrap_or(usize::MAX),
            usize::try_from(max_total).unwrap_or(usize::MAX),
            FUSION_TX_MAX_SIZE,
            |tx| check_transaction_version(&forks, height, tx).is_ok(),
        )
    }

    /// The `tx_backlog` of `get_miner_data`: exactly the transactions
    /// [`Self::select_template_transactions`] would put into the next block, in that order, so
    /// p2pool never builds a block the node would refuse (Hugin messages, wrong mixin or
    /// version) and sees a byte-identical answer while the pool is unchanged.
    pub fn template_backlog(&self) -> Result<Vec<PoolTxSummary>, ChainError> {
        let reader = self.reader()?;
        let height = self.tip().height + 1;
        let median_size = self.block_median_size(&reader)?;
        drop(reader);
        Ok(self
            .select_template_transactions(height, median_size)
            .txs
            .iter()
            .map(|e| PoolTxSummary {
                hash: e.hash,
                size: e.size(),
                fee: e.fee,
                received: e.received,
            })
            .collect())
    }

    /// C++ `Core::getBlockTemplate(b, address, extraNonce, difficulty, height)`.
    pub fn block_template(
        &self,
        miner: &MinerAddress,
        extra_nonce: &[u8],
    ) -> Result<BlockTemplate, TemplateError> {
        if extra_nonce.len() > TX_EXTRA_NONCE_MAX_COUNT {
            return Err(TemplateError::NonceTooLarge(extra_nonce.len()));
        }
        let params = self.params();
        let reader = self.reader()?;
        let top = reader.top_height().map_err(ChainError::from)?.unwrap_or(0);
        let height = top + 1;
        let h32 = u32::try_from(height).unwrap_or(u32::MAX);
        let prev_hash = reader
            .block_hash(top)
            .map_err(ChainError::from)?
            .ok_or_else(|| ChainError::Corrupt(format!("no block hash at {top}")))?;
        let top_info = reader
            .block_info(top)
            .map_err(ChainError::from)?
            .ok_or_else(|| ChainError::Corrupt(format!("no block info at {top}")))?;

        let ctx = context_from_storage(params, &reader, height)?;
        let difficulty = difficulty_for_next_block(
            params,
            h32,
            &ctx.difficulty_timestamps,
            &ctx.difficulty_cumulative,
        );
        if difficulty == 0 {
            return Err(TemplateError::DifficultyOverhead);
        }
        let major_version = block_major_version_for_height(params, h32);

        // Timestamp: now, but never below the median of the check window.
        let mut timestamp = unix_now();
        let window = if height >= LWMA_2_DIFFICULTY_BLOCK_INDEX {
            BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW_V3 as u64
        } else {
            BLOCKCHAIN_TIMESTAMP_CHECK_WINDOW as u64
        };
        if height >= window {
            let infos = reader
                .block_infos(height - window, height)
                .map_err(ChainError::from)?;
            let stamps: Vec<u64> = infos.iter().map(|i| i.timestamp).collect();
            timestamp = timestamp.max(median(&stamps));
        }

        // calculateCumulativeBlocksizeLimit(height) / 2 = max(median of 100 sizes, full reward zone)
        let median_size = self.block_median_size(&reader)?;
        let already_generated_coins = top_info.already_generated_coins;
        drop(reader);

        // fillBlockTemplate
        let forks = params.forks();
        let selection = self.select_template_transactions(height, median_size);
        let transactions_size = selection.size as u64;

        let (tx_public_key, tx_secret_key) = generate_keys(&mut os_rng());
        let p = MinerTxParams {
            major_version,
            height,
            median_size,
            already_generated_coins,
            fee: selection.fee,
            unlock_window: u64::from(params.mined_money_unlock_window),
            miner,
            tx_key: (&tx_public_key, &tx_secret_key),
            extra_nonce,
            max_outs: MINER_TX_MAX_OUTS,
            tx_version: forks.coinbase_tx_version(height),
        };
        let miner_tx = miner_tx_for_template(&p, transactions_size)?;
        let reward = miner_tx.prefix.outputs.iter().map(|o| o.amount).sum();

        let parent_block = (major_version >= 2).then(|| {
            let mut base = Transaction::default();
            base.prefix.extra = merge_mining_tag_extra(0, &Hash::ZERO);
            ParentBlock {
                // C++ assigns BLOCK_MINOR_VERSION_0 to parentBlock.majorVersion (sic).
                major_version: BLOCK_MINOR_VERSION_0,
                minor_version: BLOCK_MINOR_VERSION_0,
                prev_hash: Hash::ZERO,
                transaction_count: 1,
                base_transaction_branch: Vec::new(),
                base_transaction: base,
                blockchain_branch: Vec::new(),
            }
        });
        let block = Block {
            header: BlockHeader {
                major_version,
                minor_version: forks.template_minor_version(major_version),
                timestamp,
                prev_hash,
                nonce: 0,
            },
            parent_block,
            miner_tx,
            tx_hashes: selection.txs.iter().map(|e| e.hash).collect(),
        };
        let blob = encode_block(&block)?;
        let offset = if extra_nonce.is_empty() {
            0
        } else {
            reserved_offset(&blob, &tx_public_key, extra_nonce.len()).ok_or_else(|| {
                TemplateError::SizeMismatch("tx public key not found in the block blob".into())
            })?
        };
        Ok(BlockTemplate {
            block,
            blob,
            difficulty,
            height,
            reserved_offset: offset,
            transactions: selection.txs.iter().map(|e| e.blob.to_vec()).collect(),
            tx_secret_key,
            prev_hash,
            reward,
        })
    }

    /// The transactions half of C++ `Core::submitBlock`: decodes a mined block blob and attaches
    /// the listed transactions from the pool. Add the result with [`Chain::add_block`].
    pub fn assemble_mined_block(&self, blob: Vec<u8>) -> Result<RawBlock, SubmitError> {
        let block = decode_block(&blob).map_err(|e| SubmitError::Decode(e.to_string()))?;
        let mut transactions = Vec::with_capacity(block.tx_hashes.len());
        for h in &block.tx_hashes {
            transactions.push(
                self.pool()
                    .blob(h)
                    .ok_or(SubmitError::TransactionAbsent(*h))?,
            );
        }
        Ok(RawBlock {
            block: blob,
            transactions,
        })
    }
}
