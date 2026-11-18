//! Read functions over one MVCC snapshot.

use redb::{ReadOnlyTable, ReadTransaction, ReadableTableMetadata};
use xkr_types::{Hash, KeyImage, PublicKey, RawBlock};

use super::chunk::{ChunkCache, decode_body, read_part_range, read_parts};
use super::decode_raw_block;
use crate::{
    Error, Result,
    codec::{BlockRecord, unstrip_output_keys},
    compress::Compressor,
    pruning::{PruneState, PruningSeed, state_from_meta},
    schema::{
        ALT_BLOCKS, AltBlockHeader, BLOCK_HASH_TO_HEIGHT, BLOCK_INFOS, BODY_PARTS, BlockInfo,
        BlockInfoRow, CHUNK_BLOCKS, HOT_BLOCKS, HOT_SIGS, KEY_IMAGES, META, OUTPUT_COUNTS, OUTPUTS,
        OutputInfo, OutputRow, PAYMENT_IDS, SIG_PARTS, TIMESTAMPS, TX_HASH_TO_LOC, TxLoc, TxLocRow,
        UNLOCK_ESCAPE, UNLOCK_OVERFLOW, clamp32, meta, part_key,
    },
};

/// One row of [`Reader::global_indexes_for_range`]: `((height, tx_index), global output indexes)`.
pub type TxGlobalIndexes = ((u64, u32), Vec<u32>);

/// What every node keeps of a block, also a pruned one ([`Reader::block_prefixes`]): the block
/// blob, the transaction prefixes and all global output indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockPrefixes {
    /// The block blob (header, miner transaction, transaction hashes), as on the wire.
    pub block: Vec<u8>,
    /// Global output indexes of the miner transaction's outputs.
    pub miner_global_indexes: Vec<u32>,
    /// Per transaction in `tx_hashes` order: its prefix blob and its global output indexes.
    pub txs: Vec<(Vec<u8>, Vec<u32>)>,
}

/// Read-only views of every table, opened once from a [`ReadTransaction`]. Cheap to keep
/// for the duration of one RPC request or one verification batch; it sees a consistent
/// snapshot regardless of concurrent writers.
pub struct Reader {
    block_infos: ReadOnlyTable<u32, BlockInfoRow>,
    block_hash_to_height: ReadOnlyTable<[u8; 32], u32>,
    hot_blocks: ReadOnlyTable<u32, &'static [u8]>,
    hot_sigs: ReadOnlyTable<u32, &'static [u8]>,
    body_parts: ReadOnlyTable<u64, &'static [u8]>,
    sig_parts: ReadOnlyTable<u64, &'static [u8]>,
    tx_hash_to_loc: ReadOnlyTable<[u8; 32], TxLocRow>,
    key_images: ReadOnlyTable<[u8; 32], ()>,
    outputs: ReadOnlyTable<(u64, u32), OutputRow>,
    unlock_overflow: ReadOnlyTable<(u64, u32), u64>,
    output_counts: ReadOnlyTable<u64, u64>,
    payment_ids: ReadOnlyTable<([u8; 32], u32, u32), ()>,
    timestamps: ReadOnlyTable<(u64, u32), ()>,
    alt_blocks: ReadOnlyTable<[u8; 32], &'static [u8]>,
    top: Option<u64>,
    sealed_below: u64,
    prune: PruneState,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("top", &self.top)
            .field("sealed_below", &self.sealed_below)
            .field("prune", &self.prune)
            .finish_non_exhaustive()
    }
}

/// Where the signatures of a block are: the hot row, or an offset in its chunk's stream.
#[derive(Clone, Copy)]
enum SigSource {
    Hot(u32),
    Chunk { chunk: u64, base: u64 },
}

impl Reader {
    /// Opens all tables. The tables exist once [`crate::Store::open`] has run.
    pub fn new(txn: &ReadTransaction) -> Result<Self> {
        let meta = txn.open_table(META)?;
        let top = meta.get(meta::TOP_HEIGHT)?.map(|g| g.value());
        let sealed_below = meta.get(meta::SEALED_BELOW)?.map_or(0, |g| g.value());
        let prune = state_from_meta(
            meta.get(meta::PRUNING_SEED)?.map(|g| g.value()),
            meta.get(meta::PRUNED_BELOW)?.map_or(0, |g| g.value()),
        )?;
        Ok(Self {
            block_infos: txn.open_table(BLOCK_INFOS)?,
            block_hash_to_height: txn.open_table(BLOCK_HASH_TO_HEIGHT)?,
            hot_blocks: txn.open_table(HOT_BLOCKS)?,
            hot_sigs: txn.open_table(HOT_SIGS)?,
            body_parts: txn.open_table(BODY_PARTS)?,
            sig_parts: txn.open_table(SIG_PARTS)?,
            tx_hash_to_loc: txn.open_table(TX_HASH_TO_LOC)?,
            key_images: txn.open_table(KEY_IMAGES)?,
            outputs: txn.open_table(OUTPUTS)?,
            unlock_overflow: txn.open_table(UNLOCK_OVERFLOW)?,
            output_counts: txn.open_table(OUTPUT_COUNTS)?,
            payment_ids: txn.open_table(PAYMENT_IDS)?,
            timestamps: txn.open_table(TIMESTAMPS)?,
            alt_blocks: txn.open_table(ALT_BLOCKS)?,
            top,
            sealed_below,
            prune,
        })
    }

    // ------------------------------------------------------------------ chain shape

    /// Height of the top block, `None` for an empty chain.
    pub fn top_height(&self) -> Result<Option<u64>> {
        Ok(self.top)
    }

    /// Signatures below this height may be pruned: all of them without a stripe seed, the
    /// other stripes' with one ([`Self::prune_state`]). 0 = nothing pruned.
    #[must_use]
    pub const fn pruned_below(&self) -> u64 {
        self.prune.pruned_below
    }

    /// The stripe this node keeps below [`Self::pruned_below`]: [`PruningSeed::FULL`] on a full
    /// node, [`PruningSeed::RECENT_ONLY`] on a database pruned completely
    /// (`ops::prune_all_below`; such a node holds no old signatures and announces that seed,
    /// never the full one). [`Self::has_full_block`] answers per height for every kind.
    #[must_use]
    pub const fn pruning_seed(&self) -> PruningSeed {
        self.prune.seed
    }

    /// Seed and prune height together.
    #[must_use]
    pub const fn prune_state(&self) -> PruneState {
        self.prune
    }

    /// Whether the main-chain block at `height` is stored with its signatures, so it can be
    /// served whole (P2P `GET_OBJECTS`, `/getrawblocks`) and popped. False above the tip and
    /// for blocks whose signatures were pruned (even a block without transactions there: the
    /// answer follows the stripe rule, not the block's content).
    #[must_use]
    pub fn has_full_block(&self, height: u64) -> bool {
        self.top.is_some_and(|top| height <= top) && !self.prune.dropped(height)
    }

    /// Whether sealed chunk `chunk` has signature parts (`xkrd db verify`: dropped chunks
    /// must have none, kept chunks with transactions must have them).
    pub fn has_signature_parts(&self, chunk: u64) -> Result<bool> {
        Ok(self
            .sig_parts
            .range(part_key(chunk, 0)..part_key(chunk + 1, 0))?
            .next()
            .is_some())
    }

    /// Blocks below this height are sealed into compressed chunks.
    #[must_use]
    pub const fn sealed_below(&self) -> u64 {
        self.sealed_below
    }

    pub fn block_info(&self, height: u64) -> Result<Option<BlockInfo>> {
        let Ok(h) = u32::try_from(height) else {
            return Ok(None);
        };
        Ok(self
            .block_infos
            .get(h)?
            .map(|g| g.value().to_info(self.prune.dropped(height))))
    }

    /// `BlockInfo`s for `start..end` (clamped to the chain), in height order.
    pub fn block_infos(&self, start: u64, end: u64) -> Result<Vec<BlockInfo>> {
        let mut out = Vec::with_capacity(end.saturating_sub(start).min(100_000) as usize);
        for row in self.block_infos.range(clamp32(start)..clamp32(end))? {
            let (k, v) = row?;
            out.push(v.value().to_info(self.prune.dropped(u64::from(k.value()))));
        }
        Ok(out)
    }

    pub fn block_hash(&self, height: u64) -> Result<Option<Hash>> {
        Ok(self.block_info(height)?.map(|i| Hash(i.hash)))
    }

    pub fn height_of(&self, hash: &Hash) -> Result<Option<u64>> {
        Ok(self
            .block_hash_to_height
            .get(hash.0)?
            .map(|g| u64::from(g.value())))
    }

    /// `count` consecutive main-chain hashes from `start` (`Core::getBlockHashes`).
    pub fn block_hashes(&self, start: u64, count: u64) -> Result<Vec<Hash>> {
        Ok(self
            .block_infos(start, start.saturating_add(count))?
            .into_iter()
            .map(|i| Hash(i.hash))
            .collect())
    }

    // ------------------------------------------------------------------ consensus windows

    /// Heights of the window of (at most) `count` blocks ending at `top` inclusive.
    fn window(top: u64, count: usize) -> (u64, u64) {
        let end = top + 1;
        (end.saturating_sub(count as u64), end)
    }

    /// Timestamps of the last `count` blocks up to `top`, oldest first.
    pub fn timestamps_window(&self, top: u64, count: usize) -> Result<Vec<u64>> {
        let (s, e) = Self::window(top, count);
        Ok(self
            .block_infos(s, e)?
            .iter()
            .map(|i| i.timestamp)
            .collect())
    }

    /// Cumulative difficulties of the last `count` blocks up to `top`, oldest first.
    pub fn cumulative_difficulties_window(&self, top: u64, count: usize) -> Result<Vec<u64>> {
        let (s, e) = Self::window(top, count);
        Ok(self
            .block_infos(s, e)?
            .iter()
            .map(|i| u64::try_from(i.cumulative_difficulty()).unwrap_or(u64::MAX))
            .collect())
    }

    /// Both difficulty inputs with a single scan: `(timestamps, cumulative_difficulties)`.
    pub fn difficulty_window(&self, top: u64, count: usize) -> Result<(Vec<u64>, Vec<u64>)> {
        let (s, e) = Self::window(top, count);
        let infos = self.block_infos(s, e)?;
        Ok((
            infos.iter().map(|i| i.timestamp).collect(),
            infos
                .iter()
                .map(|i| u64::try_from(i.cumulative_difficulty()).unwrap_or(u64::MAX))
                .collect(),
        ))
    }

    /// Block sizes (CryptoNote cumulative size) of the last `count` blocks up to `top`,
    /// oldest first, for the median used by the reward penalty.
    pub fn median_block_size_window(&self, top: u64, count: usize) -> Result<Vec<u64>> {
        let (s, e) = Self::window(top, count);
        Ok(self
            .block_infos(s, e)?
            .iter()
            .map(|i| i.block_size)
            .collect())
    }

    // ------------------------------------------------------------------ outputs / key images

    /// Number of outputs ever created with `amount` (next global index).
    pub fn output_count(&self, amount: u64) -> Result<u64> {
        Ok(self.output_counts.get(amount)?.map_or(0, |g| g.value()))
    }

    /// Every `(amount, output count)`, by amount.
    pub fn output_counts(&self) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        for row in self.output_counts.range(0_u64..)? {
            let (k, v) = row?;
            out.push((k.value(), v.value()));
        }
        Ok(out)
    }

    fn output_row(&self, amount: u64, index: u64) -> Result<Option<OutputInfo>> {
        let Ok(gi) = u32::try_from(index) else {
            return Ok(None);
        };
        let Some(row) = self.outputs.get((amount, gi))?.map(|g| g.value()) else {
            return Ok(None);
        };
        let unlock_time = if row.unlock == UNLOCK_ESCAPE {
            self.unlock_overflow
                .get((amount, gi))?
                .map(|g| g.value())
                .ok_or(Error::Corrupt("escaped unlock time without overflow row"))?
        } else {
            u64::from(row.unlock)
        };
        Ok(Some(OutputInfo {
            key: row.key,
            unlock_time,
            height: u64::from(row.height),
        }))
    }

    /// `(key, unlock_time, height)` for each global index, in the given order. Errors if any
    /// index does not exist. This is a plain lookup: ring members of a transaction input are
    /// resolved with [`Self::ring_keys`], which applies the consensus order.
    pub fn output_keys(
        &self,
        amount: u64,
        global_indexes: &[u64],
    ) -> Result<Vec<(PublicKey, u64, u64)>> {
        let mut out = Vec::with_capacity(global_indexes.len());
        for &index in global_indexes {
            let o = self
                .output_row(amount, index)?
                .ok_or(Error::OutputNotFound { amount, index })?;
            out.push((PublicKey(o.key), o.unlock_time, o.height));
        }
        Ok(out)
    }

    /// Ring members for one input as the C++ main chain resolves them
    /// (`DatabaseBlockchainCache::extractKeyOutputs`, `database_blockchain_cache.cpp`): the
    /// requested indexes are read in one batch, so a repeated index is read once; the outputs
    /// that exist are then visited through a `std::map` keyed by `(amount, global index)`, so
    /// they come back sorted by global index; an index without an output is left out
    /// (`deserializeValues` drops missing rows). Returns `(global index, key, unlock_time,
    /// height)` per member, ascending and without duplicates, whatever the order of
    /// `global_indexes`. Callers compare the member count with the signature count, as C++
    /// does, and check each member's unlock time in this order.
    pub fn ring_keys(
        &self,
        amount: u64,
        global_indexes: &[u64],
    ) -> Result<Vec<(u64, PublicKey, u64, u64)>> {
        let mut indexes = global_indexes.to_vec();
        indexes.sort_unstable();
        indexes.dedup();
        let mut out = Vec::with_capacity(indexes.len());
        for index in indexes {
            if let Some(o) = self.output_row(amount, index)? {
                out.push((index, PublicKey(o.key), o.unlock_time, o.height));
            }
        }
        Ok(out)
    }

    /// Output rows for `(amount, global index)` pairs; `None` where missing.
    pub fn outputs(&self, keys: &[(u64, u64)]) -> Result<Vec<Option<OutputInfo>>> {
        keys.iter()
            .map(|&(amount, index)| self.output_row(amount, index))
            .collect()
    }

    pub fn is_key_image_spent(&self, key_image: &KeyImage) -> Result<bool> {
        Ok(self.key_images.get(key_image.0)?.is_some())
    }

    /// Batch form of [`Self::is_key_image_spent`].
    pub fn key_images_spent(&self, key_images: &[KeyImage]) -> Result<Vec<bool>> {
        key_images
            .iter()
            .map(|k| self.is_key_image_spent(k))
            .collect()
    }

    // ------------------------------------------------------------------ records

    /// Runs `f` on the stored record of the block at `height`; `None` if there is none.
    /// Sealed chunks are decoded once into `comp`'s cache.
    fn with_record<R>(
        &self,
        height: u64,
        comp: &mut Compressor,
        f: impl FnOnce(&BlockRecord, SigSource) -> Result<R>,
    ) -> Result<Option<R>> {
        let Ok(h) = u32::try_from(height) else {
            return Ok(None);
        };
        if height < self.sealed_below {
            let chunk = height / CHUNK_BLOCKS;
            let last = clamp32(chunk * CHUNK_BLOCKS + CHUNK_BLOCKS - 1);
            let tag = self
                .block_infos
                .get(last)?
                .map(|g| g.value().hash)
                .ok_or(Error::Corrupt("sealed chunk without block info"))?;
            let hit = comp
                .chunk_cache
                .as_ref()
                .is_some_and(|c| c.chunk == chunk && c.tag == tag);
            if !hit {
                let framed = read_parts(&self.body_parts, chunk)?
                    .ok_or(Error::Corrupt("sealed chunk missing"))?;
                let records = decode_body(&framed, comp)?;
                if records.len() as u64 != CHUNK_BLOCKS {
                    return Err(Error::Corrupt("sealed chunk with a wrong block count"));
                }
                comp.chunk_cache = Some(Box::new(ChunkCache::new(chunk, tag, records)));
            }
            let cache = comp.chunk_cache.as_ref().expect("filled above");
            let i = (height - chunk * CHUNK_BLOCKS) as usize;
            let src = SigSource::Chunk {
                chunk,
                base: cache.sig_base[i],
            };
            return f(&cache.records[i], src).map(Some);
        }
        match self.hot_blocks.get(h)? {
            Some(g) => {
                let record = BlockRecord::decode(g.value())?;
                f(&record, SigSource::Hot(h)).map(Some)
            }
            None => Ok(None),
        }
    }

    /// The decoded record of the block at `height` (blob and prefixes keys-stripped).
    pub fn block_record(&self, height: u64, comp: &mut Compressor) -> Result<Option<BlockRecord>> {
        self.with_record(height, comp, |r, _| Ok(r.clone()))
    }

    /// Puts the output keys back into a stripped blob.
    fn restore(&self, stripped: &[u8], outputs_at: u32, global_indexes: &[u32]) -> Result<Vec<u8>> {
        if outputs_at == 0 {
            return Ok(stripped.to_vec());
        }
        unstrip_output_keys(
            stripped,
            (outputs_at - 1) as usize,
            global_indexes.len(),
            |i, amount| {
                let gi = global_indexes[i];
                self.outputs
                    .get((amount, gi))?
                    .map(|g| g.value().key)
                    .ok_or(Error::OutputNotFound {
                        amount,
                        index: u64::from(gi),
                    })
            },
        )
    }

    /// Signature bytes `off..off + len` of a block (`off` relative to the block).
    /// [`Error::Pruned`] exactly when the prune state drops the block; missing signatures of a
    /// kept block are corruption.
    fn sig_range(&self, height: u64, src: SigSource, off: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if self.prune.dropped(height) {
            return Err(Error::Pruned(height));
        }
        let found = match src {
            SigSource::Hot(h) => match self.hot_sigs.get(h)? {
                Some(g) => {
                    let v = g.value();
                    let (s, e) = (off as usize, (off + len) as usize);
                    Some(
                        v.get(s..e)
                            .ok_or(Error::Corrupt("signature row shorter than its record"))?
                            .to_vec(),
                    )
                }
                None => None,
            },
            SigSource::Chunk { chunk, base } => {
                read_part_range(&self.sig_parts, chunk, base + off, len)?
            }
        };
        found.ok_or(Error::Corrupt("signatures missing"))
    }

    // ------------------------------------------------------------------ blobs

    /// The block and its transactions as on the wire (`/getrawblocks`, P2P `GET_OBJECTS`).
    /// Fails with [`Error::Pruned`] if the block has transactions and its signatures were
    /// pruned ([`Self::has_full_block`] is false).
    pub fn raw_block(&self, height: u64, comp: &mut Compressor) -> Result<Option<RawBlock>> {
        self.with_record(height, comp, |rec, src| {
            let block = self.restore(&rec.blob, rec.miner_outputs_at, &rec.miner_global_indexes)?;
            let sigs = self.sig_range(height, src, 0, rec.sig_bytes())?;
            let mut transactions = Vec::with_capacity(rec.txs.len());
            let mut at = 0_usize;
            for t in &rec.txs {
                let mut blob = self.restore(&t.prefix, t.outputs_at, &t.global_indexes)?;
                let n = t.sig_len as usize;
                blob.extend_from_slice(
                    sigs.get(at..at + n)
                        .ok_or(Error::Corrupt("signature stream shorter than its records"))?,
                );
                at += n;
                transactions.push(blob);
            }
            Ok(RawBlock {
                block,
                transactions,
            })
        })
    }

    /// The block blob (header + miner tx + tx hashes), as on the wire.
    pub fn block_blob(&self, height: u64, comp: &mut Compressor) -> Result<Option<Vec<u8>>> {
        self.with_record(height, comp, |rec, _| {
            self.restore(&rec.blob, rec.miner_outputs_at, &rec.miner_global_indexes)
        })
    }

    /// The block blob, every transaction prefix and every global index of the block at
    /// `height`, from one record read. Never [`Error::Pruned`]: signatures are not read.
    pub fn block_prefixes(
        &self,
        height: u64,
        comp: &mut Compressor,
    ) -> Result<Option<BlockPrefixes>> {
        self.with_record(height, comp, |rec, _| {
            let block = self.restore(&rec.blob, rec.miner_outputs_at, &rec.miner_global_indexes)?;
            let txs = rec
                .txs
                .iter()
                .map(|t| {
                    Ok((
                        self.restore(&t.prefix, t.outputs_at, &t.global_indexes)?,
                        t.global_indexes.clone(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(BlockPrefixes {
                block,
                miner_global_indexes: rec.miner_global_indexes.clone(),
                txs,
            })
        })
    }

    /// Transaction prefix blob at `(height, tx_index)` (`tx_index >= 1`).
    pub fn tx_prefix_at(
        &self,
        height: u64,
        tx_index: u32,
        comp: &mut Compressor,
    ) -> Result<Option<Vec<u8>>> {
        let Some(i) = (tx_index as usize).checked_sub(1) else {
            return Ok(None);
        };
        Ok(self
            .with_record(height, comp, |rec, _| {
                rec.txs
                    .get(i)
                    .map(|t| self.restore(&t.prefix, t.outputs_at, &t.global_indexes))
                    .transpose()
            })?
            .flatten())
    }

    /// Full transaction blob (prefix + signatures) at `(height, tx_index)`.
    fn tx_blob_at(&self, height: u64, tx_index: u32, comp: &mut Compressor) -> Result<Vec<u8>> {
        let i = (tx_index as usize)
            .checked_sub(1)
            .ok_or(Error::Corrupt("the miner transaction has no separate blob"))?;
        self.with_record(height, comp, |rec, src| {
            let t = rec
                .txs
                .get(i)
                .ok_or(Error::Corrupt("missing transaction prefix"))?;
            let mut blob = self.restore(&t.prefix, t.outputs_at, &t.global_indexes)?;
            blob.extend(self.sig_range(height, src, rec.sig_offset(i), u64::from(t.sig_len))?);
            Ok(blob)
        })?
        .ok_or(Error::BlockNotFound(height))
    }

    pub fn tx_location(&self, hash: &Hash) -> Result<Option<TxLoc>> {
        Ok(self.tx_hash_to_loc.get(hash.0)?.map(|g| {
            let row = g.value();
            TxLoc {
                height: u64::from(row.height),
                tx_index: row.tx_index,
            }
        }))
    }

    /// Full transaction blob by hash. `Ok(None)` for unknown hashes **and** for miner
    /// transactions (those are part of the block blob; use [`Self::tx_location`] +
    /// [`Self::block_blob`]).
    pub fn transaction_blob(&self, hash: &Hash, comp: &mut Compressor) -> Result<Option<Vec<u8>>> {
        match self.tx_location(hash)? {
            Some(loc) if loc.tx_index > 0 => {
                self.tx_blob_at(loc.height, loc.tx_index, comp).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Global output indexes of one transaction (`/get_o_indexes`); `tx_index` 0 = miner tx.
    pub fn tx_global_indexes(
        &self,
        height: u64,
        tx_index: u32,
        comp: &mut Compressor,
    ) -> Result<Option<Vec<u32>>> {
        Ok(self
            .with_record(height, comp, |rec, _| {
                Ok(match tx_index {
                    0 => Some(rec.miner_global_indexes.clone()),
                    i => rec
                        .txs
                        .get(i as usize - 1)
                        .map(|t| t.global_indexes.clone()),
                })
            })?
            .flatten())
    }

    /// `/get_global_indexes_for_range`: every transaction in heights `start..end` with its
    /// global indexes, keyed by `(height, tx_index)` (0 = miner tx).
    pub fn global_indexes_for_range(
        &self,
        start: u64,
        end: u64,
        comp: &mut Compressor,
    ) -> Result<Vec<TxGlobalIndexes>> {
        let Some(top) = self.top_height()? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for height in start..end.min(top + 1) {
            self.with_record(height, comp, |rec, _| {
                out.push(((height, 0), rec.miner_global_indexes.clone()));
                for (i, t) in rec.txs.iter().enumerate() {
                    out.push(((height, (i + 1) as u32), t.global_indexes.clone()));
                }
                Ok(())
            })?;
        }
        Ok(out)
    }

    /// `/get_transaction_hashes_by_payment_id`, in chain order.
    pub fn tx_hashes_by_payment_id(
        &self,
        payment_id: &[u8; 32],
        comp: &mut Compressor,
    ) -> Result<Vec<Hash>> {
        let mut locs = Vec::new();
        for row in self
            .payment_ids
            .range((*payment_id, 0_u32, 0_u32)..=(*payment_id, u32::MAX, u32::MAX))?
        {
            let (k, _) = row?;
            let (_, h, idx) = k.value();
            locs.push((h, idx));
        }
        let mut out = Vec::with_capacity(locs.len());
        for (h, idx) in locs {
            let hash = self
                .with_record(u64::from(h), comp, |rec, _| Ok(rec.tx_hash(idx)))?
                .flatten()
                .ok_or(Error::Corrupt("payment id row without its transaction"))?;
            out.push(Hash(hash));
        }
        Ok(out)
    }

    /// Height of the block with the smallest timestamp `>= timestamp`
    /// (wallet sync start; block timestamps are not strictly monotonic).
    pub fn height_by_timestamp(&self, timestamp: u64) -> Result<Option<u64>> {
        let mut range = self.timestamps.range((timestamp, 0_u32)..)?;
        match range.next() {
            Some(row) => {
                let (k, _) = row?;
                Ok(Some(u64::from(k.value().1)))
            }
            None => Ok(None),
        }
    }

    /// C++ `DatabaseBlockchainCache::getTimestampLowerBoundBlockIndex`
    /// (`src/cryptonote_core/database_blockchain_cache.cpp`): `timestamp` is rounded down to
    /// midnight (UTC); from that day backwards, the first day that has a main-chain block gives
    /// the lowest height among that day's blocks (the first block of the day in chain order).
    /// 0 when no day after day 0 has a block, and for timestamps before the end of day 0 (C++
    /// stops walking back at midnight 0).
    pub fn timestamp_lower_bound_height(&self, timestamp: u64) -> Result<u64> {
        const DAY: u64 = 86_400;
        let midnight = timestamp - timestamp % DAY;
        if midnight == 0 {
            return Ok(0);
        }
        // The latest indexed timestamp before the end of that day names the day to use.
        let mut before_day_end = self
            .timestamps
            .range(..(midnight.saturating_add(DAY), 0_u32))?;
        let Some(row) = before_day_end.next_back() else {
            return Ok(0);
        };
        let latest = row?.0.value().0;
        let day = latest - latest % DAY;
        if day == 0 {
            return Ok(0);
        }
        let mut first: Option<u32> = None;
        for row in self
            .timestamps
            .range((day, 0_u32)..(day.saturating_add(DAY), 0_u32))?
        {
            let height = row?.0.value().1;
            first = Some(first.map_or(height, |f| f.min(height)));
        }
        Ok(first.map_or(0, u64::from))
    }

    /// Heights of the main-chain blocks whose timestamp is in `begin..end`, ordered by
    /// timestamp and then height, at most `limit` (C++
    /// `DatabaseBlockchainCache::getBlockHashesByTimestamps`, which walks the timestamps of the
    /// range in ascending order).
    pub fn heights_by_timestamps(&self, begin: u64, end: u64, limit: usize) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        if begin >= end {
            return Ok(out);
        }
        for row in self
            .timestamps
            .range((begin, 0_u32)..(end, 0_u32))?
            .take(limit)
        {
            out.push(u64::from(row?.0.value().1));
        }
        Ok(out)
    }

    /// An alternative block and its header.
    pub fn alt_block(
        &self,
        hash: &Hash,
        comp: &mut Compressor,
    ) -> Result<Option<(AltBlockHeader, RawBlock)>> {
        let Some(value) = self.alt_blocks.get(hash.0)?.map(|g| g.value().to_vec()) else {
            return Ok(None);
        };
        let hl = size_of::<AltBlockHeader>();
        if value.len() < hl {
            return Err(Error::Corrupt("alt block shorter than its header"));
        }
        let header: AltBlockHeader = bytemuck::pod_read_unaligned(&value[..hl]);
        let raw = decode_raw_block(&comp.decompress(&value[hl..])?)
            .ok_or(Error::Corrupt("bad alt block encoding"))?;
        Ok(Some((header, raw)))
    }

    // ------------------------------------------------------------------ verification helpers

    /// Entry counts of the index tables (for `xkrd db verify`).
    pub fn table_lens(&self) -> Result<Vec<(&'static str, u64)>> {
        Ok(vec![
            ("block_infos", self.block_infos.len()?),
            ("block_hash_to_height", self.block_hash_to_height.len()?),
            ("hot_blocks", self.hot_blocks.len()?),
            ("hot_sigs", self.hot_sigs.len()?),
            ("body_parts", self.body_parts.len()?),
            ("sig_parts", self.sig_parts.len()?),
            ("tx_hash_to_loc", self.tx_hash_to_loc.len()?),
            ("key_images", self.key_images.len()?),
            ("outputs", self.outputs.len()?),
            ("unlock_overflow", self.unlock_overflow.len()?),
            ("output_counts", self.output_counts.len()?),
            ("payment_ids", self.payment_ids.len()?),
            ("timestamps", self.timestamps.len()?),
        ])
    }

    /// Whether the hot row of `height` exists (`xkrd db verify`).
    pub fn has_hot_row(&self, height: u64) -> Result<bool> {
        Ok(match u32::try_from(height) {
            Ok(h) => self.hot_blocks.get(h)?.is_some(),
            Err(_) => false,
        })
    }

    /// Whether `(payment_id, height, tx_index)` is indexed (`xkrd db verify`).
    pub fn has_payment_id(
        &self,
        payment_id: &[u8; 32],
        height: u64,
        tx_index: u32,
    ) -> Result<bool> {
        Ok(match u32::try_from(height) {
            Ok(h) => self.payment_ids.get((*payment_id, h, tx_index))?.is_some(),
            Err(_) => false,
        })
    }

    /// Whether `(timestamp, height)` is indexed (`xkrd db verify`).
    pub fn has_timestamp(&self, timestamp: u64, height: u64) -> Result<bool> {
        Ok(match u32::try_from(height) {
            Ok(h) => self.timestamps.get((timestamp, h))?.is_some(),
            Err(_) => false,
        })
    }
}
