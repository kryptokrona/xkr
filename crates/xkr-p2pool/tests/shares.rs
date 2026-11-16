//! Real mainnet shares of the Kryptokrona sidechain (default config), as a C++ p2pool node
//! stored them in its block cache (`p2pool.cache` slots: main-chain part then sidechain part):
//!
//! * `data/shares-mainnet.jsonl`: 61 consecutive shares (sidechain heights 27 180 to 27 236),
//!   with shares that carry one and two uncles and their sibling shares;
//! * `data/shares-sizes.jsonl`: the smallest and the largest share of the cache.
//!
//! Every share must parse, re-serialize to the same bytes, carry its own id as the coinbase
//! merkle root, meet its difficulty, and agree with a second implementation of the block format
//! (`xkr-serialization`). The chain file is checked for self-consistency: parents and uncles
//! are found by id, heights and cumulative difficulties add up, and every tx key seed follows
//! from its parent.

use std::collections::HashMap;

use xkr_consensus::Network;
use xkr_p2pool::block::{EXTRA_NONCE_SIZE, MAX_BLOCK_SIZE};
use xkr_p2pool::parse::{ParseError, SidechainView, StaticView};
use xkr_p2pool::varint::varint_len;
use xkr_p2pool::{
    Difficulty, ForkHeights, PoolBlock, SidechainConfig, parse_compact_pool_block, parse_pool_block,
};
use xkr_serialization::{aux_block_header_hash, block_pow_blob, decode_block, transaction_hash};
use xkr_types::Hash;

/// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` on mainnet.
const UNLOCK_WINDOW: u64 = 20;

fn load(name: &str) -> Vec<Vec<u8>> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            hex::decode(v["serialized"].as_str().unwrap()).unwrap()
        })
        .collect()
}

fn mainnet_view() -> StaticView {
    StaticView {
        consensus_id: SidechainConfig::main().consensus_id(),
        forks: ForkHeights::from_params(Network::Mainnet.params()),
    }
}

fn all_blobs() -> Vec<Vec<u8>> {
    let mut v = load("shares-mainnet.jsonl");
    v.extend(load("shares-sizes.jsonl"));
    v
}

fn parse(blob: &[u8]) -> PoolBlock {
    parse_pool_block(blob, &mainnet_view()).unwrap()
}

#[test]
fn every_share_parses_and_round_trips() {
    let view = mainnet_view();
    let blobs = all_blobs();
    assert_eq!(blobs.len(), 63);
    let mut with_transactions = 0;
    for blob in &blobs {
        let b = parse_pool_block(blob, &view).unwrap();
        assert_eq!(b.serialize(UNLOCK_WINDOW), *blob);
        let main = b.serialize_mainchain_data(UNLOCK_WINDOW, b.nonce, b.extra_nonce);
        assert_eq!(blob[..main.bytes.len()], main.bytes[..]);
        assert_eq!(blob[main.bytes.len()..], b.serialize_sidechain_data()[..]);

        // The id: over the received bytes, as a template computes it, and in the coinbase.
        assert_eq!(b.merkle_root, b.sidechain_id);
        assert_eq!(
            b.calculate_sidechain_id(UNLOCK_WINDOW, &view.consensus_id),
            b.sidechain_id
        );
        assert_eq!(&blob[main.merkle_root_offset..][..32], &b.sidechain_id.0);
        assert_eq!(
            blob[main.extra_nonce_offset..][..EXTRA_NONCE_SIZE],
            b.extra_nonce.to_le_bytes()
        );
        assert_eq!(blob[main.nonce_offset..][..4], b.nonce.to_le_bytes());

        // What the live sidechain writes today.
        assert_eq!(b.major_version, 5);
        assert_eq!(b.minor_version, 16);
        assert_eq!(b.unlock_time, b.txin_gen_height + UNLOCK_WINDOW);
        assert_eq!(b.software_id(), xkr_p2pool::SOFTWARE_ID_P2POOL);
        assert_eq!(b.software_version(), 2);
        assert!(b.merkle_proof.is_empty() && b.merge_mining_extra.is_empty());
        assert!(b.outputs.iter().all(|o| o.view_tag == 0));
        assert!(b.total_reward() > 0);
        assert_eq!(b.outputs.len(), b.eph_public_keys.len());
        assert!(xkr_p2pool::keys::check_keys(&b.txkey_pub, &b.txkey_sec));
        assert_eq!(
            xkr_p2pool::keys::get_tx_keys(&b.txkey_sec_seed, &b.prev_id).0,
            b.txkey_pub
        );
        let full = b.full_id();
        assert_eq!(full[..32], b.sidechain_id.0);
        assert_eq!(full[32..36], b.nonce.to_le_bytes());
        assert_eq!(full[36..], b.extra_nonce.to_le_bytes());
        if b.transactions.len() > 1 {
            with_transactions += 1;
        }

        // The main-chain part is a Kryptokrona block: cross-check with xkr-serialization.
        let block = decode_block(&main.bytes).unwrap();
        let pow = b.pow_blob(UNLOCK_WINDOW, b.extra_nonce, b.nonce);
        assert_eq!(block_pow_blob(&block).unwrap(), pow.blob);
        assert_eq!(pow.nonce_offset, 2 + varint_len(b.timestamp) + 32);
        assert_eq!(
            transaction_hash(&block.miner_tx).unwrap(),
            pow.coinbase_hash
        );
        assert_eq!(
            aux_block_header_hash(&block).unwrap(),
            b.aux_hash(&pow.coinbase_hash)
        );
        assert_eq!(block.header.prev_hash, b.prev_id);
        assert_eq!(block.header.timestamp, b.timestamp);
        assert_eq!(block.header.nonce, b.nonce);
        assert_eq!(block.tx_hashes[..], b.transactions[1..]);
        assert_eq!(
            &blob[main.aux_hash_offset..][..32],
            &b.aux_hash(&pow.coinbase_hash).0
        );
    }
    assert!(
        with_transactions > 0,
        "no share with transactions in the sample"
    );
}

#[test]
fn proof_of_work_meets_the_share_difficulty() {
    for blob in all_blobs() {
        let b = parse(&blob);
        let pow = b.pow_hash(UNLOCK_WINDOW);
        assert!(
            b.difficulty.check_pow(&pow),
            "share {} at height {}: pow {pow} above difficulty {}",
            b.sidechain_id,
            b.sidechain_height,
            b.difficulty
        );
    }
}

#[test]
fn sample_chain_is_self_consistent() {
    let blocks: Vec<PoolBlock> = load("shares-mainnet.jsonl")
        .iter()
        .map(Vec::as_slice)
        .map(parse)
        .collect();
    let by_id: HashMap<Hash, &PoolBlock> = blocks.iter().map(|b| (b.sidechain_id, b)).collect();
    assert_eq!(by_id.len(), blocks.len(), "duplicate share ids");
    let lowest = blocks.iter().map(|b| b.sidechain_height).min().unwrap();
    let (mut parents, mut uncles, mut new_seeds) = (0, 0, 0);
    for b in &blocks {
        if b.sidechain_height == lowest {
            continue;
        }
        let parent = by_id
            .get(&b.parent)
            .unwrap_or_else(|| panic!("parent of share at height {} missing", b.sidechain_height));
        parents += 1;
        assert_eq!(b.sidechain_height, parent.sidechain_height + 1);

        // Uncles: present, ascending in hash order, at most 3 below, never the parent.
        let mut cumulative = parent.cumulative_difficulty.wrapping_add(b.difficulty);
        for pair in b.uncles.windows(2) {
            assert!(xkr_p2pool::hash_less(&pair[0], &pair[1]));
        }
        for id in &b.uncles {
            let uncle = by_id.get(id).expect("uncle in the sample");
            assert_ne!(*id, b.parent);
            assert!(uncle.sidechain_height < b.sidechain_height);
            assert!(uncle.sidechain_height + 3 >= b.sidechain_height);
            cumulative = cumulative.wrapping_add(uncle.difficulty);
            uncles += 1;
        }
        assert_eq!(
            b.cumulative_difficulty, cumulative,
            "height {}",
            b.sidechain_height
        );

        // Tx key seed: kept while the main-chain block stays the same, otherwise derived from
        // the parent share.
        if b.prev_id == parent.prev_id {
            assert_eq!(b.txkey_sec_seed, parent.txkey_sec_seed);
        } else {
            assert_eq!(
                b.txkey_sec_seed,
                parent.calculate_tx_key_seed(UNLOCK_WINDOW),
                "height {}",
                b.sidechain_height
            );
            new_seeds += 1;
        }
    }
    assert!(parents >= 55, "{parents} parents found");
    assert!(uncles >= 4, "{uncles} uncles found");
    assert!(new_seeds >= 1, "no main-chain block change in the sample");
}

/// The bytes outside the id (nonce, aux hash, extra nonce, `sidechainExtraBuf[3]`) can change
/// without changing the id; any other change breaks the coinbase merkle root.
#[test]
fn id_regions() {
    let view = mainnet_view();
    let blob = load("shares-mainnet.jsonl").remove(0);
    let b = parse(&blob);
    let main = b.serialize_mainchain_data(UNLOCK_WINDOW, b.nonce, b.extra_nonce);
    let n = blob.len();
    for (offset, len) in [
        (main.nonce_offset, 4),
        (main.aux_hash_offset, 32),
        (main.extra_nonce_offset, 4),
        (n - 4, 4),
    ] {
        let mut m = blob.clone();
        for x in &mut m[offset..offset + len] {
            *x ^= 0x5a;
        }
        let parsed = parse_pool_block(&m, &view).unwrap();
        assert_eq!(parsed.sidechain_id, b.sidechain_id);
        // The "already received" key tells apart shares that differ in nonce or extra nonce.
        let mined = offset == main.nonce_offset || offset == main.extra_nonce_offset;
        assert_eq!(parsed.full_id() != b.full_id(), mined);
    }
    // sidechainExtraBuf[2] is part of the id.
    let mut m = blob.clone();
    m[n - 8] ^= 1;
    assert_eq!(parse_pool_block(&m, &view), Err(ParseError::MerkleRoot));
    // So is the merkle root itself compared with it.
    let mut m = blob.clone();
    m[main.merkle_root_offset] ^= 1;
    assert_eq!(parse_pool_block(&m, &view), Err(ParseError::MerkleRoot));
    // Another sidechain gets another id.
    let mini = StaticView {
        consensus_id: SidechainConfig::mini().consensus_id(),
        ..view
    };
    assert_eq!(parse_pool_block(&blob, &mini), Err(ParseError::MerkleRoot));
}

/// One case per parser rule that a byte edit of a real share can reach.
#[test]
fn parser_refuses_malformed_shares() {
    let view = mainnet_view();
    let blob = load("shares-mainnet.jsonl").remove(0);
    let b = parse(&blob);
    let main = b.serialize_mainchain_data(UNLOCK_WINDOW, b.nonce, b.extra_nonce);
    let side = main.bytes.len();
    let n = blob.len();
    let edit = |at: usize, value: u8| {
        let mut m = blob.clone();
        m[at] = value;
        parse_pool_block(&m, &view)
    };

    assert_eq!(edit(0, 17), Err(ParseError::MajorVersion(17)));
    assert_eq!(
        edit(0, 4),
        Err(ParseError::MajorVersionForHeight {
            major: 4,
            height: b.txin_gen_height,
            expected: 5
        })
    );
    assert_eq!(edit(1, 128), Err(ParseError::MinorVersion(128)));
    // Parent block major version must be 0.
    assert_eq!(edit(34, 1), Err(ParseError::UnexpectedByte(34)));
    // Parent transaction count 1, parent coinbase zeros.
    assert_eq!(
        edit(main.nonce_offset + 4, 2),
        Err(ParseError::UnexpectedByte(main.nonce_offset + 4))
    );
    // Merge-mining tag depth.
    assert_eq!(
        edit(main.aux_hash_offset - 1, 1),
        Err(ParseError::MergeMiningDepth)
    );
    // Coinbase version 2 needs major version 7.
    assert_eq!(
        edit(main.aux_hash_offset + 32, 2),
        Err(ParseError::UnexpectedByte(main.aux_hash_offset + 32))
    );
    // Output count 0 (a pruned blob).
    let outputs_at = main.aux_hash_offset
        + 32
        + 1
        + varint_len(b.unlock_time)
        + 2
        + varint_len(b.txin_gen_height);
    if b.outputs.len() < 0x80 {
        assert_eq!(usize::from(blob[outputs_at]), b.outputs.len());
        assert_eq!(edit(outputs_at, 0), Err(ParseError::Pruned));
    }
    // Extra nonce field size.
    assert_eq!(
        edit(main.extra_nonce_offset - 1, 37),
        Err(ParseError::ExtraNonceSize)
    );
    // Miner spend key not a point (y = 2^255 - 1).
    let mut m = blob.clone();
    m[side..side + 32].fill(0xff);
    m[side + 31] = 0x7f;
    assert_eq!(parse_pool_block(&m, &view), Err(ParseError::MinerWallet));
    // Another tx key seed gives another coinbase key.
    assert_eq!(edit(side + 64, blob[side + 64] ^ 1), Err(ParseError::TxKey));
    // Uncle count above 5.
    assert_eq!(edit(side + 128, 6), Err(ParseError::TooManyUncles));
    // Merkle proof longer than 8 (proof count byte, then the merge-mining count, then 16 bytes).
    assert_eq!(edit(n - 18, 9), Err(ParseError::MerkleProofSize));
    // A merge-mining entry needs at least 33 bytes.
    assert!(matches!(edit(n - 17, 1), Err(ParseError::Truncated(_))));
    // Truncated and trailing bytes.
    assert!(matches!(
        parse_pool_block(&blob[..n - 1], &view),
        Err(ParseError::Truncated(_))
    ));
    let mut m = blob.clone();
    m.push(0);
    assert_eq!(parse_pool_block(&m, &view), Err(ParseError::TrailingBytes));
    // Size limit.
    assert_eq!(
        parse_pool_block(
            &std::iter::repeat_n(5u8, MAX_BLOCK_SIZE + 1).collect::<Vec<u8>>(),
            &view
        ),
        Err(ParseError::TooLarge)
    );
    // A non-canonical varint (timestamp with a zero continuation byte).
    let mut m = blob[..36].to_vec();
    m.extend_from_slice(&[0x80, 0x00]);
    assert_eq!(parse_pool_block(&m, &view), Err(ParseError::Varint(36)));
}

/// A view that knows the parent's next difficulty and transactions.
struct Known {
    inner: StaticView,
    next_difficulty: Option<Difficulty>,
    transactions: Vec<Hash>,
}

impl SidechainView for Known {
    fn consensus_id(&self) -> &Hash {
        &self.inner.consensus_id
    }
    fn fork_heights(&self) -> ForkHeights {
        self.inner.forks
    }
    fn cached_next_difficulty(&self, _parent: &Hash) -> Option<Difficulty> {
        self.next_difficulty
    }
    fn transactions_of(&self, _id: &Hash) -> Option<Vec<Hash>> {
        Some(self.transactions.clone())
    }
}

#[test]
fn parent_difficulty_and_compact_transactions() {
    let blobs = all_blobs();
    let (blob, b) = blobs
        .iter()
        .map(|blob| (blob, parse(blob)))
        .max_by_key(|(_, b)| b.transactions.len())
        .unwrap();
    assert!(b.transactions.len() > 1);
    let known = |d: Option<Difficulty>| Known {
        inner: mainnet_view(),
        next_difficulty: d,
        transactions: b.transactions.clone(),
    };
    assert!(parse_pool_block(blob, &known(Some(b.difficulty))).is_ok());
    assert!(parse_pool_block(blob, &known(Some(Difficulty::ZERO))).is_ok());
    assert_eq!(
        parse_pool_block(blob, &known(Some(b.difficulty.wrapping_add(Difficulty(1))))),
        Err(ParseError::Difficulty)
    );

    // Compact form: transaction i as "index i of the parent's list", or as "0 + hash".
    let main = b.serialize_mainchain_data(UNLOCK_WINDOW, b.nonce, b.extra_nonce);
    let count = b.transactions.len() - 1;
    let tx_start = main.bytes.len() - count * 32;
    for by_index in [true, false] {
        let mut compact = blob[..tx_start].to_vec();
        for (i, h) in b.transactions.iter().enumerate().skip(1) {
            if by_index {
                xkr_p2pool::varint::write_varint(&mut compact, i as u64);
            } else {
                compact.push(0);
                compact.extend_from_slice(&h.0);
            }
        }
        compact.extend_from_slice(&blob[main.bytes.len()..]);
        let c = parse_compact_pool_block(&compact, &known(None)).unwrap();
        assert_eq!(c.sidechain_id, b.sidechain_id);
        assert_eq!(c.transactions, b.transactions);
        assert_eq!(c.serialize(UNLOCK_WINDOW), *blob);
        // Without the parent there is nothing to expand.
        assert_eq!(
            parse_compact_pool_block(&compact, &mainnet_view()),
            Err(ParseError::CompactParentUnknown)
        );
    }
}

/// Every output of a share whose PPLNS window lies inside the sample pays one of the sample's
/// miners: its one-time key derives from the share's coinbase secret key, the output index and
/// one of the miner wallets.
#[test]
fn outputs_pay_miners_of_the_window() {
    let blocks: Vec<PoolBlock> = load("shares-mainnet.jsonl")
        .iter()
        .map(Vec::as_slice)
        .map(parse)
        .collect();
    let mut wallets: Vec<xkr_p2pool::MinerWallet> = blocks.iter().map(|b| b.miner_wallet).collect();
    wallets.sort();
    wallets.dedup();
    let mut checked = 0;
    for b in blocks.iter().skip(40) {
        for (i, key) in b.eph_public_keys.iter().enumerate() {
            let payee = wallets.iter().find(|w| {
                xkr_p2pool::keys::eph_public_key(w, &b.txkey_sec, i as u64)
                    .is_some_and(|(k, _)| k == *key)
            });
            assert!(
                payee.is_some(),
                "output {i} of share at height {} pays no sample miner",
                b.sidechain_height
            );
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked} outputs checked");
}
