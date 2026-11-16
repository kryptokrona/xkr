//! Sidechain rules on synthetic chains: every share is built the way a template is (seed,
//! keys, difficulty, cumulative difficulty, PPLNS outputs, id) and then added like a share
//! from the cache, so it goes through the full verification.

use std::collections::HashMap;

use xkr_types::{Hash, PublicKey, SecretKey};

use super::*;
use crate::block::{OffchainState, TxOutput};
use crate::forks::{COINBASE_TX_VERSION_2, coinbase_tx_version};
use crate::keys::get_tx_keys;
use crate::mainchain::MainChainTip;
use crate::order::{hash_cmp, xorshift64star};
use crate::pplns::{MinerShare, expected_outputs, split_reward};
use crate::wallet::MinerWallet;

const UNLOCK: u64 = 1;
const REWARD: u64 = 3_000_000_021;
const T0: u64 = 1_700_000_000;
const MIN: u64 = 100_000;
const BASE: u64 = 1000;
const NOW: u64 = 1_800_000_000;

/// A small sidechain: 10-share window, 90 s shares (so fork choice can use few main blocks).
fn config() -> SidechainConfig {
    SidechainConfig {
        name: "unit".into(),
        password: String::new(),
        block_time: 90,
        min_diff: MIN,
        pplns_window: 10,
        uncle_penalty: 20,
    }
}

fn new_chain() -> SideChain {
    SideChain::new(SidechainOptions::new(
        config(),
        ForkHeights::default(),
        UNLOCK,
    ))
}

fn main_id(h: u64) -> Hash {
    let mut id = [0u8; 32];
    id[..8].copy_from_slice(&h.to_le_bytes());
    id[31] = 0x4d;
    Hash(id)
}

/// A main chain whose blocks below `next_height` are known, one every 90 s from `BASE`.
struct Main {
    diff: Difficulty,
    next_height: u64,
}

impl Main {
    fn new(next_height: u64) -> Self {
        Self {
            diff: Difficulty::from_u64(1_000_000_000_000_000),
            next_height,
        }
    }
}

impl MainChainView for Main {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        (height < self.next_height).then(|| ChainMain {
            difficulty: self.diff,
            height,
            timestamp: T0 + height.saturating_sub(BASE) * 90,
            reward: 0,
            id: main_id(height),
        })
    }
    fn block_by_hash(&self, id: &Hash) -> Option<ChainMain> {
        let h = u64::from_le_bytes(core::array::from_fn(|i| id.0[i]));
        if *id == main_id(h) {
            self.block_at_height(h)
        } else {
            None
        }
    }
    fn tip(&self) -> MainChainTip {
        MainChainTip {
            height: self.next_height,
            prev_id: main_id(self.next_height - 1),
            difficulty: self.diff,
        }
    }
}

/// `main` with its next block at `next`: the same known blocks, another miner data.
struct AtTip<'a> {
    main: &'a Main,
    next: u64,
}

impl MainChainView for AtTip<'_> {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        self.main.block_at_height(height)
    }
    fn block_by_hash(&self, id: &Hash) -> Option<ChainMain> {
        self.main.block_by_hash(id)
    }
    fn tip(&self) -> MainChainTip {
        MainChainTip {
            height: self.next,
            prev_id: main_id(self.next - 1),
            difficulty: self.main.diff,
        }
    }
}

/// A main chain that knows nothing.
struct NoMain;

impl MainChainView for NoMain {
    fn block_at_height(&self, _height: u64) -> Option<ChainMain> {
        None
    }
    fn block_by_hash(&self, _id: &Hash) -> Option<ChainMain> {
        None
    }
    fn tip(&self) -> MainChainTip {
        MainChainTip::default()
    }
}

fn wallet(n: u8) -> MinerWallet {
    let (spend, _) = get_tx_keys(&Hash([n; 32]), &Hash([0x11; 32]));
    let (view, _) = get_tx_keys(&Hash([n; 32]), &Hash([0x22; 32]));
    MinerWallet { spend, view }
}

/// What a test share looks like.
#[derive(Clone)]
struct Spec {
    parent: Option<Hash>,
    uncles: Vec<Hash>,
    wallet: u8,
    txin: u64,
    timestamp: u64,
    major: u8,
    tag: u32,
}

impl Spec {
    /// A share at sidechain height `height` on `parent`, one main block per share.
    fn at(parent: Option<Hash>, height: u64, wallet: u8) -> Self {
        Self {
            parent,
            uncles: Vec::new(),
            wallet,
            txin: BASE + height,
            timestamp: T0 + height * 90,
            major: 5,
            tag: 0,
        }
    }
}

fn skeleton(s: &Spec) -> PoolBlock {
    PoolBlock {
        major_version: s.major,
        minor_version: 14,
        prev_id: main_id(s.txin - 1),
        timestamp: s.timestamp,
        nonce: 0,
        unlock_time: s.txin + UNLOCK,
        txin_gen_height: s.txin,
        outputs: Vec::new(),
        eph_public_keys: Vec::new(),
        txkey_pub: PublicKey::ZERO,
        extra_nonce: 0,
        merkle_root: Hash::ZERO,
        transactions: vec![Hash::ZERO],
        miner_wallet: wallet(s.wallet),
        txkey_sec_seed: Hash::ZERO,
        parent: Hash::ZERO,
        uncles: Vec::new(),
        sidechain_height: 0,
        difficulty: Difficulty::ZERO,
        cumulative_difficulty: Difficulty::ZERO,
        merkle_proof: Vec::new(),
        merge_mining_extra: Vec::new(),
        sidechain_extra: [0, 2, s.tag, 0],
        txkey_sec: SecretKey::ZERO,
        sidechain_id: Hash::ZERO,
        offchain: OffchainState::default(),
    }
}

/// Recomputes the id (and merkle root) after a change.
fn reid(sc: &SideChain, b: &mut PoolBlock) {
    b.sidechain_id = b.calculate_sidechain_id(UNLOCK, sc.consensus_id());
    b.merkle_root = b.sidechain_id;
}

/// Coinbase outputs for `shares`, then the id.
fn finish(sc: &SideChain, b: &mut PoolBlock, shares: &[MinerShare]) {
    let rewards = split_reward(REWARD, shares).unwrap();
    let outs = expected_outputs(shares, &rewards, &b.txkey_sec).unwrap();
    let tagged = coinbase_tx_version(b.major_version) >= COINBASE_TX_VERSION_2;
    b.outputs = outs
        .iter()
        .map(|o| TxOutput {
            reward: o.amount,
            view_tag: if tagged { o.view_tag } else { 0 },
        })
        .collect();
    b.eph_public_keys = outs.iter().map(|o| o.key).collect();
    reid(sc, b);
}

/// A valid share as a peer would send it.
fn build(sc: &SideChain, main: &dyn MainChainView, s: &Spec) -> PoolBlock {
    let mut b = skeleton(s);
    match s.parent.map(|p| sc.find_block(&p).unwrap().clone()) {
        None => {
            b.difficulty = sc.config().min_difficulty();
            b.cumulative_difficulty = b.difficulty;
            b.txkey_sec_seed = *sc.consensus_id();
        }
        Some(p) => {
            b.txkey_sec_seed = if b.prev_id == p.prev_id {
                p.txkey_sec_seed
            } else {
                p.calculate_tx_key_seed(UNLOCK)
            };
            b.parent = p.sidechain_id;
            b.sidechain_height = p.sidechain_height + 1;
            b.difficulty = sc.compute_difficulty(&p).unwrap();
            b.cumulative_difficulty = p.cumulative_difficulty.wrapping_add(b.difficulty);
            for u in &s.uncles {
                let ud = sc.find_block(u).map_or(Difficulty::ZERO, |x| x.difficulty);
                b.cumulative_difficulty = b.cumulative_difficulty.wrapping_add(ud);
            }
        }
    }
    b.uncles.clone_from(&s.uncles);
    (b.txkey_pub, b.txkey_sec) = get_tx_keys(&b.txkey_sec_seed, &b.prev_id);
    let shares = sc.get_shares(&b, main).map_or_default(|w| w.shares);
    if shares.is_empty() {
        // Window not computable (missing uncle): pay the miner alone.
        let alone = MinerShare {
            weight: b.difficulty,
            wallet: b.miner_wallet,
        };
        finish(sc, &mut b, &[alone]);
    } else {
        finish(sc, &mut b, &shares);
    }
    b
}

fn add(sc: &mut SideChain, main: &dyn MainChainView, b: PoolBlock) -> Hash {
    let id = b.sidechain_id;
    sc.add_block(b, main, NOW).unwrap();
    id
}

/// Builds a share from `s` and adds it.
fn add_new(sc: &mut SideChain, main: &dyn MainChainView, s: &Spec) -> Hash {
    let b = build(sc, main, s);
    add(sc, main, b)
}

/// Builds and adds `count` shares on `parent` (`None`: from genesis) with wallets cycling
/// through `wallets`.
fn grow(
    sc: &mut SideChain,
    main: &dyn MainChainView,
    parent: Option<Hash>,
    count: u64,
    wallets: &[u8],
) -> Vec<Hash> {
    let mut ids = Vec::new();
    let mut parent = parent;
    let start = parent.map_or(0, |p| sc.find_block(&p).unwrap().sidechain_height + 1);
    for i in 0..count {
        let w = wallets[(i as usize) % wallets.len()];
        let b = build(sc, main, &Spec::at(parent, start + i, w));
        let id = add(sc, main, b);
        ids.push(id);
        parent = Some(id);
    }
    ids
}

fn state(sc: &SideChain, id: &Hash) -> (bool, bool) {
    let b = sc.find_block(id).unwrap();
    (b.offchain.verified, b.offchain.invalid)
}

const VALID: (bool, bool) = (true, false);
const INVALID: (bool, bool) = (true, true);
const WAITING: (bool, bool) = (false, false);

#[test]
fn linear_chain_verifies_and_moves_the_tip() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let ids = grow(&mut sc, &main, None, 13, &[1, 2, 3]);
    for (h, id) in ids.iter().enumerate() {
        assert_eq!(state(&sc, id), VALID, "height {h}");
        assert_eq!(sc.find_block(id).unwrap().offchain.depth, 12 - h as u64);
    }
    let tip = sc.chain_tip().unwrap().clone();
    assert_eq!(tip.sidechain_id, ids[12]);
    let events = sc.take_events();
    let new_tips: Vec<Hash> = events
        .iter()
        .filter_map(|e| match e {
            SidechainEvent::NewTip { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(new_tips, ids);
    assert!(events.contains(&SidechainEvent::StoreInCache(ids[5])));
    assert!(events.contains(&SidechainEvent::Broadcast(ids[12])));

    // Difficulty of the next share: the retarget over the tip's window.
    let data = sc.difficulty_data(&tip).unwrap();
    assert_eq!(data.len(), 10);
    assert_eq!(
        Some(sc.difficulty()),
        next_difficulty(&data, 90, Difficulty::from_u64(MIN))
    );
    assert_eq!(sc.cached_next_difficulty(&ids[12]), Some(sc.difficulty()));

    // The PPLNS window: 10 shares, three wallets.
    let w = sc.get_shares(&tip, &main).unwrap();
    assert_eq!((w.blocks, w.bottom_height, w.shares.len()), (10, 3, 3));
    assert!(!w.epoch_mixed);
    assert_eq!(sc.bottom_height(&tip, &main), 3);

    // A template on the tip equals a share built by hand on it.
    let mut t = skeleton(&Spec::at(None, 13, 4));
    let tw = sc.fill_sidechain_data(&mut t, &main).unwrap();
    let hand = build(&sc, &main, &Spec::at(Some(ids[12]), 13, 4));
    assert_eq!(t.parent, hand.parent);
    assert_eq!(t.sidechain_height, 13);
    assert_eq!(t.txkey_sec_seed, hand.txkey_sec_seed);
    assert_eq!(t.txkey_sec, hand.txkey_sec);
    assert_eq!(t.difficulty, hand.difficulty);
    assert_eq!(t.cumulative_difficulty, hand.cumulative_difficulty);
    assert!(t.uncles.is_empty());
    assert_eq!(tw.shares.len(), 4);

    // An own share (already verified) goes straight to the tip.
    let mut own = hand;
    own.offchain.verified = true;
    let own_id = add(&mut sc, &main, own);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, own_id);
    assert!(
        sc.take_events()
            .contains(&SidechainEvent::StoreInCache(own_id))
    );

    // A duplicate is refused.
    let again = sc.find_block(&own_id).unwrap().clone();
    assert_eq!(
        sc.add_block(again, &main, NOW),
        Err(AddError::Duplicate(own_id))
    );
}

#[test]
fn genesis_share() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    // A template on an empty sidechain is the genesis share.
    let mut t = skeleton(&Spec::at(None, 0, 1));
    let w = sc.fill_sidechain_data(&mut t, &main).unwrap();
    assert_eq!(t.sidechain_height, 0);
    assert!(t.parent.is_zero());
    assert_eq!(t.difficulty, Difficulty::from_u64(MIN));
    assert_eq!(t.cumulative_difficulty, Difficulty::from_u64(MIN));
    assert_eq!(t.txkey_sec_seed, *sc.consensus_id());
    assert_eq!(w.shares.len(), 1);

    let mut bad = build(&sc, &main, &Spec::at(None, 0, 2));
    bad.difficulty = Difficulty::from_u64(MIN + 1);
    bad.cumulative_difficulty = bad.difficulty;
    reid(&sc, &mut bad);
    let bad = add(&mut sc, &main, bad);
    assert_eq!(state(&sc, &bad), INVALID);
    assert!(sc.chain_tip().is_none());

    let good = add_new(&mut sc, &main, &Spec::at(None, 0, 1));
    assert_eq!(state(&sc, &good), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, good);

    // Children of an invalid share are invalid.
    let child = build(&sc, &main, &Spec::at(Some(bad), 1, 3));
    let child = add(&mut sc, &main, child);
    assert_eq!(state(&sc, &child), INVALID);
}

#[test]
fn uncles_are_chosen_weighted_and_checked() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 3, &[1]);
    // A sibling of a2 by another miner: valid, not the tip, a possible uncle.
    let mut s = Spec::at(Some(a[1]), 2, 7);
    s.timestamp += 1;
    let b2 = build(&sc, &main, &s);
    let b2_difficulty = b2.difficulty;
    sc.take_events();
    let b2 = add(&mut sc, &main, b2);
    assert_eq!(state(&sc, &b2), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, a[2]);
    assert!(
        sc.take_events()
            .contains(&SidechainEvent::PossibleUncle(b2))
    );

    // The template on a2 takes it as an uncle.
    let mut t = skeleton(&Spec::at(None, 3, 1));
    let w = sc.fill_sidechain_data(&mut t, &main).unwrap();
    assert_eq!(t.uncles, [b2]);
    let tip = sc.chain_tip().unwrap();
    assert_eq!(
        t.cumulative_difficulty,
        tip.cumulative_difficulty
            .wrapping_add(t.difficulty)
            .wrapping_add(b2_difficulty)
    );
    finish(&sc, &mut t, &w.shares);
    let a3 = add(&mut sc, &main, t);
    assert_eq!(state(&sc, &a3), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, a3);
    assert_eq!(sc.find_block(&b2).unwrap().offchain.depth, 1);

    // Weights: the uncle's miner gets 80 %, the includer 20 % on top of its own.
    let a3b = sc.find_block(&a3).unwrap().clone();
    let w = sc.get_shares(&a3b, &main).unwrap();
    assert_eq!(w.uncles, 1);
    let penalty = crate::pplns::uncle_penalty(b2_difficulty, 20);
    let of = |n: u8| {
        w.shares
            .iter()
            .find(|s| s.wallet == wallet(n))
            .unwrap()
            .weight
    };
    assert_eq!(of(7), b2_difficulty.wrapping_sub(penalty));
    let own: Difficulty = [a[0], a[1], a[2], a3]
        .iter()
        .map(|id| sc.find_block(id).unwrap().difficulty)
        .fold(Difficulty::ZERO, Difficulty::wrapping_add);
    assert_eq!(of(1), own.wrapping_add(penalty));

    // Mined twice: a4 naming b2 again.
    let mut s = Spec::at(Some(a3), 4, 2);
    s.uncles = vec![b2];
    let twice = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &twice), INVALID);

    // Too deep: a share at height 0 (a second genesis share) cannot be an uncle at height 4.
    let g2 = add_new(&mut sc, &main, &Spec::at(None, 0, 8));
    assert_eq!(state(&sc, &g2), VALID);
    let mut s = Spec::at(Some(a3), 4, 2);
    s.uncles = vec![g2];
    s.tag = 1;
    let deep = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &deep), INVALID);

    // Not strictly ascending: the same uncle twice.
    let mut s = Spec::at(Some(a[2]), 3, 9);
    s.timestamp += 2;
    let d3 = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &d3), VALID);
    let mut s = Spec::at(Some(a3), 4, 2);
    s.uncles = vec![d3, d3];
    s.tag = 2;
    let repeated = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &repeated), INVALID);

    // An unknown uncle: the share waits and the uncle is asked for.
    let mut s = Spec::at(Some(a3), 4, 2);
    s.uncles = vec![Hash([0x55; 32])];
    s.tag = 3;
    let waiting = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &waiting), WAITING);
    assert!(sc.get_missing_blocks().contains(&Hash([0x55; 32])));

    // With d3 as a proper uncle the share is valid and becomes the tip.
    let mut t = skeleton(&Spec::at(None, 4, 2));
    let w = sc.fill_sidechain_data(&mut t, &main).unwrap();
    assert_eq!(t.uncles, [d3]);
    finish(&sc, &mut t, &w.shares);
    let a4 = add(&mut sc, &main, t);
    assert_eq!(state(&sc, &a4), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, a4);
}

#[test]
fn uncle_from_another_chain_is_invalid() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 5, &[1]);
    // b1..b3 on the genesis share: b3 forks off three shares below a4.
    let b = grow(&mut sc, &main, Some(a[0]), 3, &[6]);
    assert!(b.iter().all(|id| state(&sc, id) == VALID));
    let mut s = Spec::at(Some(a[4]), 5, 2);
    s.uncles = vec![b[2]];
    let x = add_new(&mut sc, &main, &s);
    assert_eq!(state(&sc, &x), INVALID);
    // The template does not offer it either.
    let mut t = skeleton(&Spec::at(None, 5, 2));
    sc.fill_sidechain_data(&mut t, &main).unwrap();
    assert!(!t.uncles.contains(&b[2]));
}

#[test]
fn malformed_shares_are_invalid() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 4, &[1, 2]);
    let tip = a[3];
    let spec = |tag: u32| {
        let mut s = Spec::at(Some(tip), 4, 3);
        s.tag = tag;
        s
    };

    let mut b = build(&sc, &main, &spec(1));
    b.difficulty = b.difficulty.wrapping_add(Difficulty(1));
    b.cumulative_difficulty = b.cumulative_difficulty.wrapping_add(Difficulty(1));
    reid(&sc, &mut b);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "difficulty");

    let mut b = build(&sc, &main, &spec(2));
    b.cumulative_difficulty = b.cumulative_difficulty.wrapping_add(Difficulty(1));
    reid(&sc, &mut b);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "cumulative difficulty");

    let mut b = build(&sc, &main, &spec(3));
    b.txkey_sec_seed = Hash([3; 32]);
    (b.txkey_pub, b.txkey_sec) = get_tx_keys(&b.txkey_sec_seed, &b.prev_id);
    let shares = sc.get_shares(&b, &main).unwrap().shares;
    finish(&sc, &mut b, &shares);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "tx key seed");

    let mut b = build(&sc, &main, &spec(4));
    let j = b
        .outputs
        .iter()
        .position(|o| o.reward != b.outputs[0].reward)
        .unwrap();
    let (r0, rj) = (b.outputs[0].reward, b.outputs[j].reward);
    b.outputs[0].reward = rj;
    b.outputs[j].reward = r0;
    reid(&sc, &mut b);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "amount");

    let mut b = build(&sc, &main, &spec(5));
    b.eph_public_keys.swap(0, 1);
    reid(&sc, &mut b);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "payee");

    let mut b = build(&sc, &main, &spec(6));
    b.sidechain_height += 1;
    reid(&sc, &mut b);
    let id = add(&mut sc, &main, b);
    assert_eq!(state(&sc, &id), INVALID, "height");

    // Version-2 coinbase: the view tags count.
    let mut s = spec(7);
    s.major = 7;
    let good = build(&sc, &main, &s);
    assert!(good.outputs.iter().any(|o| o.view_tag != 0));
    let mut bad = good.clone();
    bad.outputs[0].view_tag ^= 1;
    bad.sidechain_extra[2] = 8;
    reid(&sc, &mut bad);
    let bad = add(&mut sc, &main, bad);
    assert_eq!(state(&sc, &bad), INVALID, "view tag");
    let good = add(&mut sc, &main, good);
    assert_eq!(state(&sc, &good), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, good);
}

#[test]
fn reorg_to_a_heavier_branch() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 3, &[1]);
    let b = grow(&mut sc, &main, Some(a[0]), 1, &[2]);
    let b2 = add_new(&mut sc, &main, &Spec::at(Some(b[0]), 2, 2));
    let (a2, b2b) = (sc.find_block(&a[2]).unwrap(), sc.find_block(&b2).unwrap());
    assert_eq!(a2.cumulative_difficulty, b2b.cumulative_difficulty);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, a[2]);
    assert!(
        sc.take_events()
            .contains(&SidechainEvent::PossibleUncle(b2))
    );

    let b3 = add_new(&mut sc, &main, &Spec::at(Some(b2), 3, 2));
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, b3);
    let events = sc.take_events();
    assert!(events.iter().any(|e| matches!(
        e,
        SidechainEvent::NewTip { id, alternative: false, .. } if *id == b3
    )));
    let cmp = sc.is_longer_chain(sc.find_block(&a[2]), sc.find_block(&b3).unwrap(), &main);
    assert_eq!(
        cmp,
        ChainComparison {
            longer: true,
            alternative: false
        }
    );

    // The old branch catching up to the same work does not take the tip back.
    let a3 = add_new(&mut sc, &main, &Spec::at(Some(a[2]), 3, 1));
    assert_eq!(state(&sc, &a3), VALID);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, b3);
}

/// Chain Y forks off more than a window below X's tip and is heavier by one uncle.
fn two_chains(y_main_offset: u64) -> (SideChain, Vec<Hash>, Vec<Hash>, Main) {
    let main = Main::new(BASE + 13);
    let mut sc = new_chain();
    let x = grow(&mut sc, &main, None, 13, &[1]);
    let g = x[0];
    let mut y = Vec::new();
    let mut parent = g;
    let mut uncle = None;
    for h in 1..=12u64 {
        let mut s = Spec::at(Some(parent), h, 2);
        s.txin += y_main_offset;
        if h == 6 {
            s.uncles = uncle.into_iter().collect();
        }
        let id = add_new(&mut sc, &main, &s);
        if h == 5 {
            let mut s = Spec::at(Some(parent), 5, 3);
            s.txin += y_main_offset;
            s.timestamp += 1;
            uncle = Some(add_new(&mut sc, &main, &s));
        }
        y.push(id);
        parent = id;
    }
    (sc, x, y, main)
}

#[test]
fn alternative_chain_switch() {
    let (mut sc, x, y, main) = two_chains(0);
    assert!(y.iter().all(|id| state(&sc, id) == VALID));
    assert_eq!(sc.find_block(&y[5]).unwrap().uncles.len(), 1);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, y[11]);
    assert!(sc.take_events().iter().any(|e| matches!(
        e,
        SidechainEvent::NewTip { alternative: true, height, .. } if *height > 0
    )));
    let cmp = sc.is_longer_chain(sc.find_block(&x[12]), sc.find_block(&y[11]).unwrap(), &main);
    assert_eq!(
        cmp,
        ChainComparison {
            longer: true,
            alternative: true
        }
    );
    // The other way round: not longer.
    let cmp = sc.is_longer_chain(sc.find_block(&y[11]), sc.find_block(&x[12]).unwrap(), &main);
    assert!(!cmp.longer && cmp.alternative);
}

#[test]
fn alternative_chain_on_unknown_main_blocks_is_refused() {
    let (sc, x, y, main) = two_chains(500);
    assert!(y.iter().all(|id| state(&sc, id) == VALID));
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, x[12]);
    let cmp = sc.is_longer_chain(sc.find_block(&x[12]), sc.find_block(&y[11]).unwrap(), &main);
    assert_eq!(
        cmp,
        ChainComparison {
            longer: false,
            alternative: true
        }
    );
    // Without a tip anything valid is longer.
    let cmp = sc.is_longer_chain(None, sc.find_block(&y[0]).unwrap(), &main);
    assert!(cmp.longer && cmp.alternative);
}

#[test]
fn shares_arriving_child_first() {
    let main = Main::new(BASE + 100);
    let mut scratch = new_chain();
    let ids = grow(&mut scratch, &main, None, 4, &[1, 2]);
    let blocks: Vec<PoolBlock> = ids
        .iter()
        .map(|id| {
            let mut b = scratch.find_block(id).unwrap().clone();
            b.offchain = OffchainState::default();
            b
        })
        .collect();

    let mut sc = new_chain();
    for b in blocks[1..].iter().rev() {
        add(&mut sc, &main, b.clone());
    }
    assert!(ids[1..].iter().all(|id| state(&sc, id) == WAITING));
    assert_eq!(
        sc.get_missing_blocks().into_iter().collect::<Vec<_>>(),
        [ids[0]]
    );
    let depths: Vec<u64> = ids[1..]
        .iter()
        .map(|id| sc.find_block(id).unwrap().offchain.depth)
        .collect();
    assert_eq!(depths, [2, 1, 0]);

    add(&mut sc, &main, blocks[0].clone());
    assert!(ids.iter().all(|id| state(&sc, id) == VALID));
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, ids[3]);
    assert_eq!(sc.find_block(&ids[0]).unwrap().offchain.depth, 3);
    assert!(sc.get_missing_blocks().is_empty());
}

#[test]
fn pruning_by_depth_and_by_age() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    assert_eq!(sc.prune_distance(), 9 * 2 + 6 + 1 + 1);
    let a = grow(&mut sc, &main, None, 6, &[1, 2]);
    // An orphan at height 5, never built on.
    let mut s = Spec::at(Some(a[4]), 5, 9);
    s.timestamp += 1;
    let orphan = add_new(&mut sc, &main, &s);
    grow(&mut sc, &main, Some(a[5]), 35, &[1, 2, 3]);
    let tip = sc.chain_tip().unwrap().sidechain_height;
    assert_eq!(tip, 40);
    // Heights below 40 - 26 are gone, except the orphan (depth 0, still young).
    assert_eq!(sc.len(), 27 + 1);
    assert!(sc.find_block(&a[0]).is_none());
    assert!(sc.find_block(&orphan).is_some());
    assert!(
        sc.blocks()
            .all(|b| b.sidechain_height >= 14 || b.sidechain_id == orphan)
    );
    assert!(
        sc.take_events()
            .iter()
            .any(|e| matches!(e, SidechainEvent::Pruned { .. }))
    );
    // An hour (W * 4 * T) later it goes too.
    assert_eq!(sc.prune_old_blocks(NOW + 3599), 0);
    assert_eq!(sc.prune_old_blocks(NOW + 3600), 1);
    assert!(sc.find_block(&orphan).is_none());
    assert!(sc.find_block_by_merkle_root(&orphan).is_none());
    // The chain goes on.
    let tip_id = sc.chain_tip().unwrap().sidechain_id;
    let next = grow(&mut sc, &main, Some(tip_id), 1, &[4]);
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, next[0]);
}

#[test]
fn testnet_window_without_cap() {
    // Main-chain difficulty 1 as on the testnet: the cap (2) ends every window after one share.
    let mut main = Main::new(BASE + 100);
    main.diff = Difficulty(1);
    let mut capped = new_chain();
    grow(&mut capped, &main, None, 12, &[1, 2, 3]);
    let tip = capped.chain_tip().unwrap().clone();
    let w = capped.get_shares(&tip, &main).unwrap();
    assert_eq!((w.blocks, w.shares.len()), (1, 1));
    assert_eq!(
        capped.pplns_params(&tip, &main).unwrap().max_weight,
        Difficulty(2)
    );

    let mut options = SidechainOptions::new(config(), ForkHeights::default(), UNLOCK);
    options.uncapped_pplns = true;
    let mut open = SideChain::new(options);
    let ids = grow(&mut open, &main, None, 12, &[1, 2, 3]);
    assert!(ids.iter().all(|id| state(&open, id) == VALID));
    let tip = open.chain_tip().unwrap().clone();
    let w = open.get_shares(&tip, &main).unwrap();
    assert_eq!((w.blocks, w.shares.len()), (10, 3));
    assert_eq!(
        open.pplns_params(&tip, &main).unwrap().max_weight,
        Difficulty::MAX
    );
    // The main-chain difficulty is still required.
    assert_eq!(
        open.get_shares(&tip, &NoMain),
        Err(PplnsError::MainChainDifficulty(0))
    );
}

/// A bare share for window arithmetic (not verified).
fn plain(height: u64, parent: Hash, difficulty: u128, wallet_n: u8, txin: u64) -> PoolBlock {
    let mut b = skeleton(&Spec::at(None, height, wallet_n));
    b.sidechain_height = height;
    b.parent = parent;
    b.difficulty = Difficulty(difficulty);
    b.txin_gen_height = txin;
    let mut id = [0u8; 32];
    id[0] = 0xb0;
    id[1] = wallet_n;
    id[2..10].copy_from_slice(&height.to_le_bytes());
    b.sidechain_id = Hash(id);
    b
}

#[test]
fn window_cap_and_uncle_rules() {
    let g = plain(0, Hash::ZERO, 100, 1, 3000);
    let b1 = plain(1, g.sidechain_id, 100, 2, 3001);
    let u1 = plain(1, g.sidechain_id, 50, 3, 3001);
    let mut b2 = plain(2, b1.sidechain_id, 100, 4, 3002);
    b2.uncles = vec![u1.sidechain_id];
    let map: HashMap<Hash, PoolBlock> = [&g, &b1, &u1]
        .into_iter()
        .map(|b| (b.sidechain_id, b.clone()))
        .collect();
    let run = |cap: u128, window: u64| {
        let params = PplnsParams {
            window,
            uncle_penalty: 20,
            max_weight: Difficulty(cap),
        };
        crate::pplns::get_shares(&b2, |id| map.get(id), &params).unwrap()
    };
    let weight = |w: &PplnsWindow, n: u8| {
        w.shares
            .iter()
            .find(|s| s.wallet == wallet(n))
            .map(|s| s.weight.0)
    };

    let w = run(u128::MAX, 10);
    assert_eq!(
        (w.blocks, w.uncles, w.bottom_height, w.weight.0),
        (3, 1, 0, 350)
    );
    assert_eq!(weight(&w, 4), Some(110));
    assert_eq!(weight(&w, 3), Some(40));
    // Above the cap: the share that crosses it is still counted, then the walk stops.
    let w = run(300, 10);
    assert_eq!((w.blocks, w.bottom_height, w.weight.0), (3, 0, 350));
    let w = run(100, 10);
    assert_eq!((w.blocks, w.bottom_height, w.weight.0), (1, 2, 150));
    // An uncle that would pass the cap is left out, and so is its penalty.
    let w = run(30, 10);
    assert_eq!((w.uncles, w.weight.0), (0, 100));
    assert_eq!(weight(&w, 3), None);
    assert_eq!(weight(&w, 4), Some(100));
    // An uncle a full window below the tip is left out.
    let w = run(u128::MAX, 1);
    assert_eq!((w.blocks, w.uncles, w.weight.0), (1, 0, 100));
    let mut b3 = plain(3, b2.sidechain_id, 100, 5, 3003);
    b3.uncles = vec![u1.sidechain_id];
    let mut with_b2 = map.clone();
    with_b2.insert(b2.sidechain_id, b2.clone());
    let params = PplnsParams {
        window: 2,
        uncle_penalty: 20,
        max_weight: Difficulty::MAX,
    };
    let w = crate::pplns::get_shares(&b3, |id| with_b2.get(id), &params).unwrap();
    assert_eq!((w.blocks, w.uncles), (2, 0));
    // Missing pieces.
    let lone = plain(5, Hash([9; 32]), 100, 1, 3005);
    assert_eq!(
        crate::pplns::get_shares(&lone, |id| map.get(id), &params),
        Err(PplnsError::MissingParent {
            height: 4,
            id: Hash([9; 32])
        })
    );
}

#[test]
fn window_across_two_seed_epochs_is_flagged() {
    // Seed height changes between main-chain heights 2048 + 64 and 2048 + 65.
    let boundary = 2048 + 65;
    let mut blocks = Vec::new();
    let mut parent = Hash::ZERO;
    for h in 0..6u64 {
        let b = plain(h, parent, 100, (h % 2) as u8 + 1, boundary - 3 + h);
        parent = b.sidechain_id;
        blocks.push(b);
    }
    let map: HashMap<Hash, PoolBlock> =
        blocks.iter().map(|b| (b.sidechain_id, b.clone())).collect();
    let params = |window| PplnsParams {
        window,
        uncle_penalty: 20,
        max_weight: Difficulty::MAX,
    };
    let tip = &blocks[5];
    assert_ne!(
        seed_height(blocks[0].txin_gen_height),
        seed_height(tip.txin_gen_height)
    );
    let w = crate::pplns::get_shares(tip, |id| map.get(id), &params(6)).unwrap();
    assert!(w.epoch_mixed);
    let w = crate::pplns::get_shares(tip, |id| map.get(id), &params(3)).unwrap();
    assert!(!w.epoch_mixed);
}

/// C++ `get_difficulty` written out with two `nth_element` calls.
fn retarget_reference(data: &[DifficultyData], block_time: u64, min: u64) -> Option<Difficulty> {
    let oldest = data.iter().map(|d| d.timestamp).min()?;
    let mut ts = Vec::new();
    for d in data {
        let dt = d.timestamp - oldest;
        if dt > u64::from(u32::MAX) {
            return None;
        }
        ts.push(dt as u32);
    }
    let cut = data.len().div_ceil(10);
    let (i1, i2) = (cut - 1, data.len() - cut);
    ts.select_nth_unstable(i1);
    let t1 = oldest + u64::from(ts[i1]);
    ts.select_nth_unstable(i2);
    let t2 = oldest + u64::from(ts[i2]);
    let delta_index = if i2 > i1 { (i2 - i1) as u64 } else { 1 };
    let delta_t = core::cmp::max(t2.saturating_sub(t1), delta_index);
    let mut d1 = u128::MAX;
    let mut d2 = 0u128;
    for d in data {
        if (t1..=t2).contains(&d.timestamp) {
            d1 = d1.min(d.cumulative_difficulty.0);
            d2 = d2.max(d.cumulative_difficulty.0);
        }
    }
    let r = d2.wrapping_sub(d1).wrapping_mul(u128::from(block_time)) / u128::from(delta_t);
    Some(Difficulty(r.max(u128::from(min))))
}

#[test]
fn retarget_arithmetic() {
    let dd = |timestamp: u64, cum: u128| DifficultyData {
        timestamp,
        cumulative_difficulty: Difficulty(cum),
    };
    let min = Difficulty::from_u64(MIN);
    // Ten shares, 10 s apart, 100 000 each: 100 000 at a 10 s target.
    let data: Vec<_> = (0..10u64)
        .map(|i| dd(T0 + i * 10, u128::from(i + 1) * 100_000))
        .collect();
    assert_eq!(next_difficulty(&data, 10, min), Some(Difficulty(100_000)));
    // Twice as fast: twice the difficulty.
    let data: Vec<_> = (0..10u64)
        .map(|i| dd(T0 + i * 5, u128::from(i + 1) * 100_000))
        .collect();
    assert_eq!(next_difficulty(&data, 10, min), Some(Difficulty(200_000)));
    // All at the same time: the time span is the index span.
    let data: Vec<_> = (0..10u64)
        .map(|i| dd(T0, u128::from(i + 1) * 1_000_000))
        .collect();
    assert_eq!(
        next_difficulty(&data, 10, min),
        Some(Difficulty(9_000_000 * 10 / 9))
    );
    // Slow shares: the minimum.
    let data: Vec<_> = (0..10u64)
        .map(|i| dd(T0 + i * 1000, u128::from(i + 1)))
        .collect();
    assert_eq!(next_difficulty(&data, 10, min), Some(min));
    // One share.
    assert_eq!(next_difficulty(&[dd(T0, 5)], 10, min), Some(min));
    assert_eq!(next_difficulty(&[], 10, min), None);
    // A timestamp more than u32::MAX seconds after the oldest.
    assert_eq!(next_difficulty(&[dd(0, 1), dd(1 << 32, 2)], 10, min), None);
    // The 10 % cut: an outlier timestamp does not move the result.
    let mut data: Vec<_> = (0..20u64)
        .map(|i| dd(T0 + i * 10, u128::from(i + 1) * 100_000))
        .collect();
    let base = next_difficulty(&data, 10, min);
    data[19].timestamp = T0 + 100_000;
    assert_eq!(next_difficulty(&data, 10, min), base);

    // Random sets against the reference.
    let mut x = 0x1234_5678_9abc_def0u64;
    for round in 0..2000 {
        let n = (round % 40) + 1;
        let data: Vec<_> = (0..n)
            .map(|_| {
                x = xorshift64star(x);
                let ts = T0 + x % 5000;
                x = xorshift64star(x);
                let cum = if round % 7 == 0 {
                    (u128::from(x) << 64) | u128::from(xorshift64star(x))
                } else {
                    u128::from(x % 1_000_000_000_000)
                };
                dd(ts, cum)
            })
            .collect();
        let bt = [1u64, 10, 30, 90][round % 4];
        assert_eq!(
            next_difficulty(&data, bt, min),
            retarget_reference(&data, bt, MIN),
            "round {round}"
        );
    }
}

#[test]
fn external_shares() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 3, &[1]);

    let mut b = build(&sc, &main, &Spec::at(Some(a[2]), 3, 2));
    b.offchain.pow_hash = Some(Hash::ZERO);

    let mut low = b.clone();
    low.difficulty = Difficulty::from_u64(MIN - 1);
    assert_eq!(
        sc.add_external_block(low, &main, NOW),
        Err(BlockRejected::DifficultyBelowMinimum)
    );

    let mut wrong_height = b.clone();
    wrong_height.txin_gen_height += 5;
    assert_eq!(
        sc.add_external_block(wrong_height, &main, NOW),
        Err(BlockRejected::WrongMainChainHeight {
            got: BASE + 3 + 5,
            expected: BASE + 3
        })
    );

    let mut short_pow = b.clone();
    short_pow.offchain.pow_hash = Some(Hash([0xff; 32]));
    assert_eq!(
        sc.add_external_block(short_pow, &main, NOW),
        Err(BlockRejected::NotEnoughPow {
            submit_to_daemon: false
        })
    );

    assert_eq!(
        sc.add_external_block(b.clone(), &NoMain, NOW),
        Ok(ExternalOutcome::Ignored(IgnoreReason::SeedUnknown))
    );

    let found = ChainMain {
        height: BASE + 3,
        id: Hash([0x77; 32]),
        ..ChainMain::default()
    };
    sc.watch_mainchain_block(found, b.merkle_root);
    // The main chain's next block is the share's height: a candidate, submitted.
    let next = AtTip {
        main: &main,
        next: BASE + 3,
    };
    assert_eq!(
        sc.add_external_block(b.clone(), &next, NOW),
        Ok(ExternalOutcome::Added {
            missing: Vec::new(),
            submit_to_daemon: true,
            found: Some(found),
        })
    );
    assert_eq!(sc.chain_tip().unwrap().sidechain_id, b.sidechain_id);
    assert_eq!(
        sc.add_external_block(b, &main, NOW),
        Ok(ExternalOutcome::AlreadyKnown)
    );

    // A share whose parent is not known: stored, waiting, parent asked for.
    let mut scratch = new_chain();
    let ids = grow(&mut scratch, &main, None, 3, &[1]);
    let mut orphan = scratch.find_block(&ids[2]).unwrap().clone();
    orphan.offchain = OffchainState::default();
    orphan.offchain.pow_hash = Some(Hash::ZERO);
    let mut other = new_chain();
    let next = AtTip {
        main: &main,
        next: BASE + 2,
    };
    let outcome = other.add_external_block(orphan, &next, NOW).unwrap();
    assert_eq!(
        outcome,
        ExternalOutcome::Added {
            missing: vec![ids[1]],
            submit_to_daemon: true,
            found: None,
        }
    );
    assert_eq!(state(&other, &ids[2]), WAITING);
}

#[test]
fn only_shares_for_the_next_main_block_are_submitted() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 3, &[1]);
    let mut b = build(&sc, &main, &Spec::at(Some(a[2]), 3, 2));
    b.offchain.pow_hash = Some(Hash::ZERO);
    let pow = Hash::ZERO;

    // Main-chain height of the share (BASE + 3) is the next block: submitted, whether it
    // builds on our top block or on another block at the same height.
    let next = AtTip {
        main: &main,
        next: BASE + 3,
    };
    assert!(submits_as_main_block(&b, &pow, &next));
    let mut other_top = b.clone();
    other_top.prev_id = Hash([0x55; 32]);
    assert!(submits_as_main_block(&other_top, &pow, &next));
    // Not enough proof of work for the main chain: not submitted.
    assert!(!submits_as_main_block(&b, &Hash([0xff; 32]), &next));

    // The main chain already holds that height (one block or many later, as for the old
    // shares of the initial sidechain sync): not submitted.
    for later in [BASE + 4, BASE + 5, BASE + 100] {
        let ahead = AtTip {
            main: &main,
            next: later,
        };
        assert!(!submits_as_main_block(&b, &pow, &ahead), "next {later}");
    }
    // The main chain is behind the share (still syncing): not submitted.
    let behind = AtTip {
        main: &main,
        next: BASE + 2,
    };
    assert!(!submits_as_main_block(&b, &pow, &behind));

    // Through add_external_block: an old share is stored but not submitted.
    let ahead = AtTip {
        main: &main,
        next: BASE + 50,
    };
    assert_eq!(
        sc.add_external_block(b, &ahead, NOW),
        Ok(ExternalOutcome::Added {
            missing: Vec::new(),
            submit_to_daemon: false,
            found: None,
        })
    );
}

#[test]
fn seen_shares() {
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let b = build(&sc, &main, &Spec::at(None, 0, 1));
    assert!(!sc.incoming_block_seen(&b, NOW));
    assert!(sc.incoming_block_seen(&b, NOW + 1));
    let mut other_nonce = b.clone();
    other_nonce.nonce = 7;
    assert!(!sc.incoming_block_seen(&other_nonce, NOW));
    sc.cleanup_incoming_blocks(NOW + 599);
    assert!(sc.incoming_block_seen(&b, NOW + 599));
    sc.cleanup_incoming_blocks(NOW + 600);
    assert!(!sc.incoming_block_seen(&b, NOW + 600));
    sc.forget_incoming_block(&b);
    assert!(!sc.incoming_block_seen(&b, NOW + 600));
    assert_eq!(sc.miner_count(), 0);
    add(&mut sc, &main, b);
    assert_eq!(sc.miner_count(), 1);
}

#[test]
fn uncle_order_in_templates() {
    // Three siblings of the tip: the template lists them in hash order.
    let main = Main::new(BASE + 100);
    let mut sc = new_chain();
    let a = grow(&mut sc, &main, None, 3, &[1]);
    let mut sib = Vec::new();
    for n in 0..3u8 {
        let mut s = Spec::at(Some(a[1]), 2, 10 + n);
        s.timestamp += 1 + u64::from(n);
        sib.push(add_new(&mut sc, &main, &s));
    }
    let mut t = skeleton(&Spec::at(None, 3, 1));
    sc.fill_sidechain_data(&mut t, &main).unwrap();
    let mut expected = sib.clone();
    expected.sort_by(hash_cmp);
    assert_eq!(t.uncles, expected);
}
