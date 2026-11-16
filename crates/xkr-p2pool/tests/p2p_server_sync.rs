//! Two P2P servers syncing a piece of the mainnet sidechain: server A holds the 61 fixture
//! shares, server B none. A announces its top share; B asks for every parent and uncle in turn
//! and ends with the whole chain below the top share that A has. Once over TCP on localhost,
//! once over an in-memory stream attached as a Hyperswarm connection.
//!
//! The fixture is a slice: its lowest share's parent is not in it, so neither side can verify
//! the chain or pick a chain tip. The test compares the stored shares.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use xkr_consensus::Network;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView};
use xkr_p2pool::p2p::{
    Direction, P2pConfig, P2pEvent, P2pServer, P2pServerConfig, SharedMainChain, SharedSideChain,
    Transport,
};
use xkr_p2pool::{
    Difficulty, ForkHeights, PoolBlock, SideChain, SidechainConfig, SidechainOptions, StaticView,
    parse_pool_block,
};
use xkr_types::Hash;

/// `CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` on mainnet.
const UNLOCK_WINDOW: u64 = 20;
const WAIT: Duration = Duration::from_secs(120);

/// A main chain that knows every height (with an unreachable difficulty, so no share counts
/// as a main-chain block) and whose top is the block the top fixture share builds on.
struct TestMain {
    tip: MainChainTip,
}

impl MainChainView for TestMain {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        Some(ChainMain {
            difficulty: Difficulty::MAX,
            height,
            ..ChainMain::default()
        })
    }
    fn block_by_hash(&self, _id: &Hash) -> Option<ChainMain> {
        None
    }
    fn tip(&self) -> MainChainTip {
        self.tip
    }
}

fn options() -> SidechainOptions {
    SidechainOptions::new(
        SidechainConfig::main(),
        ForkHeights::from_params(Network::Mainnet.params()),
        UNLOCK_WINDOW,
    )
}

/// The fixture shares, lowest first.
fn fixture() -> Vec<PoolBlock> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/shares-mainnet.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let view = StaticView {
        consensus_id: SidechainConfig::main().consensus_id(),
        forks: ForkHeights::from_params(Network::Mainnet.params()),
    };
    let mut blocks: Vec<PoolBlock> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let row: serde_json::Value = serde_json::from_str(l).unwrap();
            let blob = hex::decode(row["serialized"].as_str().unwrap()).unwrap();
            parse_pool_block(&blob, &view).unwrap()
        })
        .collect();
    blocks.sort_by_key(|b| b.sidechain_height);
    blocks
}

/// The fixture shares reachable from `top` through parents and uncles.
fn reachable(blocks: &[PoolBlock], top: Hash) -> HashSet<Hash> {
    let mut seen = HashSet::new();
    let mut todo = vec![top];
    while let Some(id) = todo.pop() {
        let Some(b) = blocks.iter().find(|b| b.sidechain_id == id) else {
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        todo.push(b.parent);
        todo.extend(b.uncles.iter().copied());
    }
    seen
}

struct Setup {
    blocks: Vec<PoolBlock>,
    top: PoolBlock,
    main: SharedMainChain,
    full: SharedSideChain,
    empty: SharedSideChain,
}

fn setup() -> Setup {
    let blocks = fixture();
    let top = blocks.last().unwrap().clone();
    let main: SharedMainChain = Arc::new(TestMain {
        tip: MainChainTip {
            height: top.txin_gen_height,
            prev_id: top.prev_id,
            difficulty: Difficulty::MAX,
        },
    });
    let mut sc = SideChain::new(options());
    for b in &blocks {
        sc.add_block(b.clone(), &*main, top.timestamp).unwrap();
    }
    assert_eq!(sc.len(), blocks.len());
    Setup {
        blocks,
        top,
        main,
        full: Arc::new(Mutex::new(sc)),
        empty: Arc::new(Mutex::new(SideChain::new(options()))),
    }
}

/// Server settings on a fixed clock (the time of the top share), without seeds.
fn config(listen: bool, add_peers: Vec<String>, now: u64) -> P2pServerConfig {
    let p2p = P2pConfig {
        listen: if listen {
            Some("127.0.0.1:0".parse().unwrap())
        } else {
            None
        },
        external_port: 37889,
        ..P2pConfig::default()
    };
    let mut cfg = P2pServerConfig::new(p2p);
    cfg.add_peers = add_peers;
    cfg.clock = Arc::new(move || now);
    cfg
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A announces the top share; B must end with every share reachable from it.
async fn sync_and_check(
    s: &Setup,
    a: &P2pServer,
    b: &P2pServer,
    client_events: &mut tokio::sync::mpsc::UnboundedReceiver<P2pEvent>,
) -> Option<Transport> {
    wait_for("both peers good", || {
        a.stats().peers_good == 1 && b.stats().peers_good == 1
    })
    .await;
    a.broadcast(s.top.sidechain_id);

    let want = reachable(&s.blocks, s.top.sidechain_id);
    assert!(want.len() > 1);
    wait_for("B to hold the chain", || {
        let sc = s.empty.lock().unwrap();
        want.iter().all(|id| sc.find_block(id).is_some())
    })
    .await;

    let sc = s.empty.lock().unwrap();
    assert_eq!(sc.len(), want.len());
    let highest = sc.blocks().max_by_key(|b| b.sidechain_height).unwrap();
    assert_eq!(highest.sidechain_id, s.top.sidechain_id);
    drop(sc);

    let mut added = HashSet::new();
    let mut from = None;
    while let Ok(e) = client_events.try_recv() {
        assert!(!matches!(e, P2pEvent::PeerRejected { .. }), "{e:?}");
        if let P2pEvent::ShareAdded { id, from: f, .. } = e {
            added.insert(id);
            from = from.or(f);
        }
    }
    assert_eq!(added, want);
    assert_eq!(a.stats().banned, 0);
    assert_eq!(b.stats().banned, 0);
    from
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_peer_syncs_the_chain() {
    let s = setup();
    let now = s.top.timestamp;
    let (a, _full_events) = P2pServer::start(
        config(true, Vec::new(), now),
        Arc::clone(&s.full),
        Arc::clone(&s.main),
    )
    .await
    .unwrap();
    let addr = a.local_addr().unwrap();
    let (b, mut client_events) = P2pServer::start(
        config(false, vec![addr.to_string()], now),
        Arc::clone(&s.empty),
        Arc::clone(&s.main),
    )
    .await
    .unwrap();
    assert_eq!(b.local_addr(), None);

    let from = sync_and_check(&s, &a, &b, &mut client_events).await;
    assert_eq!(from, Some(Transport::Tcp(addr)));
    let listener = a.stats();
    assert_eq!(
        (listener.peers_in, listener.peers_out, listener.peers_hyper),
        (1, 0, 0)
    );
    let dialer = b.stats();
    assert_eq!(
        (dialer.peers_in, dialer.peers_out, dialer.peers_hyper),
        (0, 1, 0)
    );
    wait_for("A's version record", || {
        b.stats()
            .peers
            .first()
            .is_some_and(|p| p.software.is_some())
    })
    .await;
    let software = b.stats().peers[0].software.unwrap();
    assert_eq!(software.software_id, xkr_p2pool::SOFTWARE_ID);

    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attached_stream_syncs_the_chain() {
    let s = setup();
    let now = s.top.timestamp;
    let (a, _full_events) = P2pServer::start(
        config(false, Vec::new(), now),
        Arc::clone(&s.full),
        Arc::clone(&s.main),
    )
    .await
    .unwrap();
    let (b, mut client_events) = P2pServer::start(
        config(false, Vec::new(), now),
        Arc::clone(&s.empty),
        Arc::clone(&s.main),
    )
    .await
    .unwrap();

    let (left, right) = tokio::io::duplex(1 << 20);
    a.attach(left, Direction::Outgoing, [0xbb; 32]);
    b.attach(right, Direction::Incoming, [0xaa; 32]);

    let from = sync_and_check(&s, &a, &b, &mut client_events).await;
    assert_eq!(from, Some(Transport::Hyper([0xaa; 32])));
    assert_eq!(a.stats().peers_hyper, 1);
    assert_eq!(b.stats().peers_hyper, 1);
    assert_eq!(a.stats().peers_in + a.stats().peers_out, 0);

    // A second stream from the same key is closed at once.
    let (dup, mut other_end) = tokio::io::duplex(1024);
    a.attach(dup, Direction::Outgoing, [0xbb; 32]);
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(WAIT, other_end.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(a.stats().peers_hyper, 1);

    a.stop().await;
    b.stop().await;
}
