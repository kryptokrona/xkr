//! Batch writes. One call = many blocks, all tables opened once.

use std::collections::{BTreeMap, HashMap, hash_map::Entry};
use std::ops::Bound;

use redb::{ReadableTable, Table, WriteTransaction};
use xkr_types::{Hash, RawBlock, TransactionPrefix, TxInput, TxOutputTarget};

use super::chunk::{compressor_from_blobs, delete_parts, read_parts, write_parts};
use super::{VerifiedBlock, encode_raw_block};
use crate::{
    Error, Result,
    codec::{BlockRecord, TxRecord, decode_chunk, encode_chunk, strip_output_keys},
    compress::Compressor,
    pruning::{PruneReport, PruneState, PruningSeed, STRIPE_SPAN, state_from_meta},
    schema::{
        ALT_BLOCK_HEIGHTS, ALT_BLOCKS, AltBlockHeader, BLOCK_HASH_TO_HEIGHT, BLOCK_INFOS,
        BODY_PARTS, BlockInfo, BlockInfoRow, CHUNK_BLOCKS, DEFAULT_SEAL_LAG, HOT_BLOCKS, HOT_SIGS,
        KEY_IMAGES, META, META_BLOBS, OUTPUT_COUNTS, OUTPUTS, OutputRow, PAYMENT_IDS, SIG_PARTS,
        TIMESTAMPS, TX_HASH_TO_LOC, TxLocRow, UNLOCK_ESCAPE, UNLOCK_OVERFLOW, clamp32, h32, meta,
    },
};

/// All chain tables of one write transaction.
struct WriteTables<'txn> {
    block_infos: Table<'txn, u32, BlockInfoRow>,
    block_hash_to_height: Table<'txn, [u8; 32], u32>,
    hot_blocks: Table<'txn, u32, &'static [u8]>,
    hot_sigs: Table<'txn, u32, &'static [u8]>,
    body_parts: Table<'txn, u64, &'static [u8]>,
    sig_parts: Table<'txn, u64, &'static [u8]>,
    tx_hash_to_loc: Table<'txn, [u8; 32], TxLocRow>,
    key_images: Table<'txn, [u8; 32], ()>,
    outputs: Table<'txn, (u64, u32), OutputRow>,
    unlock_overflow: Table<'txn, (u64, u32), u64>,
    output_counts: Table<'txn, u64, u64>,
    payment_ids: Table<'txn, ([u8; 32], u32, u32), ()>,
    timestamps: Table<'txn, (u64, u32), ()>,
    meta: Table<'txn, &'static str, u64>,
    meta_blobs: Table<'txn, &'static str, &'static [u8]>,
}

impl<'txn> WriteTables<'txn> {
    fn open(txn: &'txn WriteTransaction) -> Result<Self> {
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
            meta: txn.open_table(META)?,
            meta_blobs: txn.open_table(META_BLOBS)?,
        })
    }

    fn meta_u64(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.meta.get(key)?.map(|g| g.value()))
    }

    fn top_height(&self) -> Result<Option<u64>> {
        self.meta_u64(meta::TOP_HEIGHT)
    }

    /// The stored prune state (seed and height).
    fn prune_state(&self) -> Result<PruneState> {
        state_from_meta(
            self.meta_u64(meta::PRUNING_SEED)?,
            self.meta_u64(meta::PRUNED_BELOW)?.unwrap_or(0),
        )
    }

    fn block_row(&self, height: u64) -> Result<BlockInfoRow> {
        self.block_infos
            .get(h32(height)?)?
            .map(|g| g.value())
            .ok_or(Error::BlockNotFound(height))
    }
}

/// Output counts touched by a batch, written back once at the end.
type CountCache = HashMap<u64, u64>;

/// New output rows of a batch per amount, inserted at the end as one ordered run per amount.
type PendingOutputs = BTreeMap<u64, Vec<(u32, OutputRow, Option<u64>)>>;

fn count_for<'c>(
    table: &Table<'_, u64, u64>,
    cache: &'c mut CountCache,
    amount: u64,
) -> Result<&'c mut u64> {
    Ok(match cache.entry(amount) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(e) => e.insert(table.get(amount)?.map_or(0, |g| g.value())),
    })
}

fn flush_counts(table: &mut Table<'_, u64, u64>, cache: CountCache) -> Result<()> {
    for (amount, count) in cache {
        if count == 0 {
            table.remove(amount)?;
        } else {
            table.insert(amount, count)?;
        }
    }
    Ok(())
}

/// Inserts the batch's outputs through one cursor per amount. A cursor run rebuilds the leaf
/// at the insertion point packed full (greedy), instead of the half/half split of single
/// inserts in the middle of the tree, which left `outputs` about half full.
fn flush_outputs(t: &mut WriteTables<'_>, pending: PendingOutputs) -> Result<()> {
    for (amount, rows) in pending {
        let Some(&(first, _, _)) = rows.first() else {
            continue;
        };
        {
            let mut cursor = t
                .outputs
                .lower_bound_mut(Bound::Included((amount, first)))?;
            for (gi, row, _) in &rows {
                cursor.insert_before((amount, *gi), *row)?;
            }
            cursor.close()?;
        }
        for (gi, _, overflow) in &rows {
            if let Some(unlock) = overflow {
                t.unlock_overflow.insert((amount, *gi), *unlock)?;
            }
        }
    }
    Ok(())
}

/// Appends verified blocks to the main chain. `blocks` must start at `top + 1` (or 0 on an
/// empty database) and be consecutive. Everything happens inside the caller's write
/// transaction; nothing is visible until it commits. Chunks that fell `seal_lag` blocks behind
/// the new tip are sealed (compressed with `comp`) in the same transaction. Signatures of
/// blocks the database's prune state drops (only after a pop below the prune height) are not
/// stored.
///
/// Fails (and the caller must abort the transaction) on a non-contiguous height or a key
/// image that is already in the chain — consensus should have rejected both.
pub fn push_blocks(
    txn: &WriteTransaction,
    blocks: &[VerifiedBlock],
    comp: &mut Compressor,
) -> Result<()> {
    if blocks.is_empty() {
        return Ok(());
    }
    let mut t = WriteTables::open(txn)?;
    let mut next = t.top_height()?.map_or(0, |h| h + 1);
    let mut prev = if next == 0 {
        None
    } else {
        Some(t.block_row(next - 1)?.to_info(false))
    };
    let mut counts = CountCache::new();
    let mut pending = PendingOutputs::new();
    let prune = t.prune_state()?;

    for b in blocks {
        if b.height != next {
            return Err(Error::NonContiguous {
                expected: next,
                got: b.height,
            });
        }
        let info = push_one(&mut t, b, prev.as_ref(), &mut counts, &mut pending, &prune)?;
        prev = Some(info);
        next += 1;
    }

    flush_outputs(&mut t, pending)?;
    flush_counts(&mut t.output_counts, counts)?;
    t.meta.insert(meta::TOP_HEIGHT, next - 1)?;
    seal_ready(&mut t, comp, next - 1, &prune)
}

#[allow(unreachable_patterns)] // TxOutputTarget may only have the `Key` variant
fn output_key(target: &TxOutputTarget) -> Result<[u8; 32]> {
    match target {
        TxOutputTarget::Key(k) => Ok(k.0),
        TxOutputTarget::TaggedKey { key, .. } => Ok(key.0),
        _ => Err(Error::Corrupt("unsupported output target")),
    }
}

fn outputs_of(prefix: &TransactionPrefix) -> Result<Vec<(u64, [u8; 32])>> {
    prefix
        .outputs
        .iter()
        .map(|o| Ok((o.amount, output_key(&o.target)?)))
        .collect()
}

/// `blob` with the output keys cut out, and `1 + offset` of the first output (0 = unstripped).
fn strip(blob: &[u8], outputs: &[(u64, [u8; 32])]) -> (Vec<u8>, u32) {
    if let Some((off, stripped)) = strip_output_keys(blob, outputs)
        && let Ok(at) = u32::try_from(off + 1)
    {
        return (stripped, at);
    }
    (blob.to_vec(), 0)
}

fn push_one(
    t: &mut WriteTables<'_>,
    b: &VerifiedBlock,
    prev: Option<&BlockInfo>,
    counts: &mut CountCache,
    pending: &mut PendingOutputs,
    prune: &PruneState,
) -> Result<BlockInfo> {
    let h = h32(b.height)?;

    // Miner transaction: index 0, lives inside the block blob.
    let miner_prefix = &b.block.miner_tx.prefix;
    let miner_outputs = outputs_of(miner_prefix)?;
    let miner_global_indexes = assign_outputs(
        t,
        counts,
        pending,
        &miner_outputs,
        miner_prefix.unlock_time,
        h,
    )?;
    t.tx_hash_to_loc.insert(
        b.miner_tx_hash.0,
        TxLocRow {
            height: h,
            tx_index: 0,
        },
    )?;
    let (blob, miner_outputs_at) = strip(&b.block_blob, &miner_outputs);

    let mut sigs = Vec::new();
    let mut txs = Vec::with_capacity(b.txs.len());
    for (i, tx) in b.txs.iter().enumerate() {
        let idx =
            u32::try_from(i + 1).map_err(|_| Error::Corrupt("too many transactions in block"))?;
        if tx.prefix_len > tx.blob.len() {
            return Err(Error::BadPrefixLen {
                tx: tx.hash.to_string(),
                prefix_len: tx.prefix_len,
                blob_len: tx.blob.len(),
            });
        }
        let (prefix, prunable) = tx.blob.split_at(tx.prefix_len);

        for input in &tx.tx.prefix.inputs {
            if let TxInput::Key { key_image, .. } = input
                && t.key_images.insert(key_image.0, ())?.is_some()
            {
                return Err(Error::KeyImageSpent(key_image.to_string()));
            }
        }

        let outputs = outputs_of(&tx.tx.prefix)?;
        let global_indexes =
            assign_outputs(t, counts, pending, &outputs, tx.tx.prefix.unlock_time, h)?;
        let (stored, outputs_at) = strip(prefix, &outputs);
        t.tx_hash_to_loc.insert(
            tx.hash.0,
            TxLocRow {
                height: h,
                tx_index: idx,
            },
        )?;
        if let Some(pid) = tx.payment_id {
            t.payment_ids.insert((pid, h, idx), ())?;
        }
        sigs.extend_from_slice(prunable);
        txs.push(TxRecord {
            prefix: stored,
            outputs_at,
            global_indexes,
            sig_len: u32::try_from(prunable.len())
                .map_err(|_| Error::Corrupt("signatures larger than 4 GiB"))?,
        });
    }

    let record = BlockRecord {
        blob,
        miner_outputs_at,
        miner_global_indexes,
        txs,
    };
    t.hot_blocks.insert(h, record.encode().as_slice())?;
    if !sigs.is_empty() && !prune.dropped(b.height) {
        t.hot_sigs.insert(h, sigs.as_slice())?;
    }
    t.block_hash_to_height.insert(b.hash.0, h)?;
    t.timestamps.insert((b.block.header.timestamp, h), ())?;

    let tx_count =
        u32::try_from(b.txs.len()).map_err(|_| Error::Corrupt("too many transactions in block"))?;
    let mut info = BlockInfo {
        hash: b.hash.0,
        timestamp: b.block.header.timestamp,
        difficulty: b.difficulty,
        cumulative_difficulty_lo: 0,
        cumulative_difficulty_hi: 0,
        already_generated_coins: b.already_generated_coins,
        cumulative_tx_count: prev.map_or(0, |p| p.cumulative_tx_count) + 1 + u64::from(tx_count),
        block_size: b.block_size,
        cumulative_size: prev.map_or(0, |p| p.cumulative_size) + b.block_size,
        tx_count,
        flags: 0,
    };
    info.set_cumulative_difficulty(
        prev.map_or(0, BlockInfo::cumulative_difficulty) + u128::from(b.difficulty),
    );
    let row = BlockInfoRow::from_info(&info).ok_or(Error::Corrupt(
        "block info out of range (cumulative difficulty above u64 or block above 4 GiB)",
    ))?;
    t.block_infos.insert(h, row)?;
    Ok(info)
}

/// Assigns per-amount global indexes to the outputs of one transaction and queues their rows.
fn assign_outputs(
    t: &WriteTables<'_>,
    counts: &mut CountCache,
    pending: &mut PendingOutputs,
    outputs: &[(u64, [u8; 32])],
    unlock_time: u64,
    height: u32,
) -> Result<Vec<u32>> {
    let unlock = u32::try_from(unlock_time)
        .ok()
        .filter(|u| *u != UNLOCK_ESCAPE);
    let mut indexes = Vec::with_capacity(outputs.len());
    for (amount, key) in outputs {
        let count = count_for(&t.output_counts, counts, *amount)?;
        let global =
            u32::try_from(*count).map_err(|_| Error::Corrupt("global output index exceeds u32"))?;
        *count += 1;
        pending.entry(*amount).or_default().push((
            global,
            OutputRow {
                key: *key,
                height,
                unlock: unlock.unwrap_or(UNLOCK_ESCAPE),
            },
            unlock.is_none().then_some(unlock_time),
        ));
        indexes.push(global);
    }
    Ok(indexes)
}

/// Removes the outputs of one transaction (they are the newest of their amounts).
fn remove_outputs(
    t: &mut WriteTables<'_>,
    counts: &mut CountCache,
    prefix: &TransactionPrefix,
    height: u32,
) -> Result<()> {
    for out in prefix.outputs.iter().rev() {
        let key = output_key(&out.target)?;
        let count = count_for(&t.output_counts, counts, out.amount)?;
        let Some(last) = count.checked_sub(1) else {
            return Err(Error::Corrupt("output count underflow"));
        };
        *count = last;
        let gi = u32::try_from(last).map_err(|_| Error::Corrupt("global index exceeds u32"))?;
        let removed = t.outputs.remove((out.amount, gi))?.map(|g| g.value());
        match removed {
            Some(o) if o.key == key && o.height == height => {
                if o.unlock == UNLOCK_ESCAPE {
                    t.unlock_overflow.remove((out.amount, gi))?;
                }
            }
            _ => {
                return Err(Error::Corrupt(
                    "popped output does not belong to the popped transaction",
                ));
            }
        }
    }
    Ok(())
}

/// Seals every chunk whose last block is at least `seal_lag` blocks below `top`.
fn seal_ready(
    t: &mut WriteTables<'_>,
    comp: &mut Compressor,
    top: u64,
    prune: &PruneState,
) -> Result<()> {
    let lag = t.meta_u64(meta::SEAL_LAG)?.unwrap_or(DEFAULT_SEAL_LAG);
    let start = t.meta_u64(meta::SEALED_BELOW)?.unwrap_or(0);
    let mut sealed = start;
    while sealed + CHUNK_BLOCKS + lag <= top + 1 {
        seal_chunk(t, comp, sealed / CHUNK_BLOCKS, prune)?;
        sealed += CHUNK_BLOCKS;
    }
    if sealed != start {
        t.meta.insert(meta::SEALED_BELOW, sealed)?;
    }
    Ok(())
}

/// Moves the hot rows of `chunk` into `BODY_PARTS` (compressed records) and `SIG_PARTS`. A
/// chunk that `prune` drops gets no signature parts: a node that prunes keeps pruning as
/// chunks are sealed below its prune height (after a pop and re-push there); chunks above it
/// keep theirs until the next prune call.
fn seal_chunk(
    t: &mut WriteTables<'_>,
    comp: &mut Compressor,
    chunk: u64,
    prune: &PruneState,
) -> Result<()> {
    let start = chunk * CHUNK_BLOCKS;
    let end = start + CHUNK_BLOCKS;
    let mut records = Vec::with_capacity(CHUNK_BLOCKS as usize);
    let mut sigs = Vec::new();
    for height in start..end {
        let h = h32(height)?;
        let record = t
            .hot_blocks
            .get(h)?
            .map(|g| g.value().to_vec())
            .ok_or(Error::Corrupt("hot block missing while sealing a chunk"))?;
        records.push(record);
        if let Some(g) = t.hot_sigs.get(h)? {
            sigs.extend_from_slice(g.value());
        }
    }
    let framed = comp.compress_chunk(&encode_chunk(&records))?;
    write_parts(&mut t.body_parts, chunk, &framed)?;
    if !sigs.is_empty() && !prune.chunk_dropped(chunk) {
        write_parts(&mut t.sig_parts, chunk, &sigs)?;
    }
    let range = clamp32(start)..clamp32(end);
    t.hot_blocks.retain_in(range.clone(), |_, _| false)?;
    t.hot_sigs.retain_in(range, |_, _| false)?;
    Ok(())
}

/// Reverses [`seal_chunk`] (a pop below the sealed boundary). Pruned signatures stay pruned.
fn unseal_chunk(
    t: &mut WriteTables<'_>,
    comp: &mut Compressor,
    chunk: u64,
    prune: &PruneState,
) -> Result<()> {
    let start = chunk * CHUNK_BLOCKS;
    let framed = read_parts(&t.body_parts, chunk)?.ok_or(Error::Corrupt("sealed chunk missing"))?;
    let body = comp.decompress(&framed)?;
    let records = decode_chunk(&body)?;
    if records.len() as u64 != CHUNK_BLOCKS {
        return Err(Error::Corrupt("sealed chunk with a wrong block count"));
    }
    let sigs = if prune.chunk_dropped(chunk) {
        None
    } else {
        read_parts(&t.sig_parts, chunk)?
    };
    let mut at = 0_usize;
    for (i, raw) in records.iter().enumerate() {
        let h = h32(start + i as u64)?;
        t.hot_blocks.insert(h, *raw)?;
        let n = usize::try_from(BlockRecord::decode(raw)?.sig_bytes())
            .map_err(|_| Error::Corrupt("signature length overflow"))?;
        if n > 0
            && let Some(sigs) = &sigs
        {
            let part = sigs
                .get(at..at + n)
                .ok_or(Error::Corrupt("signature stream shorter than its records"))?;
            t.hot_sigs.insert(h, part)?;
        }
        at += n;
    }
    delete_parts(&mut t.body_parts, chunk)?;
    delete_parts(&mut t.sig_parts, chunk)?;
    comp.clear_cache();
    Ok(())
}

/// Removes the top blocks (reorg / rollback). `blocks` must be the current top blocks in
/// **descending** height order with the same content that was pushed (the node rebuilds them
/// from [`super::Reader::raw_block`] + decoding). A block in a sealed chunk unseals that chunk
/// first. Blocks below the prune height cannot be rebuilt, so they are never popped.
pub fn pop_blocks(txn: &WriteTransaction, blocks: &[VerifiedBlock]) -> Result<()> {
    if blocks.is_empty() {
        return Ok(());
    }
    let mut t = WriteTables::open(txn)?;
    let mut counts = CountCache::new();
    let mut comp: Option<Compressor> = None;
    let prune = t.prune_state()?;

    for b in blocks {
        let height = b.height;
        let h = h32(height)?;
        if t.top_height()? != Some(height) || t.block_row(height)?.hash != b.hash.0 {
            return Err(Error::PopMismatch(height));
        }
        let sealed = t.meta_u64(meta::SEALED_BELOW)?.unwrap_or(0);
        if height < sealed {
            if comp.is_none() {
                comp = Some(compressor_from_blobs(&t.meta_blobs)?);
            }
            let c = comp.as_mut().expect("created above");
            // Sealing moves the boundary in whole chunks.
            if sealed < CHUNK_BLOCKS || !sealed.is_multiple_of(CHUNK_BLOCKS) {
                return Err(Error::Corrupt("sealed boundary is not a chunk boundary"));
            }
            unseal_chunk(&mut t, c, sealed / CHUNK_BLOCKS - 1, &prune)?;
            t.meta.insert(meta::SEALED_BELOW, sealed - CHUNK_BLOCKS)?;
        }

        for (i, tx) in b.txs.iter().enumerate().rev() {
            let idx = (i + 1) as u32;
            remove_outputs(&mut t, &mut counts, &tx.tx.prefix, h)?;
            for input in &tx.tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    t.key_images.remove(key_image.0)?;
                }
            }
            if let Some(pid) = tx.payment_id {
                t.payment_ids.remove((pid, h, idx))?;
            }
            t.tx_hash_to_loc.remove(tx.hash.0)?;
        }
        remove_outputs(&mut t, &mut counts, &b.block.miner_tx.prefix, h)?;
        t.tx_hash_to_loc.remove(b.miner_tx_hash.0)?;

        t.hot_blocks.remove(h)?;
        t.hot_sigs.remove(h)?;
        t.block_hash_to_height.remove(b.hash.0)?;
        t.timestamps.remove((b.block.header.timestamp, h))?;
        t.block_infos.remove(h)?;
        match height.checked_sub(1) {
            Some(top) => {
                t.meta.insert(meta::TOP_HEIGHT, top)?;
            }
            None => {
                t.meta.remove(meta::TOP_HEIGHT)?;
            }
        }
    }

    flush_counts(&mut t.output_counts, counts)?;
    Ok(())
}

/// Stripe pruning (`docs/storage.md` "Stripe pruning"): deletes the ring signatures of every
/// block below `below_height` whose stripe `seed` does not keep. `below_height` is rounded down
/// to a multiple of [`STRIPE_SPAN`] and capped at the chain; sealed chunks lose their signature
/// parts, blocks not sealed yet their hot rows (chunks sealed later below the prune height get
/// no parts). Records `META["pruning_seed"]` (also when nothing is pruned: `below_height = 0`
/// only stores the seed) and `META["pruned_below"]`. Idempotent and incremental: a call with a
/// higher height only touches the new range, one with a lower height changes nothing.
///
/// `below_height` is the caller's safety bound (`xkr_chain::Checkpoints::prune_bound`): blocks
/// with dropped signatures can no longer be served whole or popped in a reorg.
///
/// Fails with [`Error::Unsupported`] for the full and the [`PruningSeed::RECENT_ONLY`] seed (use
/// [`prune_all_below`]), for a seed other than the one the database is already pruned with,
/// and on a database pruned with [`prune_all_below`] (the other stripes are gone; only a resync
/// brings them back).
pub fn prune_stripe(
    txn: &WriteTransaction,
    seed: PruningSeed,
    below_height: u64,
) -> Result<PruneReport> {
    if !seed.is_stripe() {
        return Err(Error::Unsupported(
            "stripe pruning needs a stripe seed (prune_all_below drops every signature)",
        ));
    }
    let mut t = WriteTables::open(txn)?;
    let old = t.prune_state()?;
    if old.seed.is_recent_only() || (old.seed.is_full() && old.pruned_below > 0) {
        return Err(Error::Unsupported(
            "every old signature of this database was pruned: a stripe cannot be kept any more \
             (resync to switch to stripe pruning)",
        ));
    }
    if old.seed.is_stripe() && old.seed != seed {
        return Err(Error::Unsupported(
            "the database is already pruned with another pruning seed",
        ));
    }
    t.meta.insert(meta::PRUNING_SEED, u64::from(seed.raw()))?;
    let limit = t.top_height()?.map_or(0, |top| top + 1);
    let target = below_height.min(limit) / STRIPE_SPAN * STRIPE_SPAN;
    let new = PruneState {
        seed,
        pruned_below: target.max(old.pruned_below),
    };
    drop_signatures(&mut t, old, new)
}

/// Full pruning: deletes the ring signatures of **every** block below `below_height` rounded
/// down to a multiple of [`CHUNK_BLOCKS`] (and capped at the chain), stores
/// [`PruningSeed::RECENT_ONLY`] as `META["pruning_seed"]` (so the node announces "recent blocks
/// only", never "full") and records `META["pruned_below"]`. On a stripe-pruned database the
/// kept stripe below its prune height goes too (the prune height never decreases). When
/// nothing ends up pruned (a chain shorter than one chunk) the database is left full (a stored
/// stripe seed is removed). Idempotent; the node never does this by itself
/// (`xkrd db prune --all`).
pub fn prune_all_below(txn: &WriteTransaction, below_height: u64) -> Result<PruneReport> {
    let mut t = WriteTables::open(txn)?;
    let old = t.prune_state()?;
    let limit = t.top_height()?.map_or(0, |top| top + 1);
    let target = below_height.min(limit) / CHUNK_BLOCKS * CHUNK_BLOCKS;
    let pruned_below = target.max(old.pruned_below);
    let seed = if pruned_below > 0 {
        PruningSeed::RECENT_ONLY
    } else {
        PruningSeed::FULL
    };
    let new = PruneState { seed, pruned_below };
    if seed.is_full() {
        t.meta.remove(meta::PRUNING_SEED)?;
    } else {
        t.meta.insert(meta::PRUNING_SEED, u64::from(seed.raw()))?;
    }
    drop_signatures(&mut t, old, new)
}

/// Deletes the signatures that `new` drops and `old` did not, and records `new.pruned_below`.
fn drop_signatures(
    t: &mut WriteTables<'_>,
    old: PruneState,
    new: PruneState,
) -> Result<PruneReport> {
    let mut report = PruneReport {
        state: new,
        ..PruneReport::default()
    };
    // With the same seed everything below the old height is done already.
    let from = if old.seed == new.seed {
        old.pruned_below
    } else {
        0
    };
    let sealed = t.meta_u64(meta::SEALED_BELOW)?.unwrap_or(0);
    for chunk in from / CHUNK_BLOCKS..new.pruned_below / CHUNK_BLOCKS {
        if !new.chunk_dropped(chunk) || old.chunk_dropped(chunk) {
            continue;
        }
        let start = chunk * CHUNK_BLOCKS;
        let range = clamp32(start)..clamp32(start + CHUNK_BLOCKS);
        if start < sealed {
            let parts = delete_parts(&mut t.sig_parts, chunk)?;
            if parts > 0 {
                report.chunks += 1;
                report.parts += parts;
            }
        } else {
            let mut rows = 0;
            t.hot_sigs.retain_in(range.clone(), |_, _| {
                rows += 1;
                false
            })?;
            report.hot_rows += rows;
        }
        for row in t.block_infos.range(range)? {
            let (_, v) = row?;
            report.transactions += u64::from(v.value().tx_count);
        }
    }
    if new.pruned_below != old.pruned_below {
        t.meta.insert(meta::PRUNED_BELOW, new.pruned_below)?;
    }
    Ok(report)
}

/// Stores an alternative-chain block: `AltBlockHeader` bytes followed by the compressed
/// [`encode_raw_block`] encoding.
pub fn put_alt_block(
    txn: &WriteTransaction,
    hash: &Hash,
    header: AltBlockHeader,
    raw: &RawBlock,
    comp: &mut Compressor,
) -> Result<()> {
    let mut value = bytemuck::bytes_of(&header).to_vec();
    value.extend_from_slice(&comp.compress(&encode_raw_block(raw))?);
    txn.open_table(ALT_BLOCKS)?
        .insert(hash.0, value.as_slice())?;
    txn.open_table(ALT_BLOCK_HEIGHTS)?
        .insert((header.height, hash.0), ())?;
    Ok(())
}

/// Deletes one alternative block; returns whether it existed.
pub fn remove_alt_block(txn: &WriteTransaction, hash: &Hash) -> Result<bool> {
    let mut alt = txn.open_table(ALT_BLOCKS)?;
    let Some(value) = alt.remove(hash.0)?.map(|g| g.value().to_vec()) else {
        return Ok(false);
    };
    if value.len() < size_of::<AltBlockHeader>() {
        return Err(Error::Corrupt("alt block shorter than its header"));
    }
    let header: AltBlockHeader =
        bytemuck::pod_read_unaligned(&value[..size_of::<AltBlockHeader>()]);
    txn.open_table(ALT_BLOCK_HEIGHTS)?
        .remove((header.height, hash.0))?;
    Ok(true)
}

/// Drops alternative blocks below `height` (C++ keeps alt chains only near the tip).
pub fn prune_alt_blocks_below(txn: &WriteTransaction, height: u64) -> Result<u64> {
    let mut hashes = Vec::new();
    {
        let mut heights = txn.open_table(ALT_BLOCK_HEIGHTS)?;
        heights.retain_in(..(height, [0_u8; 32]), |(_, hash), ()| {
            hashes.push(hash);
            false
        })?;
    }
    let mut alt = txn.open_table(ALT_BLOCKS)?;
    for h in &hashes {
        alt.remove(*h)?;
    }
    Ok(hashes.len() as u64)
}
