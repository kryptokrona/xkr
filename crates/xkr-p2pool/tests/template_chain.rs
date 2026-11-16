//! Block templates on a synthetic sidechain: the coinbase before and after fork 14 (version 2,
//! tagged outputs, view tags the payee finds), the hashing blob per extra nonce against the
//! node's own block decoding, and the round trip template -> found share -> parsed by a peer
//! -> verified and taken as the tip. Also the stratum side ([`PoolTemplates`]): blobs, kept
//! templates, found blocks and shares.

use std::sync::{Arc, Mutex, RwLock};

use rand_core::RngCore;
use xkr_crypto::{derive_public_key, generate_key_derivation, view_tag};
use xkr_p2pool::block::{HASHING_BLOB_MIN_SIZE, OffchainState};
use xkr_p2pool::keys::get_tx_keys;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView};
use xkr_p2pool::pplns::{decompose_outputs, split_reward};
use xkr_p2pool::stratum::{ShareSink, TemplateSource};
use xkr_p2pool::template::{
    BlockTemplate, FoundBlock, MempoolTx, OLD_TEMPLATES, PoolNode, PoolTemplates, SubmitError,
    Template, TemplateMinerData, own_share,
};
use xkr_p2pool::varint::varint_len;
use xkr_p2pool::{
    Difficulty, ForkHeights, MinerWallet, PoolBlock, SideChain, SidechainConfig, SidechainEvent,
    SidechainOptions, StaticView, parse_pool_block,
};
use xkr_serialization::{block_pow_blob, decode_block};
use xkr_types::{Hash, SecretKey, TxInput, TxOutputTarget};

const UNLOCK_WINDOW: u64 = 20;
const NOW: u64 = 1_800_000_000;
const GENERATED: u64 = 50_000_000_000_000;
const MEDIAN: u64 = 100_000;

/// Every main-chain height has the same difficulty.
struct FixedMain(Difficulty);

impl MainChainView for FixedMain {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&height.to_le_bytes());
        Some(ChainMain {
            difficulty: self.0,
            height,
            timestamp: NOW,
            reward: 0,
            id: Hash(id),
        })
    }
    fn block_by_hash(&self, _id: &Hash) -> Option<ChainMain> {
        None
    }
    fn tip(&self) -> MainChainTip {
        MainChainTip::default()
    }
}

/// A deterministic random source.
struct Counter(u64);

impl RngCore for Counter {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.0 ^ (self.0 >> 29)
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        for b in dst {
            *b = self.next_u64() as u8;
        }
    }
}

/// A payout wallet and its view secret key.
fn wallet_from(n: u8) -> (MinerWallet, SecretKey) {
    let (spend, _) = get_tx_keys(&Hash([n; 32]), &Hash([1; 32]));
    let (view, view_sec) = get_tx_keys(&Hash([n.wrapping_add(1); 32]), &Hash([2; 32]));
    (MinerWallet::assign(spend, view).unwrap(), view_sec)
}

fn wallet() -> (MinerWallet, SecretKey) {
    wallet_from(0x51)
}

fn forks(v2: bool) -> ForkHeights {
    if v2 {
        ForkHeights {
            fork13: 0,
            fork14: 100,
        }
    } else {
        ForkHeights::default()
    }
}

fn sidechain(v2: bool) -> SideChain {
    SideChain::new(SidechainOptions::new(
        SidechainConfig::main(),
        forks(v2),
        UNLOCK_WINDOW,
    ))
}

fn view(sc: &SideChain) -> StaticView {
    StaticView {
        consensus_id: *sc.consensus_id(),
        forks: sc.options().forks,
    }
}

/// Three old transactions with fees and one too new to be taken.
fn backlog() -> Vec<MempoolTx> {
    let mut txs: Vec<MempoolTx> = (0..3u8)
        .map(|i| MempoolTx {
            id: Hash([0x10 + i; 32]),
            weight: 2000,
            fee: 10_000,
            time_received: NOW - 100,
        })
        .collect();
    txs.push(MempoolTx {
        id: Hash([0x20; 32]),
        weight: 2000,
        fee: 10,
        time_received: NOW,
    });
    txs
}

fn miner_data(v2: bool, height: u64) -> TemplateMinerData {
    let mut prev = [0u8; 32];
    prev[..8].copy_from_slice(&height.to_le_bytes());
    prev[31] = 0xaa;
    TemplateMinerData {
        major_version: if v2 { 7 } else { 5 },
        minor_version: if v2 { 14 } else { 13 },
        height,
        prev_id: Hash(prev),
        difficulty: Difficulty::from_u64(1_000_000_000),
        median_weight: MEDIAN,
        already_generated_coins: GENERATED,
        median_timestamp: NOW - 1000,
        tx_backlog: backlog(),
    }
}

/// The coinbase of a template as the node decodes it: version, unlock time, input, outputs in
/// PPLNS order (the outputs of `payee` found with its view key: one-time keys, and view tags
/// in version 2), the extra (tx key, extra nonce, share id), the transaction list; the reward
/// is the daemon's.
fn check_coinbase(
    t: &Template,
    v2: bool,
    payee: (&MinerWallet, &SecretKey),
    nonce: u32,
    extra_nonce: u32,
) {
    let block = decode_block(&t.mainchain_blob(nonce, extra_nonce)).unwrap();
    let tx = &block.miner_tx.prefix;
    assert_eq!(tx.version, if v2 { 2 } else { 1 });
    assert_eq!(tx.unlock_time, t.height() + UNLOCK_WINDOW);
    assert_eq!(
        tx.inputs,
        [TxInput::Coinbase {
            height: t.height() as u32
        }]
    );
    assert_eq!(block.header.major_version, if v2 { 7 } else { 5 });
    assert_eq!(block.header.minor_version, if v2 { 14 } else { 13 });
    assert_eq!(block.header.prev_hash, t.block.prev_id);
    assert_eq!(block.tx_hashes, t.block.transactions[1..]);

    let (wallet, view_sec) = payee;
    let d = generate_key_derivation(&t.block.txkey_pub, view_sec).unwrap();
    let rewards = split_reward(t.reward, &t.shares).unwrap();
    let expected = decompose_outputs(&t.shares, &rewards);
    assert_eq!(expected.len(), tx.outputs.len());
    let mut total = 0u64;
    let mut found = 0;
    for (i, (out, (amount, to))) in tx.outputs.iter().zip(&expected).enumerate() {
        assert_eq!(out.amount, *amount, "amount {i}");
        total += out.amount;
        if to != wallet {
            continue;
        }
        found += 1;
        let index = i as u64;
        let key = derive_public_key(&d, index, &wallet.spend).unwrap();
        match out.target {
            TxOutputTarget::TaggedKey {
                key: k,
                view_tag: tag,
            } => {
                assert!(v2, "tagged output in a version-1 coinbase");
                assert_eq!(k, key, "output {i}");
                assert_eq!(tag, view_tag(&d, index), "view tag {i}");
                assert_eq!(tag, t.block.outputs[i].view_tag);
            }
            TxOutputTarget::Key(k) => {
                assert!(!v2, "untagged output in a version-2 coinbase");
                assert_eq!(k, key, "output {i}");
                assert_eq!(t.block.outputs[i].view_tag, 0);
            }
        }
    }
    assert!(found > 0, "no output for the payee");
    assert_eq!(total, t.reward);

    let extra = &tx.extra;
    assert_eq!(extra.len(), 71);
    assert_eq!(extra[0], 1);
    assert_eq!(extra[1..33], t.block.txkey_pub.0);
    assert_eq!(extra[33..35], [2, 36]);
    assert_eq!(extra[35..39], extra_nonce.to_le_bytes());
    assert_eq!(extra[39..71], t.sidechain_id().0);

    // the daemon's reward rule for the block's own size
    let (reward, _) = xkr_consensus::reward::get_block_reward(
        block.header.major_version,
        MEDIAN,
        t.block_weight,
        GENERATED,
        t.fees,
    )
    .unwrap();
    assert_eq!(t.reward, reward);
}

#[test]
fn coinbase_before_and_after_fork_14() {
    let main = FixedMain(Difficulty::from_u64(1_000_000_000));
    let (wallet, view_sec) = wallet();
    for v2 in [false, true] {
        let sc = sidechain(v2);
        let mut bt = BlockTemplate::new();
        let t = bt
            .update_with_rng(
                &miner_data(v2, 200),
                &sc,
                &main,
                &wallet,
                NOW,
                &mut Counter(1),
            )
            .unwrap();
        assert_eq!(t.id, 1);
        // genesis share of an empty sidechain
        assert_eq!(t.sidechain_height(), 0);
        assert!(t.block.parent.is_zero());
        assert_eq!(t.block.difficulty, sc.config().min_difficulty());
        assert_eq!(t.block.txkey_sec_seed, *sc.consensus_id());
        assert_eq!(t.block.timestamp, NOW);
        assert_eq!(
            t.block.sidechain_extra[..2],
            [xkr_p2pool::SOFTWARE_ID, xkr_p2pool::SOFTWARE_VERSION]
        );
        // the three old transactions, not the new one
        assert_eq!(t.block.transactions.len(), 4);
        assert!(!t.block.transactions.contains(&Hash([0x20; 32])));
        assert_eq!(t.fees, 30_000);
        assert_eq!(
            t.block_weight,
            t.block.real_coinbase(UNLOCK_WINDOW, 0).len() as u64 + 6000
        );
        check_coinbase(&t, v2, (&wallet, &view_sec), 0, 0);
        check_coinbase(&t, v2, (&wallet, &view_sec), 0x0102_0304, 0xa0b0_c0d0);

        // what a peer reads is the share as built
        let share = t.share(11, 22);
        let parsed = parse_pool_block(&share.serialize(UNLOCK_WINDOW), &view(&sc)).unwrap();
        assert_eq!(parsed, share);
        assert_eq!(parsed.sidechain_id, *t.sidechain_id());
        assert_eq!(parsed.sidechain_extra[3], 22);
    }
}

#[test]
fn timestamp_after_the_median() {
    let main = FixedMain(Difficulty::from_u64(1_000_000_000));
    let (wallet, _) = wallet();
    let sc = sidechain(true);
    let mut data = miner_data(true, 200);
    data.median_timestamp = NOW + 50;
    let t = BlockTemplate::new()
        .update_with_rng(&data, &sc, &main, &wallet, NOW, &mut Counter(2))
        .unwrap();
    assert_eq!(t.block.timestamp, NOW + 51);

    // a major version the sidechain does not expect at this height
    data.major_version = 5;
    assert!(
        BlockTemplate::new()
            .update_with_rng(&data, &sc, &main, &wallet, NOW, &mut Counter(2))
            .is_err()
    );
}

#[test]
fn hashing_blob_per_extra_nonce() {
    let main = FixedMain(Difficulty::from_u64(1_000_000_000));
    let (wallet, _) = wallet();
    for v2 in [false, true] {
        let sc = sidechain(v2);
        let t = BlockTemplate::new()
            .update_with_rng(
                &miner_data(v2, 200),
                &sc,
                &main,
                &wallet,
                NOW,
                &mut Counter(3),
            )
            .unwrap();
        let offset = 2 + varint_len(t.block.timestamp) + 32;
        let first = t.hashing_blob(0);
        assert_eq!(first.nonce_offset, offset);
        assert_eq!(offset, 39);
        assert_eq!(first.blob.len(), HASHING_BLOB_MIN_SIZE);
        assert_eq!(first.template_id, t.id);
        assert_eq!(first.height, 200);
        assert_eq!(first.sidechain_height, 0);
        assert_eq!(first.sidechain_difficulty, t.block.difficulty);
        assert_eq!(first.mainchain_difficulty, t.mainchain_difficulty);
        assert!(first.seed_hash.is_zero());
        for extra_nonce in [0u32, 1, 0xdead_beef, u32::MAX] {
            let hb = t.hashing_blob(extra_nonce);
            assert_eq!(hb.nonce_offset, offset);
            assert_eq!(hb.blob[offset..offset + 4], [0; 4]);
            // only the parent coinbase hash after the nonce depends on the extra nonce
            assert_eq!(hb.blob[..offset + 4], first.blob[..offset + 4]);
            assert_eq!(hb.blob[offset + 36..], first.blob[offset + 36..]);
            if extra_nonce != 0 {
                assert_ne!(
                    hb.blob[offset + 4..offset + 36],
                    first.blob[offset + 4..offset + 36]
                );
            }
            for nonce in [0u32, 7, 0x8000_0001] {
                let mut blob = hb.blob.clone();
                blob[offset..offset + 4].copy_from_slice(&nonce.to_le_bytes());
                // the node's proof-of-work input of the submitted block
                let block = decode_block(&t.mainchain_blob(nonce, extra_nonce)).unwrap();
                assert_eq!(block_pow_blob(&block).unwrap(), blob);
                // and the share's, as a peer computes it
                let share = t.share(nonce, extra_nonce);
                assert_eq!(share.pow_blob(UNLOCK_WINDOW, extra_nonce, nonce).blob, blob);
            }
        }
    }
}

/// Built on a synthetic chain: each template's share goes into the own sidechain, the next
/// template builds on it; a second sidechain gets the same shares as parsed blobs (as a peer
/// would) and verifies every one with the full rules.
#[test]
fn round_trip_through_a_peer() {
    let main = FixedMain(Difficulty::from_u64(1_000_000_000));
    let (wallet, view_sec) = wallet();
    let (other, other_view_sec) = wallet_from(0x61);
    for v2 in [false, true] {
        let mut own = sidechain(v2);
        let mut peer = sidechain(v2);
        let peer_view = view(&peer);
        let mut bt = BlockTemplate::new();
        let mut rng = Counter(4);
        let mut ids = Vec::new();
        for i in 0..6u64 {
            // two main-chain blocks per height pair: the tx key seed follows both rules
            let data = miner_data(v2, 200 + i / 2);
            let payee = if i % 3 == 2 { &other } else { &wallet };
            let t = bt
                .update_with_rng(&data, &own, &main, payee, NOW + i, &mut rng)
                .unwrap();
            assert_eq!(t.sidechain_height(), i);
            if let Some(prev) = ids.last() {
                assert_eq!(t.block.parent, *prev);
            }
            check_coinbase(&t, v2, (&wallet, &view_sec), 5, 6);
            if i >= 2 {
                check_coinbase(&t, v2, (&other, &other_view_sec), 5, 6);
            }
            let found = own_share(&t, 100 + i as u32, 200 + i as u32);
            assert!(found.offchain.verified);
            own.add_block(found.clone(), &main, NOW + i).unwrap();
            assert_eq!(
                own.chain_tip().map(|b| b.sidechain_id),
                Some(*t.sidechain_id())
            );
            let events = own.take_events();
            assert!(events.contains(&SidechainEvent::Broadcast(*t.sidechain_id())));

            let blob = found.serialize(UNLOCK_WINDOW);
            let parsed = parse_pool_block(&blob, &peer_view).unwrap();
            assert_eq!(parsed, local_state_cleared(found), "share {i}");
            peer.add_block(parsed, &main, NOW + i).unwrap();
            let events = peer.take_events();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, SidechainEvent::Invalid { .. })),
                "share {i}: {events:?}"
            );
            let b = peer.find_block(t.sidechain_id()).unwrap();
            assert!(b.offchain.verified && !b.offchain.invalid, "share {i}");
            assert_eq!(
                peer.chain_tip().map(|b| b.sidechain_id),
                Some(*t.sidechain_id())
            );
            ids.push(*t.sidechain_id());
        }
        // both wallets are paid in the last window
        let last = own.chain_tip().unwrap().clone();
        let window = own.get_shares(&last, &main).unwrap();
        assert_eq!(window.shares.len(), 2);
        assert_eq!(bt.current().map(|t| t.id), Some(6));
        assert!(bt.get(6 - OLD_TEMPLATES as u32).is_some());
        assert!(bt.get(1).is_none());
    }
}

/// The off-chain state of an own share is local: a parsed share starts fresh.
fn local_state_cleared(mut b: PoolBlock) -> PoolBlock {
    b.offchain = OffchainState::default();
    b
}

/// A node with one sidechain, recording what the template hands back.
struct TestNode {
    sidechain: RwLock<SideChain>,
    main: FixedMain,
    events: Mutex<Vec<SidechainEvent>>,
    blocks: Mutex<Vec<FoundBlock>>,
}

impl PoolNode for TestNode {
    fn read_sidechain(&self, f: &mut dyn FnMut(&SideChain, &dyn MainChainView)) {
        f(&self.sidechain.read().unwrap(), &self.main);
    }
    fn write_sidechain(&self, f: &mut dyn FnMut(&mut SideChain, &dyn MainChainView)) {
        f(&mut self.sidechain.write().unwrap(), &self.main);
    }
    fn on_sidechain_events(&self, events: Vec<SidechainEvent>) {
        self.events.lock().unwrap().extend(events);
    }
    fn submit_mainchain_block(&self, block: FoundBlock) {
        self.blocks.lock().unwrap().push(block);
    }
}

#[test]
fn stratum_side() {
    let node = Arc::new(TestNode {
        sidechain: RwLock::new(sidechain(true)),
        main: FixedMain(Difficulty::from_u64(1_000_000_000)),
        events: Mutex::new(Vec::new()),
        blocks: Mutex::new(Vec::new()),
    });
    let templates = PoolTemplates::new(node.clone());
    let (wallet, _) = wallet();
    assert!(templates.hashing_blob(0).is_none());
    assert!(templates.hashing_blobs(0, 4).is_empty());
    assert_eq!(templates.sidechain_tip_height(), None);
    assert_eq!(templates.chain_window_size(), 2160);
    assert_eq!(
        templates.sidechain_difficulty(),
        SidechainConfig::main().min_difficulty()
    );
    assert!(!TemplateSource::disconnected(&templates));

    let t = templates
        .update(&miner_data(true, 300), &wallet, NOW)
        .unwrap();
    assert_eq!(templates.last_updated(), NOW);
    let blobs = templates.hashing_blobs(u32::MAX, 3);
    assert_eq!(blobs.len(), 3);
    for (b, extra_nonce) in blobs.iter().zip([u32::MAX, 0, 1]) {
        assert_eq!(*b, t.hashing_blob(extra_nonce));
    }
    assert_eq!(templates.hashing_blob_for(t.id, 9), Some(t.hashing_blob(9)));
    assert_eq!(templates.difficulties(t.id), Some(t.difficulties()));

    // a main-chain block
    templates.submit_block(t.id, 1234, 5678);
    {
        let blocks = node.blocks.lock().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].blob, t.mainchain_blob(1234, 5678));
        assert_eq!(blocks[0].height, 300);
        assert_eq!(blocks[0].sidechain_id, *t.sidechain_id());
    }

    // a share: into the sidechain once, the events to the node
    assert_eq!(
        templates.submit_share_at(t.id, 1234, 5678, NOW),
        Ok(*t.sidechain_id())
    );
    assert_eq!(
        templates.submit_share_at(t.id, 1234, 5678, NOW),
        Err(SubmitError::AlreadySubmitted)
    );
    {
        let events = node.events.lock().unwrap();
        assert!(events.contains(&SidechainEvent::Broadcast(*t.sidechain_id())));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SidechainEvent::NewTip { id, .. } if id == t.sidechain_id()))
        );
    }
    assert_eq!(templates.sidechain_tip_height(), Some(0));
    let stored = node
        .sidechain
        .read()
        .unwrap()
        .find_block(t.sidechain_id())
        .cloned()
        .unwrap();
    assert_eq!((stored.nonce, stored.extra_nonce), (1234, 5678));
    assert_eq!(stored.sidechain_extra[3], 5678);

    // later templates build on it; the first one is kept for OLD_TEMPLATES updates
    let later: Vec<Arc<Template>> = (1..=OLD_TEMPLATES as u64)
        .map(|i| {
            let next = templates
                .update(&miner_data(true, 300 + i), &wallet, NOW + i)
                .unwrap();
            assert!(templates.difficulties(t.id).is_some());
            next
        })
        .collect();
    let last = later.last().unwrap();
    assert_eq!(last.block.parent, *t.sidechain_id());
    assert_eq!(templates.current().map(|c| c.id), Some(last.id));
    templates
        .update(&miner_data(true, 400), &wallet, NOW + 9)
        .unwrap();
    assert!(templates.hashing_blob_for(t.id, 0).is_none());
    assert!(templates.difficulties(t.id).is_none());
    assert_eq!(
        templates.submit_share_at(t.id, 1, 2, NOW + 9),
        Err(SubmitError::UnknownTemplate(t.id))
    );
    templates.submit_block(t.id, 1, 2);
    assert_eq!(node.blocks.lock().unwrap().len(), 1);

    // a share of a kept older template still counts
    let older = last.id - 1;
    assert!(ShareSink::submit_share(&templates, older, 77, 88));
}
