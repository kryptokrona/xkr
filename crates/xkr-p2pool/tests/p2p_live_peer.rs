//! A live check against a running p2pool node (ignored by default): connects to
//! `XKR_P2POOL_LIVE_PEER` (default `127.0.0.1:37889`) on the default sidechain and reports the
//! connection, handshake and peer state for 60 s.
//!
//! `cargo test -p xkr-p2pool --test p2p_live_peer -- --ignored --nocapture`

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use xkr_consensus::Network;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView};
use xkr_p2pool::p2p::{P2pConfig, P2pEvent, P2pServer, P2pServerConfig, SharedMainChain};
use xkr_p2pool::{Difficulty, ForkHeights, SideChain, SidechainConfig, SidechainOptions};
use xkr_types::Hash;

struct AnyMain {
    height: u64,
}

impl MainChainView for AnyMain {
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
        MainChainTip {
            height: self.height,
            prev_id: Hash::default(),
            difficulty: Difficulty::MAX,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a running p2pool node"]
async fn live_peer_handshake() {
    let peer = std::env::var("XKR_P2POOL_LIVE_PEER").unwrap_or_else(|_| "127.0.0.1:37889".into());
    let options = SidechainOptions::new(
        SidechainConfig::main(),
        ForkHeights::from_params(Network::Mainnet.params()),
        20,
    );
    let sidechain = Arc::new(Mutex::new(SideChain::new(options)));
    // the main-chain height: XKR_P2POOL_LIVE_HEIGHT (the peer's current main-chain tip)
    let height = std::env::var("XKR_P2POOL_LIVE_HEIGHT")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or(0);
    let main: SharedMainChain = Arc::new(AnyMain { height });
    let p2p = P2pConfig {
        external_port: 37889,
        allow_same_ip: true,
        ..P2pConfig::default()
    };
    let mut cfg = P2pServerConfig::new(p2p);
    cfg.add_peers = vec![peer.clone()];
    cfg.clock = Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    });
    let (server, mut events) = P2pServer::start(cfg, Arc::clone(&sidechain), main)
        .await
        .unwrap();
    for second in 0..60 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        while let Ok(e) = events.try_recv() {
            match e {
                P2pEvent::ShareAdded { .. } => {}
                other => eprintln!("[{second:>2}s] event: {other:?}"),
            }
        }
        let s = server.stats();
        if second % 5 == 0 {
            eprintln!(
                "[{second:>2}s] {peer}: out {} in {} good {} connecting {} known {} banned {} \
                 shares {} peers {:?}",
                s.peers_out,
                s.peers_in,
                s.peers_good,
                s.connecting,
                s.known,
                s.banned,
                sidechain.lock().unwrap().len(),
                s.peers
            );
        }
    }
    server.stop().await;
}
