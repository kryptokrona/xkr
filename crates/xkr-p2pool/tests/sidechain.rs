//! The sidechain against the C++ p2pool, share by share.
//!
//! `data/cpp-expected.jsonl` holds what the C++ sidechain (`SideChain` without a p2pool object,
//! shares added with `add_block` in height order, `m_testMainChainDiff` = the main-chain
//! difficulty at each share's seed height) computed for every fixture share, taken from runs
//! over 1 000-share slices of the block cache: ids, links, difficulties, depth, flags, the PPLNS window
//! (wallets and weights in output order, bottom height), `split_reward` of the share's total
//! and its coinbase outputs.
//!
//! [`fixture_shares_match_cpp`] replays the fixture shares the same way. Windows that reach
//! below the fixture cannot be walked here; for those the order, split and outputs are checked
//! from the C++ window. The ignored tests need the full local vector set in
//! `XKR_P2POOL_VECTORS_DIR` (they skip when it is not set):
//! [`full_cache_matches_cpp`] replays the same slices as the C++ run and compares every field;
//! [`full_cache_in_one_sidechain`] replays all 4 608 shares in one sidechain, where the chain is
//! deep enough to be verified, get a tip and be pruned.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;
use xkr_consensus::Network;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView, seed_height};
use xkr_p2pool::pplns::{MinerShare, expected_outputs, shuffle, split_reward};
use xkr_p2pool::{
    Difficulty, ForkHeights, MinerWallet, PoolBlock, SideChain, SidechainConfig, SidechainEvent,
    SidechainOptions, StaticView, parse_pool_block,
};
use xkr_types::{Hash, PublicKey};

/// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` on mainnet.
const UNLOCK_WINDOW: u64 = 20;
/// Local time used for the replay (no pruning happens: no share gets verified).
const NOW: u64 = 1_800_000_000;

/// Main-chain difficulty per seed height, nothing else (like `m_testMainChainDiff`).
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

fn hash(v: &Value) -> Hash {
    v.as_str().unwrap().parse().unwrap()
}

fn key(v: &Value) -> PublicKey {
    v.as_str().unwrap().parse().unwrap()
}

fn diff(v: &Value) -> Difficulty {
    Difficulty(v.as_str().unwrap().parse().unwrap())
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

/// Parses the blobs and adds them as the harness does: stable sort by sidechain height, then
/// `add_block` one by one.
fn replay(blobs: &[Vec<u8>], main: &dyn MainChainView) -> SideChain {
    let mut sc = SideChain::new(options());
    let view = StaticView {
        consensus_id: *sc.consensus_id(),
        forks: sc.options().forks,
    };
    let mut blocks: Vec<PoolBlock> = blobs
        .iter()
        .map(|b| parse_pool_block(b, &view).unwrap())
        .collect();
    blocks.sort_by_key(|b| b.sidechain_height);
    for b in blocks {
        sc.add_block(b, main, NOW).unwrap();
    }
    sc
}

fn blobs_of(rows: &[Value]) -> Vec<Vec<u8>> {
    rows.iter()
        .map(|r| hex::decode(r["serialized"].as_str().unwrap()).unwrap())
        .collect()
}

/// The main-chain difficulties the C++ run used, from its rows.
fn seed_difficulties_from_rows(rows: &[Value]) -> SeedDifficulties {
    let mut m = BTreeMap::new();
    for r in rows {
        if let Some(d) = r.get("main_chain_difficulty").filter(|d| !d.is_null()) {
            m.insert(r["seed_height"].as_u64().unwrap(), diff(d));
        }
    }
    SeedDifficulties(m)
}

fn cpp_window(row: &Value) -> Option<Vec<MinerShare>> {
    let shares = row.get("shares")?.as_array()?;
    Some(
        shares
            .iter()
            .map(|s| MinerShare {
                weight: diff(&s["weight"]),
                wallet: MinerWallet {
                    spend: key(&s["spend"]),
                    view: key(&s["view"]),
                },
            })
            .collect(),
    )
}

/// The fields every share must match whatever the sidechain around it holds: links,
/// difficulties, keys, tx key seed and its own outputs.
fn check_fixed_fields(b: &PoolBlock, row: &Value) {
    let h = b.sidechain_height;
    assert_eq!(b.sidechain_id, hash(&row["id"]));
    assert_eq!(h, row["height"].as_u64().unwrap());
    assert_eq!(b.txin_gen_height, row["txin_gen_height"].as_u64().unwrap());
    assert_eq!(
        seed_height(b.txin_gen_height),
        row["seed_height"].as_u64().unwrap()
    );
    assert_eq!(b.prev_id, hash(&row["prev_id"]), "height {h}");
    assert_eq!(b.parent, hash(&row["parent"]), "height {h}");
    let uncles: Vec<Hash> = row["uncles"].as_array().unwrap().iter().map(hash).collect();
    assert_eq!(b.uncles, uncles, "height {h}");
    assert_eq!(b.difficulty, diff(&row["difficulty"]), "height {h}");
    assert_eq!(
        b.cumulative_difficulty,
        diff(&row["cumulative_difficulty"]),
        "height {h}"
    );
    assert_eq!(b.timestamp, row["timestamp"].as_u64().unwrap());
    assert_eq!(u64::from(b.nonce), row["nonce"].as_u64().unwrap());
    assert_eq!(
        u64::from(b.extra_nonce),
        row["extra_nonce"].as_u64().unwrap()
    );
    assert_eq!(b.txkey_sec_seed, hash(&row["txkey_sec_seed"]));
    assert_eq!(b.txkey_pub, key(&row["txkey_pub"]));
    assert_eq!(b.txkey_sec.0, hash(&row["txkey_sec"]).0);
    assert_eq!(
        b.calculate_tx_key_seed(UNLOCK_WINDOW),
        hash(&row["tx_key_seed"]),
        "height {h}"
    );
    let outputs: Vec<(u64, PublicKey)> = row["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| (o["amount"].as_u64().unwrap(), key(&o["key"])))
        .collect();
    let own: Vec<(u64, PublicKey)> = b
        .outputs
        .iter()
        .zip(&b.eph_public_keys)
        .map(|(o, k)| (o.reward, *k))
        .collect();
    assert_eq!(own, outputs, "height {h}");
}

fn opt_diff(v: &Value) -> Option<Difficulty> {
    v.as_str().map(|s| Difficulty(s.parse().unwrap()))
}

/// The fixed fields, the verification flags and the next difficulty (cached and computed) of a
/// stored share.
fn check_share_fields(sc: &SideChain, row: &Value) -> PoolBlock {
    let id = hash(&row["id"]);
    let b = sc
        .find_block(&id)
        .unwrap_or_else(|| panic!("share {id} not stored"))
        .clone();
    let h = b.sidechain_height;
    check_fixed_fields(&b, row);
    assert_eq!(b.offchain.verified, row["verified"].as_u64() == Some(1));
    assert_eq!(b.offchain.invalid, row["invalid"].as_u64() == Some(1));
    assert_eq!(
        sc.cached_next_difficulty(&id),
        opt_diff(&row["cached_next_difficulty"])
    );
    assert_eq!(
        sc.compute_difficulty(&b),
        opt_diff(&row["next_difficulty"]),
        "height {h}"
    );
    b
}

/// Order, split and outputs from the C++ window: sorting the wallets and shuffling them with
/// the share's tx key seed gives the C++ order, `split_reward` of the share's total gives the
/// C++ split, and the outputs derived from it are the share's own.
fn check_payout_from_cpp_window(b: &PoolBlock, row: &Value, window: &[MinerShare]) {
    let h = b.sidechain_height;
    let mut sorted = window.to_vec();
    sorted.sort_by_key(|s| s.wallet);
    shuffle(&mut sorted, &b.txkey_sec_seed);
    assert_eq!(sorted, window, "output order at height {h}");

    let expected_split: Vec<u64> = row["split_reward"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let rewards = split_reward(b.total_reward(), window).unwrap();
    assert_eq!(rewards, expected_split, "split at height {h}");

    let outs = expected_outputs(window, &rewards, &b.txkey_sec).unwrap();
    assert_eq!(outs.len(), b.outputs.len(), "output count at height {h}");
    for (i, o) in outs.iter().enumerate() {
        assert_eq!(o.amount, b.outputs[i].reward, "amount {i} at height {h}");
        assert_eq!(o.key, b.eph_public_keys[i], "key {i} at height {h}");
    }
}

/// The PPLNS window this sidechain computes equals the C++ one.
fn check_window(sc: &SideChain, main: &dyn MainChainView, b: &PoolBlock, row: &Value) {
    let h = b.sidechain_height;
    let w = sc.get_shares(b, main).unwrap();
    assert_eq!(
        Some(w.shares.clone()),
        cpp_window(row),
        "window at height {h}"
    );
    assert_eq!(
        w.bottom_height,
        row["window_bottom"].as_u64().unwrap(),
        "bottom at height {h}"
    );
    assert_eq!(
        w.epoch_mixed,
        row["epoch_mixed"].as_u64() == Some(1),
        "epoch at height {h}"
    );
}

#[test]
fn fixture_shares_match_cpp() {
    let rows = read_jsonl(&data_dir().join("cpp-expected.jsonl"));
    let ids = &rows[0]["consensus_id"];
    for (name, config) in [
        ("default", SidechainConfig::main()),
        ("mini", SidechainConfig::mini()),
        ("nano", SidechainConfig::nano()),
    ] {
        assert_eq!(config.consensus_id(), hash(&ids[name]), "{name}");
    }
    let rows = &rows[1..];
    assert_eq!(rows.len(), 63);
    let main = seed_difficulties_from_rows(rows);
    let mut blobs = Vec::new();
    for name in ["shares-mainnet.jsonl", "shares-sizes.jsonl"] {
        blobs.extend(blobs_of(&read_jsonl(&data_dir().join(name))));
    }
    let sc = replay(&blobs, &main);
    assert_eq!(sc.len(), rows.len());
    assert!(sc.chain_tip().is_none());

    // The consecutive run starts at 27 180; the C++ run had 696 more shares on top of its
    // highest share (27 236), and everything above reaches the run only through that share,
    // so each depth here is the C++ depth minus that. The two lone shares have nothing on top.
    let run_bottom = 27_180;
    let top_depth = rows
        .iter()
        .find(|r| r["height"].as_u64() == Some(27_236))
        .and_then(|r| r["depth"].as_u64())
        .unwrap();

    let mut walked = 0;
    for row in rows {
        let b = check_share_fields(&sc, row);
        let h = b.sidechain_height;
        let cpp_depth = row["depth"].as_u64().unwrap();
        if h >= run_bottom {
            assert_eq!(
                b.offchain.depth,
                cpp_depth - top_depth,
                "depth at height {h}"
            );
        } else {
            assert_eq!(b.offchain.depth, 0, "depth at height {h}");
        }

        let window = cpp_window(row).expect("every fixture share has a C++ window");
        check_payout_from_cpp_window(&b, row, &window);

        let inside = h >= run_bottom && row["window_bottom"].as_u64().unwrap() >= run_bottom;
        assert_eq!(sc.get_shares(&b, &main).is_ok(), inside, "height {h}");
        if inside {
            check_window(&sc, &main, &b, row);
            walked += 1;
        }
    }
    assert_eq!(walked, 35);

    // Nothing is verified without the history below the fixture: the missing parents are
    // those of the lowest share of the run and of the two lone shares.
    let missing = sc.get_missing_blocks();
    assert_eq!(missing.len(), 3, "{missing:?}");
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

/// The full vector set: C++ rows (one per share), the main-chain difficulties, the cache blobs
/// in file order; `None` when [`vectors_dir`] is not set.
fn full_set() -> Option<(Vec<Value>, SeedDifficulties, Vec<Vec<u8>>)> {
    let dir = vectors_dir()?;
    let rows: Vec<Value> = read_jsonl(&dir.join("cpp-vectors-a.jsonl"))
        .into_iter()
        .filter(|r| r.get("id").is_some())
        .collect();
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
    Some((rows, main, blobs))
}

/// What one replay gave for one share: the values that depend on the shares held around it.
#[derive(Debug, PartialEq, Eq)]
struct RunValues {
    depth: u64,
    verified: bool,
    invalid: bool,
    cached_next_difficulty: Option<Difficulty>,
    next_difficulty: Option<Difficulty>,
    /// Wallets and weights, bottom height, epoch flag.
    window: Option<(Vec<MinerShare>, u64, bool)>,
}

impl RunValues {
    fn of_row(row: &Value) -> Self {
        Self {
            depth: row["depth"].as_u64().unwrap(),
            verified: row["verified"].as_u64() == Some(1),
            invalid: row["invalid"].as_u64() == Some(1),
            cached_next_difficulty: opt_diff(&row["cached_next_difficulty"]),
            next_difficulty: opt_diff(&row["next_difficulty"]),
            window: cpp_window(row).map(|w| {
                (
                    w,
                    row["window_bottom"].as_u64().unwrap(),
                    row["epoch_mixed"].as_u64() == Some(1),
                )
            }),
        }
    }
}

/// Every share of node a's block cache, replayed the way the C++ values were made.
///
/// The C++ harness ran on overlapping slices of 1 000 lines of `cache-blocks-a.jsonl` (lines
/// 1-1000, 501-1500, …, 4001-4608), each in a new sidechain, and kept per share the row of one
/// of those runs. The same slices are replayed here: the fixed fields must match in every run,
/// and the C++ row (depth, flags, next difficulty, PPLNS window) must equal the values of one
/// of the runs that held the share; a share without a C++ window has none in any run. Split
/// and outputs follow from the C++ window.
#[test]
#[ignore = "needs the full vector set (XKR_P2POOL_VECTORS_DIR)"]
fn full_cache_matches_cpp() {
    let Some((rows, main, blobs)) = full_set() else {
        return;
    };
    assert_eq!(blobs.len(), 4608);
    let by_id: HashMap<Hash, &Value> = rows.iter().map(|r| (hash(&r["id"]), r)).collect();
    assert_eq!(by_id.len(), rows.len());

    let mut runs: HashMap<Hash, Vec<RunValues>> = HashMap::new();
    let mut start = 0;
    loop {
        let end = (start + 1000).min(blobs.len());
        let sc = replay(&blobs[start..end], &main);
        assert_eq!(sc.len(), end - start, "slice {start}..{end}");
        for b in sc.blocks() {
            let row = by_id
                .get(&b.sidechain_id)
                .unwrap_or_else(|| panic!("share {} has no C++ row", b.sidechain_id));
            check_fixed_fields(b, row);
            runs.entry(b.sidechain_id).or_default().push(RunValues {
                depth: b.offchain.depth,
                verified: b.offchain.verified,
                invalid: b.offchain.invalid,
                cached_next_difficulty: sc.cached_next_difficulty(&b.sidechain_id),
                next_difficulty: sc.compute_difficulty(b),
                window: sc
                    .get_shares(b, &main)
                    .ok()
                    .map(|w| (w.shares, w.bottom_height, w.epoch_mixed)),
            });
        }
        if end == blobs.len() {
            break;
        }
        start += 500;
    }

    let mut compared = 0;
    for row in &rows {
        let id = hash(&row["id"]);
        let h = row["height"].as_u64().unwrap();
        let got = runs
            .get(&id)
            .unwrap_or_else(|| panic!("share {id} in no run"));
        let expected = RunValues::of_row(row);
        assert!(
            got.contains(&expected),
            "height {h}: C++ {expected:?} not among the runs {got:?}"
        );
        match &expected.window {
            Some((window, _, _)) => {
                let view = StaticView {
                    consensus_id: SidechainConfig::main().consensus_id(),
                    forks: ForkHeights::from_params(Network::Mainnet.params()),
                };
                let blob = hex::decode(row["serialized"].as_str().unwrap()).unwrap();
                let b = parse_pool_block(&blob, &view).unwrap();
                check_payout_from_cpp_window(&b, row, window);
                compared += 1;
            }
            None => assert!(got.iter().all(|r| r.window.is_none()), "height {h}"),
        }
    }
    assert_eq!(compared, 4555);
}

/// All 4 608 shares in one sidechain, which C++ never did: the chain is deep enough that the
/// lowest shares are accepted as deep, everything above them is verified with the full rules
/// (difficulty retarget over 2 160 real shares, PPLNS outputs), a tip is chosen and the
/// shares more than the prune distance below it are removed.
#[test]
#[ignore = "needs the full vector set (XKR_P2POOL_VECTORS_DIR)"]
fn full_cache_in_one_sidechain() {
    let Some((rows, main, blobs)) = full_set() else {
        return;
    };
    let mut sc = replay(&blobs, &main);
    let events = sc.take_events();
    let invalid_events = events
        .iter()
        .filter(|e| matches!(e, SidechainEvent::Invalid { .. }))
        .count();
    let pruned: usize = events
        .iter()
        .map(|e| match e {
            SidechainEvent::Pruned { blocks, .. } => *blocks,
            _ => 0,
        })
        .sum();
    let verified = sc.blocks().filter(|b| b.offchain.verified).count();
    let tip = sc.chain_tip().expect("a chain tip").clone();
    let cutoff = tip.sidechain_height - sc.prune_distance();
    eprintln!(
        "one sidechain: {} shares, {} stored, {pruned} pruned (below height {cutoff}), \
         {verified} verified, {} waiting, {invalid_events} invalid, tip height {}",
        rows.len(),
        sc.len(),
        sc.len() - verified,
        tip.sidechain_height
    );
    assert_eq!(invalid_events, 0);
    assert!(sc.blocks().all(|b| !b.offchain.invalid));
    assert_eq!(sc.len() + pruned, rows.len());

    let max_height = rows
        .iter()
        .map(|r| r["height"].as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(tip.sidechain_height, max_height);
    // The last full window below the tip passed every check, difficulty included.
    let mut cur = Some(&tip);
    let mut checked = 0;
    while let Some(b) = cur {
        if checked == sc.config().pplns_window {
            break;
        }
        assert!(
            b.offchain.verified && !b.offchain.invalid,
            "height {}",
            b.sidechain_height
        );
        checked += 1;
        cur = sc.parent_of(b);
    }
    assert_eq!(checked, sc.config().pplns_window);
    assert!(sc.compute_difficulty(&tip).is_some());

    let mut compared = 0;
    for row in &rows {
        let id = hash(&row["id"]);
        let h = row["height"].as_u64().unwrap();
        let Some(b) = sc.find_block(&id) else {
            assert!(h < cutoff, "share at height {h} pruned above {cutoff}");
            continue;
        };
        check_fixed_fields(b, row);
        if cpp_window(row).is_some()
            && let Ok(w) = sc.get_shares(b, &main)
        {
            assert_eq!(Some(w.shares), cpp_window(row), "window at height {h}");
            assert_eq!(w.bottom_height, row["window_bottom"].as_u64().unwrap());
            compared += 1;
        }
    }
    assert!(compared > 4000, "{compared} windows compared");
}
