//! Bridge lifecycle against a fake worker written in Rust over an in-memory byte stream (no Bare
//! worker needed): what xkrd's supervisor relies on.
//!
//! * a [`CloseHandle`] closes a connection whose `HyperConn` is owned elsewhere (boxed into a
//!   Levin peer task);
//! * when the worker dies, `incoming()` yields one `Err` and then ends, `wait_closed` resolves,
//!   and `announce`/`lookup`/`open` fail immediately instead of waiting for their timeouts;
//! * `HyperConn` boxes into `xkr_p2p_legacy::BoxedPeerStream`.

use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_util::codec::Framed;
use xkr_p2p_hyper::frame::{
    Body, ConnId, DEFAULT_INITIAL_CREDIT, DEFAULT_MAX_FRAME, Frame, FrameCodec, Hello, Opened, Tlv,
    VERSION_MAJOR, VERSION_MINOR, WireAddr, features, tlv_tag,
};
use xkr_p2p_hyper::{AnnounceMode, Bridge, BridgeConfig, CloseReason, HyperConn, OpenFlags};

const NODE_KEY: [u8; 32] = [1; 32];
const REMOTE_KEY: [u8; 32] = [2; 32];

/// Compile-time check: a `HyperConn` is a legacy `BoxedPeerStream`.
fn _boxes_as_peer_stream(conn: HyperConn) -> xkr_p2p_legacy::BoxedPeerStream {
    Box::new(conn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_handle_and_terminal_bridge_failure() {
    let (client, worker_io) = tokio::io::duplex(1 << 20);
    let inbound_id = ConnId::new(true, 0, 1).0;
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let (kill_tx, kill_rx) = oneshot::channel();
    // The fake worker: does the Hello exchange, announces one inbound connection, then forwards
    // every frame it receives until `kill` fires and drops its end of the stream.
    let server = tokio::spawn(async move {
        let seen = seen_tx;
        let mut kill = kill_rx;
        let mut framed = Framed::new(worker_io, FrameCodec::new(DEFAULT_MAX_FRAME));
        let hello = framed.next().await.expect("client hello").expect("decode");
        assert!(
            matches!(hello.body, Body::Hello(_)),
            "first frame must be Hello"
        );
        framed
            .send(Frame::control(Body::Hello(Hello {
                major: VERSION_MAJOR,
                minor: VERSION_MINOR,
                max_frame: DEFAULT_MAX_FRAME,
                initial_credit: DEFAULT_INITIAL_CREDIT,
                workers: 1,
                worker: 0,
                features: features::HUGIN_MID,
                tlvs: vec![Tlv::new(tlv_tag::PUBLIC_KEY, NODE_KEY.to_vec())],
            })))
            .await
            .expect("send hello");
        framed
            .send(Frame::conn(
                inbound_id,
                Body::Opened(Box::new(Opened {
                    request_id: 0,
                    remote_public_key: REMOTE_KEY,
                    handshake_hash: [3; 64],
                    topic: [0; 32],
                    initiator: false,
                    addr: WireAddr::NONE,
                    tlvs: Vec::new(),
                })),
            ))
            .await
            .expect("send opened");
        loop {
            tokio::select! {
                _ = &mut kill => break,
                f = framed.next() => match f {
                    Some(Ok(f)) => {
                        let _ = seen.send(f);
                    }
                    _ => break,
                },
            }
        }
        // Dropping `framed` closes the stream: the worker "died".
    });

    let bridge = Bridge::connect_stream(client, BridgeConfig::default())
        .await
        .expect("connect");
    assert_eq!(bridge.node_public_key(), Some(NODE_KEY));
    assert!(bridge.error().is_none());
    let mut incoming = bridge.incoming().expect("incoming");

    let conn = timeout(Duration::from_secs(5), incoming.next())
        .await
        .expect("inbound timed out")
        .expect("stream ended")
        .expect("bridge error");
    assert_eq!(conn.info().remote_public_key, REMOTE_KEY);
    assert!(!conn.info().initiator);

    // The connection moves into another task (as it does into a Levin peer task); the handle
    // closes it from outside: the owner reads EOF and the worker gets our Close.
    let handle = conn.close_handle();
    assert_eq!(handle.conn_id(), inbound_id);
    assert!(!handle.is_closed());
    let owner = tokio::spawn(async move {
        let mut conn = conn;
        let mut buf = [0u8; 16];
        conn.read(&mut buf).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.close(CloseReason::Normal);
    let n = timeout(Duration::from_secs(5), owner)
        .await
        .expect("owner did not see EOF")
        .expect("join")
        .expect("read");
    assert_eq!(n, 0);
    assert!(handle.is_closed());
    let close = timeout(Duration::from_secs(5), async {
        loop {
            let f = seen_rx.recv().await.expect("fake worker gone");
            if f.conn_id == inbound_id && matches!(f.body, Body::Close(_)) {
                return f;
            }
        }
    })
    .await
    .expect("no Close frame");
    assert!(matches!(close.body, Body::Close(c) if c.reason == 0));
    handle.close(CloseReason::Normal); // idempotent

    // The worker dies.
    kill_tx.send(()).expect("kill");
    server.await.expect("fake worker panicked");

    let item = timeout(Duration::from_secs(5), incoming.next())
        .await
        .expect("incoming did not end");
    assert!(
        matches!(item, Some(Err(_))),
        "first item after death must be the error, got {item:?}"
    );
    let end = timeout(Duration::from_secs(5), incoming.next())
        .await
        .expect("incoming did not end");
    assert!(end.is_none(), "incoming must end after the error");
    let err = timeout(Duration::from_secs(1), bridge.wait_closed())
        .await
        .expect("wait_closed");
    assert_eq!(bridge.error(), Some(err));

    // Requests fail fast (their timeouts are 40-60 s).
    let t0 = Instant::now();
    assert!(
        bridge
            .announce([9; 32], AnnounceMode::Both, None)
            .await
            .is_err()
    );
    assert!(bridge.lookup([9; 32], 8).await.is_err());
    assert!(
        bridge
            .open(REMOTE_KEY, None, OpenFlags::NONE)
            .await
            .is_err()
    );
    assert!(bridge.unannounce([9; 32]).await.is_err());
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "requests after death took {:?}",
        t0.elapsed()
    );
}

/// A lookup keeps at most `max_peers` peers however many the worker sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_cap() {
    use xkr_p2p_hyper::frame::{LookupPeer, LookupResult, flags};

    let (client, worker_io) = tokio::io::duplex(1 << 20);
    let server = tokio::spawn(async move {
        let mut framed = Framed::new(worker_io, FrameCodec::new(DEFAULT_MAX_FRAME));
        let hello = framed.next().await.expect("client hello").expect("decode");
        assert!(
            matches!(hello.body, Body::Hello(_)),
            "first frame must be Hello"
        );
        framed
            .send(Frame::control(Body::Hello(Hello {
                major: VERSION_MAJOR,
                minor: VERSION_MINOR,
                max_frame: DEFAULT_MAX_FRAME,
                initial_credit: DEFAULT_INITIAL_CREDIT,
                workers: 1,
                worker: 0,
                features: features::HUGIN_MID,
                tlvs: vec![Tlv::new(tlv_tag::PUBLIC_KEY, NODE_KEY.to_vec())],
            })))
            .await
            .expect("send hello");
        loop {
            let Some(Ok(f)) = framed.next().await else {
                return;
            };
            let Body::Lookup(l) = f.body else {
                continue;
            };
            assert_eq!(l.max_peers, 3);
            // A worker that ignores max_peers: ten peers in two frames.
            for (part, end) in [(0u8, false), (1u8, true)] {
                let peers = (0..5u8)
                    .map(|i| LookupPeer {
                        public_key: [part * 5 + i + 10; 32],
                        addr: WireAddr::NONE,
                    })
                    .collect();
                let mut frame = Frame::control(Body::LookupResult(LookupResult {
                    request_id: l.request_id,
                    peers,
                    tlvs: Vec::new(),
                }));
                if end {
                    frame.flags = flags::END;
                }
                framed.send(frame).await.expect("send lookup result");
            }
        }
    });

    let bridge = Bridge::connect_stream(client, BridgeConfig::default())
        .await
        .expect("connect");
    let peers = timeout(Duration::from_secs(5), bridge.lookup([9; 32], 3))
        .await
        .expect("lookup timed out")
        .expect("lookup");
    assert_eq!(peers.len(), 3, "{peers:?}");
    assert_eq!(peers[0].public_key, [10; 32]);
    bridge.close();
    let _ = timeout(Duration::from_secs(5), server).await;
}
