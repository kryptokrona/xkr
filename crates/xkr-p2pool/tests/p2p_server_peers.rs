//! Connection rules of the P2P server over real TCP on loopback addresses: a peer sending
//! garbage is banned, one connection per IP (localhost exempt), and the peer file is read at
//! start and written at stop.
//!
//! The tests connect from 127.0.0.3 to 127.0.0.5, so they run on Linux only (Linux routes all
//! of 127.0.0.0/8 to the loopback interface); only 127.0.0.1 counts as localhost, which is
//! never banned.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use xkr_consensus::Network;
use xkr_p2pool::mainchain::{ChainMain, MainChainTip, MainChainView};
use xkr_p2pool::p2p::peerfile::{PEER_FILE_NAME, parse_peer_list};
use xkr_p2pool::p2p::{P2pConfig, P2pEvent, P2pServer, P2pServerConfig, SharedMainChain};
use xkr_p2pool::{ForkHeights, SideChain, SidechainConfig, SidechainOptions};
use xkr_types::Hash;

const WAIT: Duration = Duration::from_secs(20);
/// `HANDSHAKE_CHALLENGE`: id, challenge, peer id.
const CHALLENGE_LEN: usize = 1 + 8 + 8;

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

fn sidechain() -> Arc<Mutex<SideChain>> {
    Arc::new(Mutex::new(SideChain::new(SidechainOptions::new(
        SidechainConfig::main(),
        ForkHeights::from_params(Network::Mainnet.params()),
        20,
    ))))
}

fn main_chain() -> SharedMainChain {
    Arc::new(NoMain)
}

/// A listening server that dials nobody.
async fn listening() -> (P2pServer, tokio::sync::mpsc::UnboundedReceiver<P2pEvent>) {
    let cfg = P2pServerConfig::new(P2pConfig {
        listen: Some("127.0.0.1:0".parse().unwrap()),
        out_peers: 0,
        ..P2pConfig::default()
    });
    P2pServer::start(cfg, sidechain(), main_chain())
        .await
        .unwrap()
}

async fn connect_from(ip: &str, to: SocketAddr) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket
        .bind(SocketAddr::new(ip.parse().unwrap(), 0))
        .unwrap();
    socket.connect(to).await.unwrap()
}

/// Reads the server's handshake challenge.
async fn expect_challenge(s: &mut TcpStream) {
    let mut buf = [0u8; CHALLENGE_LEN];
    tokio::time::timeout(WAIT, s.read_exact(&mut buf))
        .await
        .expect("no challenge")
        .expect("connection closed instead of a challenge");
    assert_eq!(buf[0], 0);
}

/// Reads until the server closes the connection.
async fn expect_closed(s: &mut TcpStream) {
    let mut buf = [0u8; 256];
    tokio::time::timeout(WAIT, async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .expect("connection still open");
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn garbage_bans_the_sender() {
    let (server, mut events) = listening().await;
    let addr = server.local_addr().unwrap();

    let mut s = connect_from("127.0.0.3", addr).await;
    expect_challenge(&mut s).await;
    s.write_all(&[0xff, 1, 2, 3]).await.unwrap();
    expect_closed(&mut s).await;
    wait_for("the ban", || server.stats().banned == 1).await;
    let e = events.recv().await.unwrap();
    let P2pEvent::PeerRejected { peer, reason } = e else {
        panic!("{e:?}");
    };
    assert_eq!(peer.to_string().split(':').next(), Some("127.0.0.3"));
    assert!(reason.contains("unknown message id"), "{reason}");

    // The banned address is closed at once, another one still gets a challenge.
    let mut again = connect_from("127.0.0.3", addr).await;
    expect_closed(&mut again).await;
    let mut other = connect_from("127.0.0.5", addr).await;
    expect_challenge(&mut other).await;

    server.stop().await;
}

#[tokio::test]
async fn one_connection_per_ip() {
    let (server, _events) = listening().await;
    let addr = server.local_addr().unwrap();

    let mut first = connect_from("127.0.0.4", addr).await;
    expect_challenge(&mut first).await;
    wait_for("the first connection", || server.stats().peers_in == 1).await;
    let mut second = connect_from("127.0.0.4", addr).await;
    expect_closed(&mut second).await;
    assert_eq!(server.stats().peers_in, 1);

    // localhost may connect more than once
    let mut local_one = connect_from("127.0.0.1", addr).await;
    let mut local_two = connect_from("127.0.0.1", addr).await;
    expect_challenge(&mut local_one).await;
    expect_challenge(&mut local_two).await;
    wait_for("three connections", || server.stats().peers_in == 3).await;
    assert_eq!(server.stats().banned, 0);

    server.stop().await;
}

#[tokio::test]
async fn peer_file_round_trip() {
    let dir = std::env::temp_dir().join(format!("xkr-p2pool-peerfile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(PEER_FILE_NAME);
    let text = "1.2.3.4:37889\n[2001:db8::1]:37890\n5.6.7.8:20211\n127.0.0.1:37889\n";
    std::fs::write(&path, text).unwrap();

    let mut cfg = P2pServerConfig::new(P2pConfig {
        out_peers: 0,
        ..P2pConfig::default()
    });
    cfg.peer_file = Some(path.clone());
    let (server, _events) = P2pServer::start(cfg, sidechain(), main_chain())
        .await
        .unwrap();
    // localhost is never kept in the peer list
    assert_eq!(server.stats().known, 3);
    server.stop().await;

    let saved = std::fs::read_to_string(&path).unwrap();
    let got: HashSet<SocketAddr> = parse_peer_list(&saved)
        .iter()
        .map(xkr_p2pool::p2p::PeerEntry::socket_addr)
        .collect();
    let want: HashSet<SocketAddr> = ["1.2.3.4:37889", "[2001:db8::1]:37890", "5.6.7.8:20211"]
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(got, want);
    assert_eq!(saved.lines().count(), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}
