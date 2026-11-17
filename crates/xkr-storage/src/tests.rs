//! Round-trip tests on temporary database files, including sealed chunks, reorgs across the
//! sealed boundary, full and stripe pruning, maintenance, re-compression and a size regression
//! over the real mainnet fixture blocks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use xkr_serialization::{decode_block, decode_transaction, encode_block, encode_transaction};
use xkr_types::{
    Block, BlockHeader, Hash, KeyImage, PublicKey, RawBlock, Signature, Transaction,
    TransactionPrefix, TxInput, TxOutput, TxOutputTarget, payment_id,
};

use crate::{
    Compressor, Error, MaintainOptions, Store, VerifiedBlock, VerifiedTx,
    codec::strip_output_keys,
    ops::{self, encode_raw_block},
    pruning::{PruneMode, PruneReport, PruneState, PruningSeed, STRIPE_SPAN},
    schema::{AltBlockHeader, CHUNK_BLOCKS},
};

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "xkr-storage-test-{}-{}-{nanos}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 32 pseudo-random bytes from `(tag, n)` (splitmix64), so blobs look like real keys.
fn rnd32(tag: u8, n: u64) -> [u8; 32] {
    let mut x = (u64::from(tag) << 56) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = [0_u8; 32];
    for c in out.chunks_mut(8) {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        c.copy_from_slice(&z.to_le_bytes());
    }
    out
}

fn out(amount: u64, key: [u8; 32]) -> TxOutput {
    TxOutput {
        amount,
        target: TxOutputTarget::Key(PublicKey(key)),
    }
}

fn pubkey_extra(key: [u8; 32]) -> Vec<u8> {
    let mut e = vec![0x01];
    e.extend_from_slice(&key);
    e
}

const PID: [u8; 32] = [0x77; 32];

/// Block `height` (major version 1, real CryptoNote encoding): a miner tx paying amounts 100
/// and 5000, and `n_txs` transactions that each spend one key image with a ring of 2 and
/// create outputs of amounts 100 and 7. Tx 0 carries payment id [`PID`]; tx 1 has
/// `unlock_time = u64::MAX` (the escaped unlock path).
fn block(height: u64, n_txs: u64) -> VerifiedBlock {
    let miner_tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: height + 10,
            inputs: vec![TxInput::Coinbase {
                height: height as u32,
            }],
            outputs: vec![
                out(100, rnd32(0xa0, height)),
                out(5000, rnd32(0xa1, height)),
            ],
            extra: pubkey_extra(rnd32(0xa2, height)),
        },
        signatures: vec![],
        clsag_signatures: vec![],
    };
    let txs: Vec<VerifiedTx> = (0..n_txs)
        .map(|i| {
            let id = height * 100 + i;
            let extra = if i == 0 {
                let mut e = vec![0x02, 33, 0x00];
                e.extend_from_slice(&PID);
                e
            } else {
                pubkey_extra(rnd32(0xc1, id))
            };
            let tx = Transaction {
                prefix: TransactionPrefix {
                    version: 1,
                    unlock_time: if i == 1 { u64::MAX } else { 0 },
                    inputs: vec![TxInput::Key {
                        amount: 100,
                        output_indexes: vec![0, 1],
                        key_image: KeyImage(rnd32(0xd0, id)),
                    }],
                    outputs: vec![out(100, rnd32(0xe0, id)), out(7, rnd32(0xe1, id))],
                    extra,
                },
                signatures: vec![vec![Signature(rnd64(0xf0, id)), Signature(rnd64(0xf1, id))]],
                clsag_signatures: vec![],
            };
            let blob = encode_transaction(&tx).unwrap();
            VerifiedTx {
                hash: Hash(rnd32(0xc0, id)),
                prefix_len: blob.len() - 128,
                payment_id: payment_id(&tx.prefix.extra).map(|h| h.0),
                tx,
                blob,
            }
        })
        .collect();
    let block = Block {
        header: BlockHeader {
            major_version: 1,
            minor_version: 0,
            timestamp: 1_600_000_000 + height * 90,
            prev_hash: Hash(rnd32(0xb0, height.wrapping_sub(1))),
            nonce: height as u32,
        },
        parent_block: None,
        miner_tx,
        tx_hashes: txs.iter().map(|t| t.hash).collect(),
    };
    VerifiedBlock {
        height,
        hash: Hash(rnd32(0xb0, height)),
        block_blob: encode_block(&block).unwrap(),
        block,
        miner_tx_hash: Hash(rnd32(0xf9, height)),
        difficulty: 1000 + height,
        already_generated_coins: 1_000_000 * (height + 1),
        block_size: 300 + height,
        txs,
    }
}

fn rnd64(tag: u8, n: u64) -> [u8; 64] {
    let mut s = [0_u8; 64];
    s[..32].copy_from_slice(&rnd32(tag, n));
    s[32..].copy_from_slice(&rnd32(tag ^ 0x55, n));
    s
}

fn chain(n: u64) -> Vec<VerifiedBlock> {
    (0..n)
        .map(|h| block(h, if h == 0 { 0 } else { h % 4 }))
        .collect()
}

/// Blocks `from..to` with few transactions (two in every 16th block, at heights `16k + 3`):
/// cheap enough for chains spanning several stripe spans.
fn sparse_blocks(from: u64, to: u64) -> Vec<VerifiedBlock> {
    (from..to)
        .map(|h| block(h, if h % 16 == 3 { 2 } else { 0 }))
        .collect()
}

/// Everything below `pruned_below` dropped (`prune_all_below`, which stores the "recent blocks
/// only" seed).
fn all_below(pruned_below: u64) -> PruneState {
    PruneState {
        seed: PruningSeed::RECENT_ONLY,
        pruned_below,
    }
}

/// Transactions of `blocks[from..to]` (heights = indexes).
fn txs_in(blocks: &[VerifiedBlock], from: u64, to: u64) -> u64 {
    blocks[from as usize..to as usize]
        .iter()
        .map(|b| b.txs.len() as u64)
        .sum()
}

fn push(store: &Store, blocks: &[VerifiedBlock], batch: usize) {
    let mut comp = store.compressor().unwrap();
    for b in blocks.chunks(batch) {
        let txn = store.begin_write().unwrap();
        ops::push_blocks(&txn, b, &mut comp).unwrap();
        txn.commit().unwrap();
    }
}

fn raw_of(b: &VerifiedBlock) -> RawBlock {
    RawBlock {
        block: b.block_blob.clone(),
        transactions: b.txs.iter().map(|t| t.blob.clone()).collect(),
    }
}

/// Global indexes, keys, unlock times and heights of one transaction's outputs, against a
/// running per-amount count.
fn check_outputs(
    r: &crate::Reader,
    comp: &mut Compressor,
    counts: &mut HashMap<u64, u32>,
    h: u64,
    prefix: &TransactionPrefix,
    tx_index: u32,
) {
    let gis = r.tx_global_indexes(h, tx_index, comp).unwrap().unwrap();
    assert_eq!(gis.len(), prefix.outputs.len());
    for (o, gi) in prefix.outputs.iter().zip(&gis) {
        let next = counts.entry(o.amount).or_default();
        assert_eq!(*gi, *next, "global index at {h}/{tx_index}");
        *next += 1;
        let k = o.key();
        let ring = r.output_keys(o.amount, &[u64::from(*gi)]).unwrap();
        assert_eq!(ring[0], (*k, prefix.unlock_time, h));
    }
}

/// Checks every read path of `store` against the pushed `blocks` (all of them on the chain,
/// starting at height 0), with the signatures that `state` drops missing (`xkrd db verify`
/// in miniature: kept blocks are whole, dropped ones fail with `Pruned`, and sealed chunks have
/// signature parts exactly when they are kept and have transactions).
fn check_all(store: &Store, blocks: &[VerifiedBlock], state: PruneState) {
    let r = store.reader().unwrap();
    let mut comp = store.compressor().unwrap();
    assert_eq!(r.top_height().unwrap(), blocks.last().map(|b| b.height));
    assert_eq!(r.prune_state(), state);
    assert_eq!(r.pruning_seed(), state.seed);
    assert_eq!(r.pruned_below(), state.pruned_below);
    assert!(!r.has_full_block(blocks.len() as u64));
    for chunk in 0..r.sealed_below() / CHUNK_BLOCKS {
        let first = (chunk * CHUNK_BLOCKS) as usize;
        let with_txs = blocks[first..first + CHUNK_BLOCKS as usize]
            .iter()
            .any(|b| !b.txs.is_empty());
        assert_eq!(
            r.has_signature_parts(chunk).unwrap(),
            with_txs && !state.chunk_dropped(chunk),
            "signature parts of chunk {chunk}"
        );
    }
    let mut counts: HashMap<u64, u32> = HashMap::new();
    let mut pid_hashes = Vec::new();
    for b in blocks {
        let h = b.height;
        let info = r.block_info(h).unwrap().unwrap();
        assert_eq!(info.hash, b.hash.0);
        assert_eq!(info.tx_count as usize, b.txs.len());
        assert_eq!(info.is_pruned(), state.dropped(h), "flag of block {h}");
        assert_eq!(r.has_full_block(h), !state.dropped(h), "block {h} whole");
        assert_eq!(r.height_of(&b.hash).unwrap(), Some(h));
        assert_eq!(r.block_blob(h, &mut comp).unwrap().unwrap(), b.block_blob);
        let pruned = state.dropped(h) && !b.txs.is_empty();
        match r.raw_block(h, &mut comp) {
            Ok(raw) => {
                assert!(!pruned, "block {h} should be pruned");
                assert_eq!(raw.unwrap(), raw_of(b), "raw block {h}");
            }
            Err(Error::Pruned(p)) => assert!(pruned && p == h),
            Err(e) => panic!("block {h}: {e}"),
        }

        check_outputs(&r, &mut comp, &mut counts, h, &b.block.miner_tx.prefix, 0);
        let loc = r.tx_location(&b.miner_tx_hash).unwrap().unwrap();
        assert_eq!((loc.height, loc.tx_index), (h, 0));
        for (i, t) in b.txs.iter().enumerate() {
            let idx = (i + 1) as u32;
            check_outputs(&r, &mut comp, &mut counts, h, &t.tx.prefix, idx);
            assert_eq!(
                r.tx_prefix_at(h, idx, &mut comp).unwrap().unwrap(),
                t.blob[..t.prefix_len]
            );
            match r.transaction_blob(&t.hash, &mut comp) {
                Ok(blob) => assert_eq!(blob.unwrap(), t.blob),
                Err(Error::Pruned(_)) => assert!(pruned),
                Err(e) => panic!("{e}"),
            }
            let loc = r.tx_location(&t.hash).unwrap().unwrap();
            assert_eq!((loc.height, loc.tx_index), (h, idx));
            for input in &t.tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    assert!(r.is_key_image_spent(key_image).unwrap());
                }
            }
            if t.payment_id == Some(PID) {
                pid_hashes.push(t.hash);
            }
        }
    }
    for (amount, n) in counts {
        assert_eq!(r.output_count(amount).unwrap(), u64::from(n));
    }
    assert_eq!(
        r.tx_hashes_by_payment_id(&PID, &mut comp).unwrap(),
        pid_hashes
    );
    if let Some(top) = blocks.last() {
        let rows = r
            .global_indexes_for_range(0, top.height + 1, &mut comp)
            .unwrap();
        let txs: usize = blocks.iter().map(|b| b.txs.len() + 1).sum();
        assert_eq!(rows.len(), txs);
        assert_eq!(
            r.height_by_timestamp(blocks[0].block.header.timestamp + 1)
                .unwrap(),
            blocks.get(1).map(|b| b.height)
        );
    }
}

#[test]
fn push_read_pop_hot() {
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    let blocks = chain(12);
    push(&store, &blocks, 5);
    assert_eq!(store.reader().unwrap().sealed_below(), 0);
    check_all(&store, &blocks, PruneState::default());

    let mut comp = store.compressor().unwrap();
    // Non-contiguous pushes and double spends are refused.
    let txn = store.begin_write().unwrap();
    assert!(matches!(
        ops::push_blocks(&txn, &[block(14, 0)], &mut comp),
        Err(Error::NonContiguous {
            expected: 12,
            got: 14
        })
    ));
    drop(txn);
    let mut dup = block(12, 1);
    dup.txs[0].tx.prefix.inputs = blocks[5].txs[0].tx.prefix.inputs.clone();
    let txn = store.begin_write().unwrap();
    assert!(matches!(
        ops::push_blocks(&txn, &[dup], &mut comp),
        Err(Error::KeyImageSpent(_))
    ));
    drop(txn);

    // Pop the top three blocks: everything they added disappears.
    let txn = store.begin_write().unwrap();
    let popped: Vec<_> = blocks[9..].iter().rev().cloned().collect();
    ops::pop_blocks(&txn, &popped).unwrap();
    txn.commit().unwrap();
    check_all(&store, &blocks[..9], PruneState::default());
    let r = store.reader().unwrap();
    for b in &blocks[9..] {
        assert!(r.height_of(&b.hash).unwrap().is_none());
        assert!(r.tx_location(&b.miner_tx_hash).unwrap().is_none());
        for t in &b.txs {
            assert!(r.tx_location(&t.hash).unwrap().is_none());
        }
    }
    let lens: HashMap<_, _> = r.table_lens().unwrap().into_iter().collect();
    drop(r);
    // Tx 1 of a block (only blocks with >= 2 txs have one) has two outputs with an escaped
    // unlock time; the popped blocks' rows are gone.
    let escaped = blocks[..9].iter().filter(|b| b.txs.len() >= 2).count() as u64 * 2;
    assert_eq!(lens["unlock_overflow"], escaped);

    // Re-push them.
    push(&store, &blocks[9..], 10);
    check_all(&store, &blocks, PruneState::default());
}

/// `ring_keys` returns the members ascending by global index, each once, and leaves out
/// indexes without an output; `output_keys` keeps the requested order.
#[test]
fn ring_keys_are_sorted_unique_and_skip_missing() {
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    push(&store, &chain(12), 5);
    let r = store.reader().unwrap();
    let count = r.output_count(100).unwrap();
    assert!(count >= 3, "{count}");
    let member = |i: u64| {
        let (key, unlock, height) = r.output_keys(100, &[i]).unwrap()[0];
        (i, key, unlock, height)
    };
    assert_eq!(
        r.ring_keys(100, &[2, 0, 2, count + 5, 1]).unwrap(),
        vec![member(0), member(1), member(2)]
    );
    assert_eq!(r.ring_keys(100, &[count]).unwrap(), Vec::new());
    let wire = r.output_keys(100, &[2, 0]).unwrap();
    assert_eq!(wire[0].0, member(2).1);
    assert_eq!(wire[1].0, member(0).1);
    assert!(matches!(
        r.output_keys(100, &[count]),
        Err(Error::OutputNotFound { .. })
    ));
}

#[test]
fn sealed_chunks_return_identical_blocks() {
    let blocks = chain(3 * CHUNK_BLOCKS + 17);

    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(5).unwrap();
    push(&store, &blocks, 37);
    let sealed = store.reader().unwrap().sealed_below();
    assert_eq!(sealed, 3 * CHUNK_BLOCKS);
    check_all(&store, &blocks, PruneState::default());

    // Pop across the sealed boundary: the last chunk is unsealed, the rest stays sealed.
    let keep = 2 * CHUNK_BLOCKS + 10;
    let txn = store.begin_write().unwrap();
    let popped: Vec<_> = blocks[keep as usize..].iter().rev().cloned().collect();
    ops::pop_blocks(&txn, &popped).unwrap();
    txn.commit().unwrap();
    assert_eq!(store.reader().unwrap().sealed_below(), 2 * CHUNK_BLOCKS);
    check_all(&store, &blocks[..keep as usize], PruneState::default());

    // Re-push in one batch: sealing catches up.
    push(&store, &blocks[keep as usize..], 1000);
    assert_eq!(store.reader().unwrap().sealed_below(), 3 * CHUNK_BLOCKS);
    check_all(&store, &blocks, PruneState::default());

    // A reader's chunk cache never serves a chunk that was resealed with other content:
    // the cache is tagged with the chunk's last block hash.
    let mut comp = store.compressor().unwrap();
    let r = store.reader().unwrap();
    assert_eq!(
        r.raw_block(70, &mut comp).unwrap().unwrap(),
        raw_of(&blocks[70])
    );
    assert_eq!(
        r.raw_block(3, &mut comp).unwrap().unwrap(),
        raw_of(&blocks[3])
    );
}

#[test]
fn pruning_drops_signatures_only() {
    let blocks = chain(3 * CHUNK_BLOCKS + 30);
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(40).unwrap();
    push(&store, &blocks, 50);
    assert_eq!(store.reader().unwrap().sealed_below(), 2 * CHUNK_BLOCKS);

    // 150 rounds down to 128: chunks 0 and 1 lose their signature parts.
    let txn = store.begin_write().unwrap();
    let report = ops::prune_all_below(&txn, 150).unwrap();
    txn.commit().unwrap();
    assert_eq!(report.transactions, txs_in(&blocks, 0, 128));
    assert_eq!((report.chunks, report.hot_rows), (2, 0));
    assert_eq!(report.state, all_below(128));
    assert_eq!(store.reader().unwrap().pruned_below(), 128);
    check_all(&store, &blocks, all_below(128));

    // Hot rows are pruned too (seal lag 40: blocks >= 128 are hot), up to 192.
    let txn = store.begin_write().unwrap();
    let report = ops::prune_all_below(&txn, 1_000).unwrap();
    txn.commit().unwrap();
    let pruned = store.reader().unwrap().pruned_below();
    assert_eq!(
        pruned,
        (3 * CHUNK_BLOCKS + 30) / CHUNK_BLOCKS * CHUNK_BLOCKS
    );
    let hot_with_txs = blocks[128..192]
        .iter()
        .filter(|b| !b.txs.is_empty())
        .count();
    assert_eq!(report.hot_rows, hot_with_txs as u64);
    assert_eq!(report.transactions, txs_in(&blocks, 128, 192));
    check_all(&store, &blocks, all_below(pruned));
    // Idempotent.
    let txn = store.begin_write().unwrap();
    assert_eq!(
        ops::prune_all_below(&txn, 100).unwrap(),
        PruneReport {
            state: all_below(pruned),
            ..PruneReport::default()
        }
    );
    drop(txn);
}

/// Stripe pruning with 2 stripes, seed stripe 1: heights `0..4096` and `8192..12288` are kept,
/// `4096..8192` dropped once below the prune height.
#[test]
fn stripe_pruning_keeps_one_stripe() {
    let span = STRIPE_SPAN;
    let blocks = sparse_blocks(0, 2 * span + 200);
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(1_024).unwrap();
    push(&store, &blocks, 500);
    let sealed = store.reader().unwrap().sealed_below();
    // Part of the dropped span is sealed, the rest of it is hot.
    assert!(
        sealed > span && sealed < 2 * span - CHUNK_BLOCKS,
        "sealed below {sealed}"
    );

    let seed = PruningSeed::new(2, 1).unwrap();
    let state = |pruned_below| PruneState { seed, pruned_below };
    store.set_pruning_seed(seed).unwrap();
    check_all(&store, &blocks, state(0));

    // 6000 rounds down to one span, which stripe 1 keeps: nothing is deleted.
    let txn = store.begin_write().unwrap();
    let report = ops::prune_stripe(&txn, seed, 6_000).unwrap();
    txn.commit().unwrap();
    assert_eq!(
        report,
        PruneReport {
            state: state(span),
            ..PruneReport::default()
        }
    );
    check_all(&store, &blocks, state(span));

    // Up to the chain: 8392 rounds down to 8192. The sealed chunks of 4096..sealed lose their
    // signature parts, the hot blocks sealed..8192 their rows; 8192.. stays whole.
    let txn = store.begin_write().unwrap();
    let report = ops::prune_stripe(&txn, seed, u64::MAX).unwrap();
    txn.commit().unwrap();
    assert_eq!(report.state, state(2 * span));
    assert_eq!(report.chunks, (sealed - span) / CHUNK_BLOCKS);
    let hot_with_txs = blocks[sealed as usize..2 * span as usize]
        .iter()
        .filter(|b| !b.txs.is_empty())
        .count();
    assert_eq!(report.hot_rows, hot_with_txs as u64);
    assert_eq!(report.transactions, txs_in(&blocks, span, 2 * span));
    check_all(&store, &blocks, state(2 * span));
    {
        let r = store.reader().unwrap();
        let mut comp = store.compressor().unwrap();
        // Blocks with transactions (16k + 3): the kept stripe and the blocks above the prune
        // height read whole; dropped ones, sealed or hot, are pruned.
        for h in [3, span - 13, 2 * span + 3, 2 * span + 195] {
            assert_eq!(
                r.raw_block(h, &mut comp).unwrap().unwrap(),
                raw_of(&blocks[h as usize])
            );
        }
        for h in [span + 3, sealed + 3, 2 * span - 13] {
            assert!(matches!(r.raw_block(h, &mut comp), Err(Error::Pruned(p)) if p == h));
            assert!(r.block_info(h).unwrap().unwrap().is_pruned());
        }
    }

    // Idempotent; other seeds and the full seed are refused.
    let txn = store.begin_write().unwrap();
    assert_eq!(
        ops::prune_stripe(&txn, seed, u64::MAX).unwrap(),
        PruneReport {
            state: state(2 * span),
            ..PruneReport::default()
        }
    );
    for other in [
        PruningSeed::new(2, 2).unwrap(),
        PruningSeed::new(4, 1).unwrap(),
        PruningSeed::FULL,
        PruningSeed::RECENT_ONLY,
    ] {
        assert!(matches!(
            ops::prune_stripe(&txn, other, u64::MAX),
            Err(Error::Unsupported(_))
        ));
    }
    drop(txn);

    // The chain grows: the hot chunks sealed..8192 are sealed below the prune height without
    // signature parts, the chunks above it with theirs (`check_all` checks both).
    let more = sparse_blocks(blocks.len() as u64, 2 * span + 1_300);
    push(&store, &more, 400);
    let all: Vec<_> = blocks.iter().chain(&more).cloned().collect();
    assert!(store.reader().unwrap().sealed_below() > 2 * span);
    check_all(&store, &all, state(2 * span));

    // The seed survives a reopen; the next bound inside the same span changes nothing.
    drop(store);
    let store = Store::open(&tmp.0).unwrap();
    let report = store
        .prune(PruneMode::Stripe(seed), u64::MAX, &mut |_| {})
        .unwrap();
    assert_eq!(
        report,
        PruneReport {
            state: state(2 * span),
            ..PruneReport::default()
        }
    );

    // Dropping everything takes the kept stripe too, and a stripe cannot come back.
    let top_chunk = all.len() as u64 / CHUNK_BLOCKS * CHUNK_BLOCKS;
    let mut steps = 0;
    let report = store
        .prune(PruneMode::All, u64::MAX, &mut |_| steps += 1)
        .unwrap();
    assert_eq!(report.state, all_below(top_chunk));
    assert!(steps >= 1);
    assert_eq!(
        report.transactions,
        txs_in(&all, 0, span) + txs_in(&all, 2 * span, top_chunk)
    );
    check_all(&store, &all, all_below(top_chunk));
    let txn = store.begin_write().unwrap();
    assert!(matches!(
        ops::prune_stripe(&txn, seed, u64::MAX),
        Err(Error::Unsupported(_))
    ));
    drop(txn);
}

/// Entry count of `table` (a `Reader::table_lens` name).
fn table_len(store: &Store, table: &str) -> u64 {
    store
        .reader()
        .expect("reader")
        .table_lens()
        .expect("table lens")
        .into_iter()
        .find(|(name, _)| *name == table)
        .map(|(_, len)| len)
        .expect("known table")
}

/// A pop below the sealed boundary into chunks whose signatures were dropped (`unseal_chunk`
/// with `chunk_dropped`) brings no signatures back: the unsealed blocks stay pruned (whole
/// reads fail with `Pruned`; blobs, prefixes, outputs and key images stay), re-pushing them
/// stores no hot signature rows and seals the chunk again without signature parts, and blocks
/// appended above the prune height are sealed with theirs.
#[test]
fn reorg_across_dropped_chunks_keeps_them_pruned() {
    let blocks = chain(5 * CHUNK_BLOCKS);
    let first = 3 * CHUNK_BLOCKS as usize + 17;
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).expect("store");
    store.set_seal_lag(5).expect("seal lag");
    push(&store, &blocks[..first], 37);
    assert_eq!(
        store.reader().expect("reader").sealed_below(),
        3 * CHUNK_BLOCKS
    );

    // Every sealed chunk (0..192) loses its signature parts; the hot blocks above stay whole.
    let state = all_below(3 * CHUNK_BLOCKS);
    let txn = store.begin_write().expect("write");
    let report = ops::prune_all_below(&txn, 3 * CHUNK_BLOCKS).expect("prune");
    txn.commit().expect("commit");
    assert_eq!(report.state, state);
    assert_eq!((report.chunks, report.hot_rows), (3, 0));
    assert_eq!(table_len(&store, "sig_parts"), 0);
    check_all(&store, &blocks[..first], state);

    // Pop into chunk 2 (128..192): it is unsealed without signatures.
    let keep = 2 * CHUNK_BLOCKS as usize + 10;
    let popped: Vec<_> = blocks[keep..first].iter().rev().cloned().collect();
    let txn = store.begin_write().expect("write");
    ops::pop_blocks(&txn, &popped).expect("pop into a dropped chunk");
    txn.commit().expect("commit");
    {
        let r = store.reader().expect("reader");
        assert_eq!(r.sealed_below(), 2 * CHUNK_BLOCKS);
        assert_eq!(
            r.prune_state(),
            state,
            "a pop never lowers the prune height"
        );
        for h in 2 * CHUNK_BLOCKS..keep as u64 {
            assert!(r.has_hot_row(h).expect("hot row"), "block {h} is hot again");
            assert!(!r.has_full_block(h), "block {h} stays pruned");
        }
        let mut comp = store.compressor().expect("compressor");
        // Block 130 has two transactions: the block reads as pruned, its parts do not.
        assert!(matches!(
            r.raw_block(130, &mut comp),
            Err(Error::Pruned(130))
        ));
        assert_eq!(
            r.block_blob(130, &mut comp).expect("blob").expect("block"),
            blocks[130].block_blob
        );
        let t = &blocks[130].txs[0];
        assert!(matches!(
            r.transaction_blob(&t.hash, &mut comp),
            Err(Error::Pruned(130))
        ));
        assert_eq!(
            r.tx_prefix_at(130, 1, &mut comp)
                .expect("prefix")
                .expect("transaction"),
            t.blob[..t.prefix_len]
        );
    }
    assert_eq!(table_len(&store, "hot_sigs"), 0);
    assert_eq!(table_len(&store, "sig_parts"), 0);
    check_all(&store, &blocks[..keep], state);

    // Re-push the same blocks: 138..192 get no hot signature rows and chunk 2 is sealed
    // without parts; the blocks above the prune height keep theirs.
    push(&store, &blocks[keep..first], 1_000);
    assert_eq!(
        store.reader().expect("reader").sealed_below(),
        3 * CHUNK_BLOCKS
    );
    let hot_with_txs = blocks[3 * CHUNK_BLOCKS as usize..first]
        .iter()
        .filter(|b| !b.txs.is_empty())
        .count();
    assert_eq!(table_len(&store, "hot_sigs"), hot_with_txs as u64);
    assert_eq!(table_len(&store, "sig_parts"), 0);
    check_all(&store, &blocks[..first], state);

    // The chain grows past the sealed region: chunk 3 (above the prune height) is sealed with
    // its signature parts (`check_all` checks the parts of every sealed chunk).
    push(&store, &blocks[first..], 50);
    assert_eq!(
        store.reader().expect("reader").sealed_below(),
        4 * CHUNK_BLOCKS
    );
    check_all(&store, &blocks, state);
}

/// The same with stripe pruning, both ways: with stripe 2 of 2 the chunks just below the
/// prune height (span 1) are kept and a pop there restores their signatures from the parts;
/// with stripe 1 of 2 they are dropped and stay dropped. Either way re-pushing the blocks
/// leaves the database as the prune did.
#[test]
fn stripe_reorg_below_the_prune_height_follows_the_stripe() {
    let span = STRIPE_SPAN;
    let blocks = sparse_blocks(0, 2 * span + 100);
    let keep = (2 * span - 100) as usize;
    for stripe in [1, 2] {
        let seed = PruningSeed::new(2, stripe).expect("valid seed");
        let state = PruneState {
            seed,
            pruned_below: 2 * span,
        };
        let tmp = TempDb::new();
        let store = Store::open(&tmp.0).expect("store");
        store.set_seal_lag(5).expect("seal lag");
        push(&store, &blocks, 500);
        let txn = store.begin_write().expect("write");
        let report = ops::prune_stripe(&txn, seed, u64::MAX).expect("prune");
        txn.commit().expect("commit");
        assert_eq!(report.state, state);
        check_all(&store, &blocks, state);

        // Pop 8291..=8092: chunks 128, 127 and 126 are unsealed (127 and 126 below the prune
        // height, in span 1).
        let popped: Vec<_> = blocks[keep..].iter().rev().cloned().collect();
        let txn = store.begin_write().expect("write");
        ops::pop_blocks(&txn, &popped).expect("pop below the prune height");
        txn.commit().expect("commit");
        let sealed = keep as u64 / CHUNK_BLOCKS * CHUNK_BLOCKS;
        assert_eq!(store.reader().expect("reader").sealed_below(), sealed);
        let kept = seed.keeps(sealed);
        assert_eq!(kept, stripe == 2, "stripe {stripe}");
        let hot_with_txs = blocks[sealed as usize..keep]
            .iter()
            .filter(|b| !b.txs.is_empty())
            .count() as u64;
        assert!(hot_with_txs > 0);
        assert_eq!(
            table_len(&store, "hot_sigs"),
            if kept { hot_with_txs } else { 0 },
            "stripe {stripe}"
        );
        check_all(&store, &blocks[..keep], state);

        push(&store, &blocks[keep..], 500);
        check_all(&store, &blocks, state);
    }
}

#[test]
fn maintenance_and_recompression_keep_the_data() {
    let blocks = chain(2 * CHUNK_BLOCKS + 5);
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(3).unwrap();
    push(&store, &blocks, 7);

    let opts = MaintainOptions {
        force: true,
        ..MaintainOptions::default()
    };
    let report = store.maintain(&opts, &mut |_| {}).unwrap();
    assert!(
        report
            .repacked
            .iter()
            .any(|(t, n, _)| *t == "outputs" && *n > 0)
    );
    check_all(&store, &blocks, PruneState::default());

    // A raw-content dictionary: every chunk is rewritten and stays readable.
    let dict: Vec<u8> = (0..8192_u32).map(|i| (i % 251) as u8).collect();
    let n = store.recompress(&dict, &mut |_, _| {}).unwrap();
    assert_eq!(n, 2);
    assert_eq!(store.dictionary().unwrap(), Some(dict.clone()));
    check_all(&store, &blocks, PruneState::default());
    // Idempotent.
    assert_eq!(store.recompress(&dict, &mut |_, _| {}).unwrap(), 0);
    // Installing a dictionary is refused once chunks exist.
    assert!(matches!(
        store.set_dictionary(&dict),
        Err(Error::Unsupported(_))
    ));

    // The database reopens (no repack left half-done) and new blocks still seal.
    drop(store);
    let store = Store::open(&tmp.0).unwrap();
    let more: Vec<_> = (blocks.len() as u64..blocks.len() as u64 + CHUNK_BLOCKS)
        .map(|h| block(h, h % 3))
        .collect();
    push(&store, &more, 64);
    let all: Vec<_> = blocks.iter().chain(&more).cloned().collect();
    assert_eq!(store.reader().unwrap().sealed_below(), 3 * CHUNK_BLOCKS);
    check_all(&store, &all, PruneState::default());
}

/// A re-compression that stopped part way is finished by running it again with the same
/// dictionary; a different one is refused until then, and the chunks stay readable throughout.
#[test]
fn recompression_resumes_with_the_same_dictionary() {
    use crate::schema::{META_BLOBS, meta};

    let blocks = chain(2 * CHUNK_BLOCKS + 5);
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(3).unwrap();
    push(&store, &blocks, 7);
    let dict = |m: u32| -> Vec<u8> { (0..8192_u32).map(|i| (i % m) as u8).collect() };
    let (first, second, third) = (dict(251), dict(241), dict(239));
    assert_eq!(store.recompress(&first, &mut |_, _| {}).unwrap(), 2);

    // A run from `first` to `second` that stopped before it rewrote a chunk.
    let txn = store.begin_write().unwrap();
    {
        let mut blobs = txn.open_table(META_BLOBS).unwrap();
        blobs
            .insert(meta::ZSTD_DICTIONARY_PREV, first.as_slice())
            .unwrap();
        blobs
            .insert(meta::ZSTD_DICTIONARY, second.as_slice())
            .unwrap();
    }
    txn.commit().unwrap();
    check_all(&store, &blocks, PruneState::default());

    assert!(matches!(
        store.recompress(&third, &mut |_, _| {}),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(store.dictionary().unwrap(), Some(second.clone()));
    check_all(&store, &blocks, PruneState::default());

    store.recompress(&second, &mut |_, _| {}).unwrap();
    assert_eq!(store.recompress(&second, &mut |_, _| {}).unwrap(), 0);
    check_all(&store, &blocks, PruneState::default());
    // Finished: any dictionary may follow.
    store.recompress(&third, &mut |_, _| {}).unwrap();
    assert_eq!(store.dictionary().unwrap(), Some(third));
    check_all(&store, &blocks, PruneState::default());
}

/// Popping below the sealed boundary unseals whole chunks; a boundary that is not a multiple of
/// the chunk size is reported as a corrupt database.
#[test]
fn pops_below_the_sealed_boundary_need_whole_chunks() {
    use crate::schema::{META, meta};

    let blocks = chain(5);
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    push(&store, &blocks, 5);
    assert_eq!(store.reader().unwrap().sealed_below(), 0);
    for sealed in [5, CHUNK_BLOCKS + 1] {
        let txn = store.begin_write().unwrap();
        txn.open_table(META)
            .unwrap()
            .insert(meta::SEALED_BELOW, sealed)
            .unwrap();
        assert!(
            matches!(
                ops::pop_blocks(&txn, &[blocks[4].clone()]),
                Err(Error::Corrupt(_))
            ),
            "sealed below {sealed}"
        );
        drop(txn);
    }
    // Nothing was written: the chain is unchanged.
    check_all(&store, &blocks, PruneState::default());
}

#[test]
fn alt_blocks_round_trip() {
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    let mut comp = Compressor::new(3).unwrap();
    let raw = RawBlock {
        block: vec![1; 100],
        transactions: vec![vec![2; 10], vec![3; 300]],
    };
    let header = AltBlockHeader {
        height: 7,
        cumulative_difficulty_lo: 9,
        cumulative_difficulty_hi: 0,
        block_size: 400,
    };
    let hash = Hash([9; 32]);

    let txn = store.begin_write().unwrap();
    ops::put_alt_block(&txn, &hash, header, &raw, &mut comp).unwrap();
    txn.commit().unwrap();
    let (hdr, back) = store
        .reader()
        .unwrap()
        .alt_block(&hash, &mut comp)
        .unwrap()
        .unwrap();
    assert_eq!(hdr, header);
    assert_eq!(encode_raw_block(&back), encode_raw_block(&raw));

    let txn = store.begin_write().unwrap();
    assert_eq!(ops::prune_alt_blocks_below(&txn, 8).unwrap(), 1);
    txn.commit().unwrap();
    assert!(
        store
            .reader()
            .unwrap()
            .alt_block(&hash, &mut comp)
            .unwrap()
            .is_none()
    );
}

#[test]
fn schema_version_is_checked() {
    let tmp = TempDb::new();
    drop(Store::open(&tmp.0).unwrap());
    // Reopening the same file works.
    let store = Store::open(&tmp.0).unwrap();
    assert!(store.reader().unwrap().top_height().unwrap().is_none());
    drop(store);

    // A v1 database is refused with a version error (not a table type error).
    let tmp = TempDb::new();
    {
        const V1: redb::TableDefinition<u64, crate::BlockInfo> =
            redb::TableDefinition::new("block_infos");
        let db = redb::Database::create(&tmp.0).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(crate::schema::META)
            .unwrap()
            .insert(crate::schema::meta::SCHEMA_VERSION, 1_u64)
            .unwrap();
        txn.open_table(V1).unwrap();
        txn.commit().unwrap();
    }
    assert!(matches!(
        Store::open(&tmp.0),
        Err(Error::SchemaVersion {
            found: 1,
            expected: 2
        })
    ));
}

// ---------------------------------------------------------------------- mainnet fixtures

fn fixtures() -> Vec<serde_json::Value> {
    serde_json::from_str(include_str!(
        "../../xkr-serialization/tests/fixtures/blocks.json"
    ))
    .expect("fixture json")
}

fn hex_field(v: &serde_json::Value, k: &str) -> Vec<u8> {
    hex::decode(v[k].as_str().unwrap()).unwrap()
}

fn hash_field(v: &serde_json::Value, k: &str) -> [u8; 32] {
    hex_field(v, k).try_into().unwrap()
}

/// XORs `b` with a pad derived from `(seed, n)`.
fn scramble(b: &mut [u8], seed: u64, n: u64) {
    for (i, chunk) in b.chunks_mut(32).enumerate() {
        let pad = rnd32(0x5a, seed.wrapping_mul(1_000_003) ^ (n << 8) ^ i as u64);
        for (x, p) in chunk.iter_mut().zip(pad) {
            *x ^= p;
        }
    }
}

/// The 39 fixture blocks as consecutive heights. `replica > 0` scrambles every key, key image,
/// signature, transaction public key and hash, so replicas do not compress against each other.
fn fixture_blocks(replica: u64, first_height: u64) -> Vec<VerifiedBlock> {
    // Scrambles `b` with the next pad of this replica (replica 0 stays real).
    fn mix(b: &mut [u8], replica: u64, n: &mut u64) {
        if replica > 0 {
            *n += 1;
            scramble(b, replica, *n);
        }
    }
    fn mix_extra(extra: &mut [u8], replica: u64, n: &mut u64) {
        if extra.first() == Some(&1)
            && let Some(k) = extra.get_mut(1..33)
        {
            mix(k, replica, n);
        }
    }
    let mut n = 0_u64;
    let mut out = Vec::new();
    for (i, f) in fixtures().iter().enumerate() {
        let mut block = decode_block(&hex_field(f, "blob")).unwrap();
        let mut hash = hash_field(f, "hash");
        let mut miner_tx_hash = hash_field(f, "miner_tx_hash");
        mix(&mut hash, replica, &mut n);
        mix(&mut miner_tx_hash, replica, &mut n);
        for o in &mut block.miner_tx.prefix.outputs {
            let k = o.key_mut();
            mix(&mut k.0, replica, &mut n);
        }
        mix_extra(&mut block.miner_tx.prefix.extra, replica, &mut n);
        let mut txs = Vec::new();
        for (t, th) in f["txs"]
            .as_array()
            .unwrap()
            .iter()
            .zip(&mut block.tx_hashes)
        {
            let mut tx = decode_transaction(&hex_field(t, "blob")).unwrap();
            for input in &mut tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    mix(&mut key_image.0, replica, &mut n);
                }
            }
            for o in &mut tx.prefix.outputs {
                let k = o.key_mut();
                mix(&mut k.0, replica, &mut n);
            }
            mix_extra(&mut tx.prefix.extra, replica, &mut n);
            for ring in &mut tx.signatures {
                for s in ring {
                    mix(&mut s.0, replica, &mut n);
                }
            }
            mix(&mut th.0, replica, &mut n);
            let blob = encode_transaction(&tx).unwrap();
            let sig_bytes: usize = tx.signatures.iter().map(|s| s.len() * 64).sum();
            txs.push(VerifiedTx {
                hash: *th,
                prefix_len: blob.len() - sig_bytes,
                payment_id: payment_id(&tx.prefix.extra).map(|h| h.0),
                tx,
                blob,
            });
        }
        let block_blob = encode_block(&block).unwrap();
        out.push(VerifiedBlock {
            height: first_height + i as u64,
            hash: Hash(hash),
            block_blob,
            block,
            miner_tx_hash: Hash(miner_tx_hash),
            difficulty: f["difficulty"].as_u64().unwrap(),
            already_generated_coins: f["already_generated_coins"].as_u64().unwrap(),
            block_size: f["cumulative_size"].as_u64().unwrap(),
            txs,
        });
    }
    out
}

#[test]
fn real_blobs_are_strippable() {
    for b in fixture_blocks(0, 0) {
        let outputs: Vec<(u64, [u8; 32])> = b
            .block
            .miner_tx
            .prefix
            .outputs
            .iter()
            .map(|o| {
                let k = o.key();
                (o.amount, k.0)
            })
            .collect();
        assert!(
            strip_output_keys(&b.block_blob, &outputs).is_some(),
            "miner tx of fixture block {}",
            b.height
        );
        for t in &b.txs {
            let outputs: Vec<(u64, [u8; 32])> =
                t.tx.prefix
                    .outputs
                    .iter()
                    .map(|o| {
                        let k = o.key();
                        (o.amount, k.0)
                    })
                    .collect();
            assert!(strip_output_keys(&t.blob[..t.prefix_len], &outputs).is_some());
        }
    }
}

/// Size regression: 5 scrambled replicas of the 39 mainnet fixture blocks (195 blocks, three
/// sealed chunks), then maintenance. The fixtures are signature-heavy (1300 inputs, 389 KB of
/// signatures in 482 KB of blocks; key images, stored in the prefix and in the index, are
/// another 9 %), so unpruned storage stays near the raw size (expected ~1.13); pruned it must
/// shrink to about a third. Bounds are deliberately a little loose; the test prints the measured
/// numbers so they can be tightened.
#[test]
#[allow(clippy::cast_precision_loss)]
fn fixture_size_regression() {
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    store.set_seal_lag(0).unwrap();
    let mut all = Vec::new();
    for r in 0..5 {
        let blocks = fixture_blocks(r, all.len() as u64);
        all.extend(blocks);
    }
    let raw: u64 = all
        .iter()
        .map(|b| (b.block_blob.len() + b.txs.iter().map(|t| t.blob.len()).sum::<usize>()) as u64)
        .sum();
    push(&store, &all, 40);
    check_all_light(&store, &all);

    let opts = MaintainOptions {
        force: true,
        ..MaintainOptions::default()
    };
    store.maintain(&opts, &mut |_| {}).unwrap();
    check_all_light(&store, &all);
    let stats = store.stats().unwrap();
    let tables = stats.table_bytes();
    println!(
        "fixture size: raw {raw} B, tables {tables} B ({:.3} of raw), file {} B",
        tables as f64 / raw as f64,
        stats.file_bytes
    );
    for t in &stats.tables {
        println!(
            "  {:24} {:>8} rows {:>9} B fill {:.2}",
            t.name,
            t.entries,
            t.total_bytes(),
            t.fill()
        );
    }
    assert!(
        tables * 100 <= raw * 125,
        "unpruned tables {tables} B > 1.25 x raw {raw} B"
    );
    assert!(stats.file_bytes <= raw * 2 + (2 << 20), "file too large");

    let txn = store.begin_write().unwrap();
    ops::prune_all_below(&txn, u64::MAX).unwrap();
    txn.commit().unwrap();
    store.maintain(&opts, &mut |_| {}).unwrap();
    let stats = store.stats().unwrap();
    let tables = stats.table_bytes();
    println!(
        "fixture size pruned: tables {tables} B ({:.3} of raw), file {} B",
        tables as f64 / raw as f64,
        stats.file_bytes
    );
    assert!(
        tables * 100 <= raw * 45,
        "pruned tables {tables} B > 0.45 x raw {raw} B"
    );
}

/// Raw blocks of every height come back identical (fixture blocks have arbitrary outputs, so
/// [`check_all`]'s model of the synthetic chain does not apply).
fn check_all_light(store: &Store, blocks: &[VerifiedBlock]) {
    let r = store.reader().unwrap();
    let mut comp = store.compressor().unwrap();
    for b in blocks {
        assert_eq!(
            r.raw_block(b.height, &mut comp).unwrap().unwrap(),
            raw_of(b),
            "fixture block at {}",
            b.height
        );
    }
}

/// `Store::open_observed` reports a resumed repack (and nothing on a clean open).
#[test]
fn open_observed_reports_a_resumed_repack() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::OpenEvent;

    let tmp = TempDb::new();
    drop(Store::open(&tmp.0).unwrap());
    let events = Rc::new(RefCell::new(Vec::<OpenEvent>::new()));
    let observe = |events: &Rc<RefCell<Vec<OpenEvent>>>| {
        let events = events.clone();
        move |e: &OpenEvent| {
            events.borrow_mut().push(e.clone());
            true
        }
    };
    drop(Store::open_observed(&tmp.0, 16 << 20, observe(&events)).unwrap());
    assert!(events.borrow().is_empty());

    // a repack interrupted after it created its temporary table
    {
        let db = redb::Database::create(&tmp.0).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let def = redb::TableDefinition::<(u64, u32), ()>::new("timestamps.repack");
            let _ = txn.open_table(def).unwrap();
        }
        txn.commit().unwrap();
    }
    drop(Store::open_observed(&tmp.0, 16 << 20, observe(&events)).unwrap());
    assert_eq!(
        *events.borrow(),
        [OpenEvent::ResumingRepack {
            table: "timestamps".into()
        }]
    );
}

/// Opens `path` with `Store::open_observed` and returns the store and whether redb checked
/// (repaired) the file on the way.
fn open_noting_repair(path: &std::path::Path) -> (Store, bool) {
    use std::cell::Cell;
    use std::rc::Rc;

    use crate::OpenEvent;

    let repaired = Rc::new(Cell::new(false));
    let seen = repaired.clone();
    let store = Store::open_observed(path, 16 << 20, move |e: &OpenEvent| {
        if matches!(e, OpenEvent::Repair { .. }) {
            seen.set(true);
        }
        true
    })
    .unwrap();
    (store, repaired.get())
}

/// `Store::close` closes the file while other handles to the store still exist (tasks that
/// hold the chain at exit): the next open finds a clean file and skips redb's check, and the
/// remaining handles get `Error::Closed`.
#[test]
fn close_with_handles_left_leaves_a_clean_file() {
    use std::sync::Arc;

    let tmp = TempDb::new();
    let store = Arc::new(Store::open(&tmp.0).unwrap());
    let blocks = chain(6);
    push(&store, &blocks, 3);
    let late = Arc::clone(&store);
    let snapshot = store.reader().unwrap();

    store.close().unwrap();
    assert!(store.is_closed());
    // a read snapshot opened before the close may fail its reads, but never panics
    let _ = snapshot.top_height();
    drop(snapshot);
    assert!(matches!(late.reader(), Err(Error::Closed)));
    assert!(matches!(late.compressor(), Err(Error::Closed)));
    assert!(matches!(late.begin_write(), Err(Error::Closed)));
    assert!(matches!(late.dictionary(), Err(Error::Closed)));
    assert!(matches!(late.stats(), Err(Error::Closed)));
    assert!(matches!(
        late.maintain(&MaintainOptions::default(), &mut |_| {}),
        Err(Error::Closed)
    ));
    assert!(late.reader().unwrap_err().is_closed());
    // closing again does nothing
    late.close().unwrap();

    // `late` (and `store`) are still alive: the file was closed anyway
    let (reopened, repaired) = open_noting_repair(&tmp.0);
    assert!(!repaired, "a closed store must not be checked again");
    assert_eq!(reopened.reader().unwrap().top_height().unwrap(), Some(5));
    check_all_light(&reopened, &blocks);
    drop(late);
    drop(store);
}

/// The test above detects a missing close: a copy of the file taken while the store is open
/// (what a process that exits without closing leaves behind) is checked on open, and keeps
/// every committed block. Unix only: Windows does not let another handle read the open,
/// locked database file.
#[test]
#[cfg(unix)]
fn a_file_left_open_is_checked_on_open() {
    let tmp = TempDb::new();
    let copy = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    let blocks = chain(6);
    push(&store, &blocks, 3);
    std::fs::copy(&tmp.0, &copy.0).unwrap();

    let (reopened, repaired) = open_noting_repair(&copy.0);
    assert!(repaired, "a file left open must be checked");
    assert_eq!(reopened.reader().unwrap().top_height().unwrap(), Some(5));
    check_all_light(&reopened, &blocks);
    reopened.close().unwrap();
    store.close().unwrap();

    // both closed: neither is checked again
    assert!(!open_noting_repair(&tmp.0).1);
    assert!(!open_noting_repair(&copy.0).1);
}

/// A write transaction still open on another thread defers the close to its end; the file is
/// clean afterwards.
#[test]
fn close_during_a_write_transaction_finishes_when_it_ends() {
    let tmp = TempDb::new();
    let store = Store::open(&tmp.0).unwrap();
    let blocks = chain(4);
    push(&store, &blocks[..2], 2);

    let mut comp = store.compressor().unwrap();
    let txn = store.begin_write().unwrap();
    ops::push_blocks(&txn, &blocks[2..], &mut comp).unwrap();
    assert!(matches!(store.close(), Err(Error::CloseDeferred)));
    assert!(matches!(store.reader(), Err(Error::Closed)));
    txn.commit().unwrap();

    let (reopened, repaired) = open_noting_repair(&tmp.0);
    assert!(!repaired, "the deferred close must leave a clean file");
    assert_eq!(reopened.reader().unwrap().top_height().unwrap(), Some(3));
}
