//! Two connection tasks over an in-memory duplex stream (standing in for TCP or a Hyperswarm
//! connection): handshake, tip exchange, peer list with the version record, a block request by
//! id and a broadcast and its notify. Share bytes are opaque here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use xkr_p2pool::p2p::handshake::{HandshakeError, PeerIds};
use xkr_p2pool::p2p::message::PeerEntry;
use xkr_p2pool::p2p::peers::{SUPPORTED_PROTOCOL, VersionRecord};
use xkr_p2pool::p2p::session::{Direction, Domain, Event, PeerError, PeerSession, SessionConfig};
use xkr_p2pool::p2p::{Command, ConnEnd, ConnEvent, run_connection};

const WAIT: Duration = Duration::from_secs(20);

fn config(direction: Direction, consensus_id: [u8; 32], ids: [u64; 4], port: u16) -> SessionConfig {
    SessionConfig {
        consensus_id,
        peer_ids: PeerIds(ids),
        domain: Domain::Hyper,
        direction,
        listen_port: port,
        version: VersionRecord::ours(),
    }
}

/// A connection task and a stand-in owner that answers requests from a tiny share store and
/// forwards every event to the test.
struct Node {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<Event>,
    task: JoinHandle<ConnEnd>,
}

fn start<S>(stream: S, cfg: SessionConfig, conn: u64, tip: &'static [u8]) -> Node
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (ev_tx, mut ev_rx) = mpsc::channel::<ConnEvent>(64);
    let (fwd_tx, fwd_rx) = mpsc::channel(256);
    let session = PeerSession::new(cfg, Instant::now());
    let task = tokio::spawn(run_connection(stream, session, conn, cmd_rx, ev_tx));
    let owner_cmd = cmd_tx.clone();
    tokio::spawn(async move {
        while let Some(ConnEvent { conn: c, event }) = ev_rx.recv().await {
            assert_eq!(c, conn);
            let reply = match &event {
                Event::BlockRequest(None) => Some(Command::RespondBlock {
                    blob: Arc::from(tip),
                    notify: Vec::new(),
                }),
                Event::BlockRequest(Some(id)) => Some(Command::RespondBlock {
                    blob: Arc::from(&id[..4]),
                    notify: vec![[0x77; 32]],
                }),
                Event::PeerListRequest => Some(Command::RespondPeerList {
                    candidates: vec![PeerEntry::from_socket_addr(
                        "1.2.3.4:37889".parse().unwrap(),
                    )],
                    num_connections: 2,
                }),
                _ => None,
            };
            if let Some(r) = reply {
                let _ = owner_cmd.send(r).await;
            }
            if fwd_tx.send(event).await.is_err() {
                break;
            }
        }
    });
    Node {
        commands: cmd_tx,
        events: fwd_rx,
        task,
    }
}

/// The next event matching `want`, skipping others.
async fn until(node: &mut Node, what: &str, want: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(WAIT, async {
        loop {
            let e = node.events.recv().await.expect("connection ended");
            if want(&e) {
                return e;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

#[tokio::test]
async fn two_nodes_exchange_handshake_peers_and_blocks() {
    let cid = [0x5a; 32];
    let (left, right) = tokio::io::duplex(1 << 20);
    let mut a = start(
        left,
        config(Direction::Outgoing, cid, [1, 2, 3, 4], 37889),
        1,
        b"tip of a",
    );
    let mut b = start(
        right,
        config(Direction::Incoming, cid, [5, 6, 7, 8], 37999),
        2,
        b"tip of b",
    );

    // handshake: each learns the other's peer id (Hyperswarm slot) and listen port
    let e = until(&mut a, "peer id", |e| {
        matches!(e, Event::PeerIdentified { .. })
    })
    .await;
    assert_eq!(e, Event::PeerIdentified { peer_id: 8 });
    until(&mut a, "handshake", |e| *e == Event::HandshakeComplete).await;
    until(&mut b, "handshake", |e| *e == Event::HandshakeComplete).await;
    until(&mut a, "listen port", |e| matches!(e, Event::ListenPort(_))).await;
    let e = until(&mut b, "listen port", |e| matches!(e, Event::ListenPort(_))).await;
    assert_eq!(e, Event::ListenPort(37889));

    // tips
    let e = until(&mut a, "tip", |e| matches!(e, Event::BlockResponse { .. })).await;
    assert_eq!(
        e,
        Event::BlockResponse {
            requested: None,
            blob: b"tip of b".to_vec()
        }
    );
    let e = until(&mut b, "tip", |e| matches!(e, Event::BlockResponse { .. })).await;
    assert_eq!(
        e,
        Event::BlockResponse {
            requested: None,
            blob: b"tip of a".to_vec()
        }
    );

    // peer lists, with the version record first
    for node in [&mut a, &mut b] {
        let Event::PeerList(list) = until(node, "peers", |e| matches!(e, Event::PeerList(_))).await
        else {
            unreachable!()
        };
        assert_eq!(list.version, Some(VersionRecord::ours()));
        assert_eq!(list.version.unwrap().protocol, SUPPORTED_PROTOCOL);
        // two connections: one slot, taken by the record
        assert!(list.peers.is_empty());
    }

    // a block by id; the answer carries a notify first (protocol 1.4)
    let wanted = [0x31; 32];
    a.commands
        .send(Command::RequestBlock(wanted))
        .await
        .unwrap();
    until(&mut a, "notify", |e| *e == Event::BlockNotify([0x77; 32])).await;
    let e = until(&mut a, "block", |e| {
        matches!(e, Event::BlockResponse { .. })
    })
    .await;
    assert_eq!(
        e,
        Event::BlockResponse {
            requested: Some(wanted),
            blob: vec![0x31; 4]
        }
    );

    // a broadcast reaches b in full
    let id = [0x99; 32];
    let blob: Arc<[u8]> = Arc::from(&b"a new share"[..]);
    a.commands
        .send(Command::Broadcast {
            id,
            blob: blob.clone(),
        })
        .await
        .unwrap();
    let e = until(&mut b, "broadcast", |e| {
        matches!(e, Event::BlockBroadcast { .. })
    })
    .await;
    assert_eq!(
        e,
        Event::BlockBroadcast {
            blob: blob.to_vec(),
            compact: false
        }
    );

    // b knows a has it: b's relay is only a notify
    b.commands.send(Command::NoteBroadcasted(id)).await.unwrap();
    b.commands
        .send(Command::Broadcast { id, blob })
        .await
        .unwrap();
    let e = until(&mut a, "relay", |e| {
        matches!(e, Event::BlockNotify(_) | Event::BlockBroadcast { .. })
    })
    .await;
    assert_eq!(e, Event::BlockNotify(id));

    // a closes; b sees the stream end
    a.commands.send(Command::Close).await.unwrap();
    let outgoing_end = tokio::time::timeout(WAIT, a.task).await.unwrap().unwrap();
    assert!(matches!(outgoing_end, ConnEnd::ByOwner), "{outgoing_end:?}");
    let incoming_end = tokio::time::timeout(WAIT, b.task).await.unwrap().unwrap();
    assert!(
        matches!(incoming_end, ConnEnd::Closed | ConnEnd::Io(_)),
        "{incoming_end:?}"
    );
    assert!(!incoming_end.bans());
}

#[tokio::test]
async fn other_sidechain_is_banned() {
    let (left, right) = tokio::io::duplex(1 << 16);
    let mut a = start(
        left,
        config(Direction::Outgoing, [1; 32], [1, 2, 3, 4], 37889),
        1,
        b"",
    );
    let b = start(
        right,
        config(Direction::Incoming, [2; 32], [5, 6, 7, 8], 37889),
        2,
        b"",
    );
    let incoming_end = tokio::time::timeout(WAIT, b.task).await.unwrap().unwrap();
    let outgoing_end = tokio::time::timeout(WAIT, a.task).await.unwrap().unwrap();
    let wrong = |e: &ConnEnd| {
        matches!(
            e,
            ConnEnd::Ban(PeerError::Handshake(HandshakeError::WrongSolution))
        )
    };
    assert!(
        wrong(&outgoing_end) || wrong(&incoming_end),
        "{outgoing_end:?} {incoming_end:?}"
    );
    // no handshake ever completed on a
    while let Ok(e) = a.events.try_recv() {
        assert_ne!(e, Event::HandshakeComplete);
    }
}

#[tokio::test]
async fn owner_ban_and_rejected_blocks_end_the_connection() {
    let cid = [3; 32];
    let (left, right) = tokio::io::duplex(1 << 16);
    let mut a = start(
        left,
        config(Direction::Outgoing, cid, [1, 2, 3, 4], 37889),
        1,
        b"x",
    );
    let mut b = start(
        right,
        config(Direction::Incoming, cid, [5, 6, 7, 8], 37889),
        2,
        b"y",
    );
    until(&mut a, "handshake", |e| *e == Event::HandshakeComplete).await;
    until(&mut b, "handshake", |e| *e == Event::HandshakeComplete).await;

    for _ in 0..5 {
        a.commands.send(Command::RejectBlock).await.unwrap();
    }
    let outgoing_end = tokio::time::timeout(WAIT, a.task).await.unwrap().unwrap();
    assert!(
        matches!(outgoing_end, ConnEnd::Ban(PeerError::RejectedBlocks)),
        "{outgoing_end:?}"
    );

    let incoming_end = tokio::time::timeout(WAIT, b.task).await.unwrap().unwrap();
    assert!(!incoming_end.bans(), "{incoming_end:?}");
}
