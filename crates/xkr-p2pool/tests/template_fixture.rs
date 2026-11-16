//! Templates rebuilt from real mainnet shares, byte for byte.
//!
//! For a share whose parent and PPLNS window are held, a template is built with the share's
//! own main-chain fields (versions, previous block, height, timestamp), its miner wallet, its
//! transaction list and reward and its sidechain extra words; with the nonce and extra nonce
//! the miner found, the template's share must serialize to exactly the bytes the C++ p2pool
//! sent. Everything else comes from the template code: tx key seed and keys, cumulative
//! difficulty, PPLNS window, reward split, outputs, coinbase, aux hash, share id.
//!
//! [`fixture_shares_rebuilt`] uses the fixture shares in `tests/data` (no chain tip there, so
//! the parent is given and the difficulty and uncles are the share's). The ignored
//! [`full_cache_rebuilt`] replays all 4 608 shares of the local vector set
//! (`XKR_P2POOL_VECTORS_DIR`) height by height and builds each share on the sidechain tip with
//! the full template path: uncle choice and difficulty included.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;
use xkr_consensus::Network;
use xkr_p2pool::keys::get_tx_keys;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView};
use xkr_p2pool::template::{ShareSpec, build_share, finish_share, mined_share};
use xkr_p2pool::{
    Difficulty, ForkHeights, PoolBlock, SideChain, SidechainConfig, SidechainOptions, StaticView,
    parse_pool_block,
};
use xkr_types::Hash;

/// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` on mainnet.
const UNLOCK_WINDOW: u64 = 20;
/// Local time used for the replay.
const NOW: u64 = 1_800_000_000;

/// Main-chain difficulty per seed height, nothing else.
struct SeedDifficulties(BTreeMap<u64, Difficulty>);

impl MainChainView for SeedDifficulties {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        self.0.get(&height).map(|d| ChainMain {
            difficulty: *d,
            height,
            ..ChainMain::default()
        })
    }
    fn block_by_hash(&self, _id: &Hash) -> Option<ChainMain> {
        None
    }
    fn tip(&self) -> MainChainTip {
        MainChainTip::default()
    }
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn options() -> SidechainOptions {
    SidechainOptions::new(
        SidechainConfig::main(),
        ForkHeights::from_params(Network::Mainnet.params()),
        UNLOCK_WINDOW,
    )
}

fn blobs_of(rows: &[Value]) -> Vec<Vec<u8>> {
    rows.iter()
        .map(|r| hex::decode(r["serialized"].as_str().unwrap()).unwrap())
        .collect()
}

/// The shares with their bytes, ordered by height (stable).
fn parse_all(blobs: Vec<Vec<u8>>, sc: &SideChain) -> Vec<(PoolBlock, Vec<u8>)> {
    let view = StaticView {
        consensus_id: *sc.consensus_id(),
        forks: sc.options().forks,
    };
    let mut shares: Vec<(PoolBlock, Vec<u8>)> = blobs
        .into_iter()
        .map(|b| (parse_pool_block(&b, &view).unwrap(), b))
        .collect();
    shares.sort_by_key(|(b, _)| b.sidechain_height);
    shares
}

/// The node and miner fields of a real share.
fn spec_of(b: &PoolBlock) -> ShareSpec {
    ShareSpec {
        major_version: b.major_version,
        minor_version: b.minor_version,
        prev_id: b.prev_id,
        height: b.txin_gen_height,
        timestamp: b.timestamp,
        wallet: b.miner_wallet,
        software_id: b.sidechain_extra[0],
        software_version: b.sidechain_extra[1],
        random: b.sidechain_extra[2],
    }
}

/// The sidechain part of a share on `parent` as `fill_sidechain_data` writes it, with the
/// difficulty and uncles given: tx key seed (the parent's while the main-chain block is the
/// same, else derived from the parent), keys, parent, height, cumulative difficulty. `None`
/// if an uncle is not held.
fn sidechain_part(
    sc: &SideChain,
    block: &mut PoolBlock,
    parent: &PoolBlock,
    difficulty: Difficulty,
    uncles: &[Hash],
) -> Option<()> {
    block.txkey_sec_seed = if block.prev_id == parent.prev_id {
        parent.txkey_sec_seed
    } else {
        parent.calculate_tx_key_seed(UNLOCK_WINDOW)
    };
    (block.txkey_pub, block.txkey_sec) = get_tx_keys(&block.txkey_sec_seed, &block.prev_id);
    block.parent = parent.sidechain_id;
    block.sidechain_height = parent.sidechain_height + 1;
    block.uncles = uncles.to_vec();
    block.difficulty = difficulty;
    block.cumulative_difficulty = parent.cumulative_difficulty.wrapping_add(difficulty);
    for id in uncles {
        let u = sc.find_block(id)?;
        block.cumulative_difficulty = block.cumulative_difficulty.wrapping_add(u.difficulty);
    }
    Some(())
}

#[test]
fn fixture_shares_rebuilt() {
    let rows = read_jsonl(&data_dir().join("cpp-expected.jsonl"));
    let rows = &rows[1..];
    let mut seeds = BTreeMap::new();
    for r in rows {
        if let Some(d) = r.get("main_chain_difficulty").and_then(Value::as_str) {
            seeds.insert(
                r["seed_height"].as_u64().unwrap(),
                Difficulty(d.parse().unwrap()),
            );
        }
    }
    let main = SeedDifficulties(seeds);

    let mut blobs = Vec::new();
    for name in ["shares-mainnet.jsonl", "shares-sizes.jsonl"] {
        blobs.extend(blobs_of(&read_jsonl(&data_dir().join(name))));
    }
    let mut sc = SideChain::new(options());
    let shares = parse_all(blobs, &sc);
    for (b, _) in &shares {
        sc.add_block(b.clone(), &main, NOW).unwrap();
    }

    let mut compared = 0;
    let mut with_uncles = 0;
    let mut with_transactions = 0;
    for (share, bytes) in &shares {
        let h = share.sidechain_height;
        let Some(parent) = sc.find_block(&share.parent) else {
            continue;
        };
        let mut t = spec_of(share).new_block(UNLOCK_WINDOW);
        if sidechain_part(&sc, &mut t, parent, share.difficulty, &share.uncles).is_none() {
            continue;
        }
        // the window must lie inside the fixture
        let Ok(window) = sc.get_shares(&t, &main) else {
            continue;
        };
        finish_share(
            &mut t,
            &window.shares,
            share.total_reward(),
            &share.transactions[1..],
            UNLOCK_WINDOW,
            sc.consensus_id(),
        )
        .unwrap();
        assert_eq!(t.sidechain_id, share.sidechain_id, "id at height {h}");
        assert_eq!(
            t.merkle_root, share.merkle_root,
            "merkle root at height {h}"
        );
        let mined = mined_share(&t, share.nonce, share.extra_nonce);
        assert_eq!(
            mined.serialize(UNLOCK_WINDOW),
            *bytes,
            "bytes at height {h}"
        );
        assert_eq!(mined, *share, "fields at height {h}");
        compared += 1;
        with_uncles += usize::from(!share.uncles.is_empty());
        with_transactions += usize::from(share.transactions.len() > 1);
    }
    eprintln!(
        "fixture: {compared} shares rebuilt, {with_uncles} with uncles, \
         {with_transactions} with transactions"
    );
    // every share whose PPLNS window lies inside the fixture (the same 35 as the sidechain
    // test walks)
    assert_eq!(compared, 35);
    assert!(with_uncles > 0);
}

/// The full vector set's directory from `XKR_P2POOL_VECTORS_DIR`; `None` (with a note) when
/// the variable is not set, so the ignored tests skip.
fn vectors_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XKR_P2POOL_VECTORS_DIR").map(PathBuf::from);
    if dir.is_none() {
        eprintln!("skipped: set XKR_P2POOL_VECTORS_DIR to the full p2pool vector set");
    }
    dir
}

/// All shares of node a's block cache added height by height to one sidechain. Before the
/// shares of each height are added, every share of that height whose parent is the current tip
/// is rebuilt with the template path ([`build_share`]: `fill_sidechain_data` on the tip, then
/// the coinbase) from its own node and miner fields, transactions and reward. Once the chain is
/// deep enough to have a tip, every such share must come out byte for byte, except where the
/// sidechain here knows a share at an uncle height that the share's node did not know when it
/// built the share (a share received later), so the uncle lists differ.
#[test]
#[ignore = "needs the full vector set (XKR_P2POOL_VECTORS_DIR)"]
fn full_cache_rebuilt() {
    let Some(dir) = vectors_dir() else {
        return;
    };
    let seeds: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("seed-diffs.json")).unwrap())
            .unwrap();
    let main = SeedDifficulties(
        seeds["seeds"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(h, v)| {
                (
                    h.parse::<u64>().unwrap(),
                    Difficulty::from_u64(v["difficulty"].as_u64().unwrap()),
                )
            })
            .collect(),
    );
    let blobs = blobs_of(&read_jsonl(&dir.join("cache-blocks-a.jsonl")));
    assert_eq!(blobs.len(), 4608);

    let mut sc = SideChain::new(options());
    let shares = parse_all(blobs, &sc);
    let mut by_height: BTreeMap<u64, Vec<&(PoolBlock, Vec<u8>)>> = BTreeMap::new();
    for s in &shares {
        by_height.entry(s.0.sidechain_height).or_default().push(s);
    }

    let mut compared = 0usize;
    let mut uncles_differ = 0usize;
    let mut not_on_tip = 0usize;
    let mut first_height = None;
    let mut reasons: HashMap<&'static str, usize> = HashMap::new();
    for (h, group) in &by_height {
        if let Some(tip) = sc.chain_tip().map(|t| t.sidechain_id) {
            for (share, bytes) in group {
                if share.parent != tip {
                    not_on_tip += 1;
                    continue;
                }
                let (t, _) = build_share(
                    &spec_of(share),
                    &sc,
                    &main,
                    &share.transactions[1..],
                    share.total_reward(),
                )
                .unwrap_or_else(|e| panic!("template at height {h}: {e}"));
                if t.uncles != share.uncles {
                    uncles_differ += 1;
                    let reason = if t.uncles.len() > share.uncles.len() {
                        "more uncles here"
                    } else {
                        "fewer uncles here"
                    };
                    *reasons.entry(reason).or_default() += 1;
                    continue;
                }
                assert_eq!(t.difficulty, share.difficulty, "difficulty at height {h}");
                assert_eq!(t.sidechain_id, share.sidechain_id, "id at height {h}");
                let mined = mined_share(&t, share.nonce, share.extra_nonce);
                assert_eq!(
                    mined.serialize(UNLOCK_WINDOW),
                    *bytes,
                    "bytes at height {h}"
                );
                compared += 1;
                first_height.get_or_insert(*h);
            }
        }
        for (share, _) in group {
            sc.add_block(share.clone(), &main, NOW).unwrap();
        }
    }
    eprintln!(
        "full cache: {} shares, {compared} rebuilt byte for byte (from height {first_height:?}), \
         {uncles_differ} with other uncles {reasons:?}, {not_on_tip} not on the tip",
        shares.len()
    );
    // every share above the verified depth (the last 184 of this set) has a tip to build on
    assert!(compared >= 150, "{compared} shares rebuilt");
    // a late share is a candidate uncle for the next three heights
    assert!(
        uncles_differ * 4 < compared,
        "{uncles_differ} uncle lists differ"
    );
}
