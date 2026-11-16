//! Consensus rules checked against real mainnet data (see `tests/fixtures/README.md`).
//!
//! * `headers.json`: ~1400 block headers in several ranges (1-300, 4950-5060, 186 900-187 100,
//!   439 950-440 050, 619 900-620 100, 699 900-700 100, 799 900-800 100, 2 199 900-2 200 100,
//!   2 616 900-2 617 100); every header whose difficulty window is complete is recomputed.
//! * `../xkr-serialization/tests/fixtures/blocks.json`: full blocks + transactions + node
//!   statistics (`f_block_json`: size median, already generated coins, fees).
//! * `rings.json`: ring member output keys (resolved from the chain) for all transactions of
//!   blocks 5052/5053 and for one input of a transaction in block 2 617 400.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use xkr_consensus::difficulty::difficulty_window;
use xkr_consensus::genesis::MAINNET_GENESIS_HASH;
use xkr_consensus::reward::{get_block_reward, is_fusion_transaction};
use xkr_consensus::validation::{check_block_mixins, validate_block_header_and_coinbase};
use xkr_consensus::{
    BlockTransaction, ChainContext, MAINNET, ResolvedInput, block_major_version_for_height,
    check_proof_of_work, difficulty_for_next_block, validate_block, validate_semantic,
    validate_transaction,
};
use xkr_crypto::{check_hash, check_ring_signature, key_image_in_prime_subgroup};
use xkr_serialization::{
    block_hash, block_long_hash, decode_block, decode_transaction, transaction_hash_from_blob,
    transaction_prefix_hash,
};
use xkr_types::{Block, Hash, PublicKey, Transaction, TxInput};

struct Header {
    timestamp: u64,
    difficulty: u64,
    major_version: u8,
}

fn headers() -> BTreeMap<u32, Header> {
    let v: Vec<Value> = serde_json::from_str(include_str!("fixtures/headers.json")).unwrap();
    v.iter()
        .map(|h| {
            (
                h["height"].as_u64().unwrap() as u32,
                Header {
                    timestamp: h["timestamp"].as_u64().unwrap(),
                    difficulty: h["difficulty"].as_u64().unwrap(),
                    major_version: h["major_version"].as_u64().unwrap() as u8,
                },
            )
        })
        .collect()
}

fn blocks() -> Vec<Value> {
    serde_json::from_str(include_str!(
        "../../xkr-serialization/tests/fixtures/blocks.json"
    ))
    .unwrap()
}

fn rings() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/rings.json")).unwrap()
}

fn genesis() -> Hash {
    MAINNET_GENESIS_HASH.parse().unwrap()
}

fn decode_fixture_block(f: &Value) -> Block {
    decode_block(&hex::decode(f["blob"].as_str().unwrap()).unwrap()).unwrap()
}

fn fixture_txs(f: &Value) -> Vec<(Hash, Vec<u8>, Transaction)> {
    f["txs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let blob = hex::decode(t["blob"].as_str().unwrap()).unwrap();
            let tx = decode_transaction(&blob).unwrap();
            (t["hash"].as_str().unwrap().parse().unwrap(), blob, tx)
        })
        .collect()
}

/// Timestamps and synthetic cumulative difficulties (prefix sums of per-block difficulties;
/// only differences matter to every algorithm) for a height range.
fn window_data(
    h: &BTreeMap<u32, Header>,
    range: std::ops::Range<u32>,
) -> Option<(Vec<u64>, Vec<u64>)> {
    let mut ts = Vec::new();
    let mut cd = Vec::new();
    let mut cum = 0u64;
    for height in range {
        let hd = h.get(&height)?;
        ts.push(hd.timestamp);
        cum += hd.difficulty;
        cd.push(cum);
    }
    Some((ts, cd))
}

#[test]
fn versions_match_chain() {
    for (height, hd) in headers() {
        assert_eq!(
            block_major_version_for_height(&MAINNET, height),
            hd.major_version,
            "height {height}"
        );
    }
    for f in blocks() {
        let height = f["height"].as_u64().unwrap() as u32;
        assert_eq!(
            u64::from(block_major_version_for_height(&MAINNET, height)),
            f["major_version"].as_u64().unwrap()
        );
    }
}

#[test]
fn difficulty_matches_every_header() {
    let h = headers();
    let mut checked = 0;
    let mut by_algo = [0usize; 5];
    for (&height, hd) in &h {
        if height == 0 {
            continue;
        }
        let range = difficulty_window(&MAINNET, height);
        let Some((ts, cd)) = window_data(&h, range) else {
            continue;
        };
        let got = difficulty_for_next_block(&MAINNET, height, &ts, &cd);
        assert_eq!(got, hd.difficulty, "difficulty at height {height}");
        checked += 1;
        let idx = u64::from(height - 1);
        let algo = match idx {
            2_200_000.. => 4,
            800_000.. => 3,
            700_000.. => 2,
            620_000.. => 1,
            _ => 0,
        };
        by_algo[algo] += 1;
    }
    assert!(checked > 1300, "only {checked} headers checked");
    assert!(
        by_algo.iter().all(|n| *n > 50),
        "every algorithm exercised: {by_algo:?}"
    );
}

#[test]
fn rewards_match_chain() {
    for f in blocks() {
        let height = f["height"].as_u64().unwrap();
        if height == 0 {
            continue; // hard-coded genesis coinbase
        }
        let block = decode_fixture_block(&f);
        let (reward, emission) = get_block_reward(
            block.header.major_version,
            f["size_median"].as_u64().unwrap(),
            f["cumulative_size"].as_u64().unwrap(),
            f["already_generated_coins_prev"].as_u64().unwrap(),
            f["total_fee"].as_u64().unwrap(),
        )
        .unwrap();
        assert_eq!(reward, f["reward"].as_u64().unwrap(), "reward at {height}");
        let outputs: u64 = block.miner_tx.prefix.outputs.iter().map(|o| o.amount).sum();
        assert_eq!(outputs, reward, "coinbase outputs at {height}");
        let generated = f["already_generated_coins"].as_u64().unwrap()
            - f["already_generated_coins_prev"].as_u64().unwrap();
        assert_eq!(emission, generated as i64, "emission at {height}");
    }
}

fn check_pow_for(heights: &[u64]) {
    for f in blocks() {
        let height = f["height"].as_u64().unwrap();
        if !heights.contains(&height) {
            continue;
        }
        let block = decode_fixture_block(&f);
        let difficulty = f["difficulty"].as_u64().unwrap();
        let pow = block_long_hash(&block).unwrap();
        assert!(
            check_hash(&pow, difficulty),
            "PoW at {height}: {pow} vs {difficulty}"
        );
        assert!(
            check_proof_of_work(&block, difficulty, &genesis()).is_ok(),
            "merge-mining proof at {height}"
        );
        if difficulty > 1_000 {
            // A wrong algorithm would almost never meet the difficulty; make sure a much harder
            // target is rejected so the positive check is meaningful.
            assert!(
                !check_hash(&pow, u64::MAX),
                "PoW at {height} unexpectedly meets 2^64-1"
            );
        }
    }
}

/// CN-Turtle-Lite v2 (major 5): every v5 block in the fixtures.
#[test]
fn pow_cn_turtle_lite_v2_blocks() {
    let heights: Vec<u64> = blocks()
        .iter()
        .filter(|f| f["major_version"].as_u64() == Some(5))
        .map(|f| f["height"].as_u64().unwrap())
        .collect();
    assert!(heights.len() > 25);
    check_pow_for(&heights);
}

/// CN v0 (major 1-3) and CN-Lite v1 (major 4, only block 4 on mainnet). Slow in debug builds.
#[test]
fn pow_early_versions() {
    check_pow_for(&[1, 2, 3, 4]);
}

#[test]
fn semantic_validation_and_fees_of_all_fixture_transactions() {
    for f in blocks() {
        let height = f["height"].as_u64().unwrap() as u32;
        let mut fees = 0u64;
        let txs = fixture_txs(&f);
        for (hash, blob, tx) in &txs {
            let fee = validate_semantic(tx).unwrap_or_else(|e| panic!("tx {hash}: {e}"));
            fees += fee;
            if fee == 0 {
                assert!(
                    is_fusion_transaction(tx, blob.len(), height),
                    "zero-fee tx {hash} must be a fusion tx"
                );
            }
            for input in &tx.prefix.inputs {
                if let TxInput::Key { key_image, .. } = input {
                    assert!(key_image_in_prime_subgroup(key_image));
                }
            }
        }
        assert_eq!(fees, f["total_fee"].as_u64().unwrap(), "fees at {height}");
        let decoded: Vec<Transaction> = txs.into_iter().map(|t| t.2).collect();
        assert!(
            check_block_mixins(&MAINNET, height, &decoded).is_ok(),
            "mixin at {height}"
        );
    }
}

fn ring_keys(r: &Value) -> Vec<Option<Vec<PublicKey>>> {
    r["ring_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            k.as_array().map(|keys| {
                keys.iter()
                    .map(|x| x.as_str().unwrap().parse().unwrap())
                    .collect()
            })
        })
        .collect()
}

#[test]
fn real_ring_signatures_verify() {
    let blocks = blocks();
    let mut verified = 0;
    for r in rings() {
        let height = r["height"].as_u64().unwrap();
        let f = blocks
            .iter()
            .find(|b| b["height"].as_u64() == Some(height))
            .unwrap();
        let wanted: Hash = r["tx_hash"].as_str().unwrap().parse().unwrap();
        let (_, blob, tx) = fixture_txs(f).into_iter().find(|t| t.0 == wanted).unwrap();
        assert_eq!(transaction_hash_from_blob(&blob), wanted);
        let prefix_hash = transaction_prefix_hash(&tx.prefix);
        let other = xkr_crypto::cn_fast_hash(b"not the prefix");
        for (i, keys) in ring_keys(&r).iter().enumerate() {
            let Some(keys) = keys else { continue };
            let TxInput::Key { key_image, .. } = &tx.prefix.inputs[i] else {
                panic!("coinbase input")
            };
            let sigs = &tx.signatures[i];
            assert!(
                check_ring_signature(&prefix_hash, key_image, keys, sigs),
                "tx {wanted} input {i}"
            );
            assert!(!check_ring_signature(&other, key_image, keys, sigs));
            verified += 1;
        }
    }
    assert!(verified > 500, "{verified}");
}

/// Full `Core::addBlock` pipeline for blocks 5052 and 5053, with every ring member resolved.
#[test]
fn full_block_validation_5052_5053() {
    let h = headers();
    let all_rings = rings();
    for height in [5052u32, 5053] {
        let f = blocks()
            .into_iter()
            .find(|b| b["height"].as_u64() == Some(u64::from(height)))
            .unwrap();
        let block = decode_fixture_block(&f);
        let hash = block_hash(&block).unwrap();
        let txs = fixture_txs(&f);

        // ring keys per tx per input
        let keys: Vec<Vec<Vec<PublicKey>>> = txs
            .iter()
            .map(|(th, _, _)| {
                let r = all_rings
                    .iter()
                    .find(|r| r["tx_hash"].as_str() == Some(th.to_string().as_str()))
                    .unwrap_or_else(|| panic!("ring keys for {th}"));
                ring_keys(r).into_iter().map(Option::unwrap).collect()
            })
            .collect();
        let resolved: Vec<Vec<ResolvedInput<'_>>> = keys
            .iter()
            .map(|per_tx| {
                per_tx
                    .iter()
                    .map(|k| ResolvedInput {
                        key_image_spent: false,
                        ring: Ok(k.as_slice()),
                    })
                    .collect()
            })
            .collect();
        let entries: Vec<BlockTransaction<'_>> = txs
            .iter()
            .zip(&resolved)
            .map(|((th, blob, tx), res)| BlockTransaction {
                tx,
                hash: *th,
                prefix_hash: transaction_prefix_hash(&tx.prefix),
                blob_size: blob.len(),
                inputs: res,
            })
            .collect();

        let recent: Vec<u64> = (height - 60..height).map(|x| h[&x].timestamp).collect();
        let (dts, dcd) = window_data(&h, difficulty_window(&MAINNET, height)).unwrap();
        let sizes = [f["size_median"].as_u64().unwrap()];
        let ctx = ChainContext {
            params: &MAINNET,
            height,
            prev_hash: block.header.prev_hash,
            recent_timestamps: &recent,
            difficulty_timestamps: &dts,
            difficulty_cumulative: &dcd,
            recent_block_sizes: &sizes,
            already_generated_coins: f["already_generated_coins_prev"].as_u64().unwrap(),
            adjusted_time: block.header.timestamp,
            genesis_hash: genesis(),
            in_checkpoint_zone: false,
            checkpoint: None,
        };
        let accepted = validate_block(&block, &hash, &entries, &ctx)
            .unwrap_or_else(|e| panic!("block {height}: {e}"));
        assert_eq!(accepted.difficulty, f["difficulty"].as_u64().unwrap());
        assert_eq!(accepted.reward, f["reward"].as_u64().unwrap());
        assert_eq!(
            accepted.cumulative_size,
            f["cumulative_size"].as_u64().unwrap()
        );
        assert_eq!(accepted.fees, f["total_fee"].as_u64().unwrap());

        // A double spend inside the block is caught by the block-wide key-image set.
        let mut seen = HashSet::new();
        assert!(validate_transaction(&entries[0], height - 1, false, &mut seen).is_ok());
        assert!(validate_transaction(&entries[0], height - 1, false, &mut seen).is_err());

        // Timestamp rules.
        let mut late = ctx;
        late.adjusted_time = block.header.timestamp - 7201;
        assert!(validate_block_header_and_coinbase(&block, &late).is_err());
        let mut early = block.clone();
        early.header.timestamp = 0;
        assert!(validate_block_header_and_coinbase(&early, &ctx).is_err());
    }
}
