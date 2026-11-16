//! The frame layer over the in-memory channel between a bridge link and a Bare worker, without
//! libbare: fake Bare workers on plain threads speak the frames through `MemPeer`, the way the
//! pump of a real worker moves bytes between bare-kit's IPC and the bridge link.
//!
//! * Hello, requests, an inbound connection, data with credits and the two-way close over a
//!   7-byte channel (every frame split across many reads and writes);
//! * two workers: topics routed by group, `header.shard` and `conn_id` naming the worker, one
//!   merged `incoming()`;
//! * one worker ending fails the whole bridge;
//! * a worker answering with the wrong index is refused;
//! * a worker's `DhtStatus` frames reach [`Bridge::dht_status`].

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::{Buf, BytesMut};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;
use tokio_util::codec::{Decoder, Encoder};
use xkr_p2p_hyper::frame::{
    Ack, Body, Close, ConnId, DEFAULT_INITIAL_CREDIT, DEFAULT_MAX_FRAME, DhtStatus, Frame,
    FrameCodec, Hello, Opened, Tlv, VERSION_MAJOR, VERSION_MINOR, WireAddr, close_reason,
    dht_state, features, flags, nat, tlv_tag,
};
use xkr_p2p_hyper::transport::{MemPeer, mem_channel};
use xkr_p2p_hyper::{
    AnnounceMode, Bridge, BridgeConfig, BridgeError, CloseReason, HuginMid, OpenFlags,
};

const REMOTE: [u8; 32] = [2; 32];
const WAIT: Duration = Duration::from_secs(10);

/// Joins a fake worker thread without blocking the runtime.
async fn join(t: thread::JoinHandle<()>) {
    timeout(WAIT, tokio::task::spawn_blocking(move || t.join()))
        .await
        .expect("fake worker ended in time")
        .expect("spawn_blocking")
        .expect("fake worker panicked");
}

fn worker_key(worker: u16) -> [u8; 32] {
    [0x40 + worker as u8; 32]
}

fn config() -> BridgeConfig {
    BridgeConfig {
        keepalive_idle: Duration::from_secs(60),
        ..BridgeConfig::default()
    }
}

fn hello(worker: u16, workers: u16) -> Frame {
    Frame {
        shard: worker,
        ..Frame::control(Body::Hello(Hello {
            major: VERSION_MAJOR,
            minor: VERSION_MINOR,
            max_frame: DEFAULT_MAX_FRAME,
            initial_credit: DEFAULT_INITIAL_CREDIT,
            workers,
            worker,
            features: features::HUGIN_MID,
            tlvs: vec![Tlv::new(tlv_tag::PUBLIC_KEY, worker_key(worker).to_vec())],
        }))
    }
}

fn opened(request_id: u32, topic: [u8; 32], initiator: bool) -> Body {
    Body::Opened(Box::new(Opened {
        request_id,
        remote_public_key: REMOTE,
        handshake_hash: [3; 64],
        topic,
        initiator,
        addr: WireAddr::NONE,
        tlvs: Vec::new(),
    }))
}

/// The Bare side of one link, driven synchronously like a worker's pump.
struct FakeWorker {
    peer: MemPeer,
    codec: FrameCodec,
    rx: BytesMut,
    worker: u16,
}

impl FakeWorker {
    fn new(peer: MemPeer, worker: u16) -> Self {
        Self {
            peer,
            codec: FrameCodec::new(DEFAULT_MAX_FRAME),
            rx: BytesMut::new(),
            worker,
        }
    }

    fn send(&mut self, frame: Frame) {
        let mut out = BytesMut::new();
        self.codec.encode(frame, &mut out).expect("encode");
        while !out.is_empty() {
            let seen = self.peer.generation();
            match self.peer.write(&out) {
                None => return, // the bridge is gone
                Some(0) => self.peer.wait(seen, Duration::from_millis(20)),
                Some(n) => out.advance(n),
            }
        }
    }

    /// The next frame; `None` when the link closed or nothing came within `limit`.
    fn recv(&mut self, limit: Duration) -> Option<Frame> {
        let deadline = Instant::now() + limit;
        // Small reads on purpose, so frames arrive in pieces.
        let mut buf = [0u8; 5];
        loop {
            if let Some(f) = self.codec.decode(&mut self.rx).expect("decode") {
                return Some(f);
            }
            let seen = self.peer.generation();
            match self.peer.read(&mut buf) {
                None => return None,
                Some(0) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    self.peer.wait(seen, Duration::from_millis(20));
                }
                Some(n) => self.rx.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// The next frame that is not a Ping (answered) or Pong.
    fn expect(&mut self) -> Frame {
        loop {
            let f = self.recv(WAIT).expect("a frame from the bridge");
            match f.body {
                Body::Ping(n) => self.send(Frame {
                    shard: self.worker,
                    ..Frame::control(Body::Pong(n))
                }),
                Body::Pong(_) => {}
                _ => return f,
            }
        }
    }

    /// Reads the bridge's Hello and answers as worker `self.worker` of `workers`.
    fn hello(&mut self, workers: u16) -> Hello {
        let f = self.expect();
        assert_eq!(
            f.shard, self.worker,
            "Hello names the worker in header.shard"
        );
        let Body::Hello(h) = f.body else {
            panic!("the first frame must be Hello");
        };
        self.send(hello(self.worker, workers));
        h
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_credits_and_close_over_a_tiny_channel() {
    const TOPIC: [u8; 32] = [5; 32];
    const PAYLOAD_LEN: usize = 10_000;
    // 7 bytes per direction: every frame is split across many reads and writes.
    let (stream, peer) = mem_channel(7);
    let inbound_id = ConnId::new(true, 0, 1).0;
    let worker = thread::spawn(move || {
        let mut w = FakeWorker::new(peer, 0);
        let h = w.hello(1);
        assert_eq!((h.worker, h.workers), (0, 1));
        let f = w.expect();
        let Body::Announce(a) = f.body else {
            panic!("expected Announce, got {f:?}");
        };
        assert_eq!(a.topic, TOPIC);
        w.send(Frame::control(Body::Ack(Ack {
            request_id: a.request_id,
            tlvs: vec![Tlv::new(tlv_tag::PUBLIC_KEY, worker_key(0).to_vec())],
        })));
        w.send(Frame::conn(inbound_id, opened(0, TOPIC, false)));
        // Echo what Rust writes, crediting it back.
        let mut got = 0;
        while got < PAYLOAD_LEN {
            let f = w.expect();
            assert_eq!(f.conn_id, inbound_id);
            match f.body {
                Body::Data(d) => {
                    got += d.len();
                    w.send(Frame::conn(inbound_id, Body::Credit(d.len() as u32)));
                    w.send(Frame::conn(inbound_id, Body::Data(d)));
                }
                Body::Credit(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        // Rust closes; answer with our own Close (two-way close), then end.
        loop {
            let f = w.expect();
            match f.body {
                Body::Close(c) => {
                    assert_eq!(c.reason, close_reason::NORMAL);
                    w.send(Frame::conn(
                        inbound_id,
                        Body::Close(Close {
                            reason: close_reason::NORMAL,
                            code: 0,
                            message: String::new(),
                        }),
                    ));
                    break;
                }
                Body::Credit(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        // Dropping the peer closes the channel: the worker "ended".
    });

    let bridge = Bridge::connect_stream(stream, config())
        .await
        .expect("connect");
    assert_eq!(bridge.workers(), 1);
    assert_eq!(bridge.node_public_key(), Some(worker_key(0)));
    let ack = timeout(WAIT, bridge.announce(TOPIC, AnnounceMode::Both, None))
        .await
        .expect("Ack in time")
        .expect("Ack");
    assert_eq!(ack.local_public_key(), Some(worker_key(0)));

    let mut incoming = bridge.incoming().expect("incoming");
    let conn = timeout(WAIT, incoming.next())
        .await
        .expect("inbound in time")
        .expect("stream open")
        .expect("connection");
    assert_eq!(conn.info().topic, Some(TOPIC));
    assert_eq!(conn.info().remote_public_key, REMOTE);
    let handle = conn.close_handle();

    let payload: Vec<u8> = (0..PAYLOAD_LEN).map(|i| (i % 251) as u8).collect();
    let (mut rd, mut wr) = tokio::io::split(conn);
    let sent = payload.clone();
    let writer = tokio::spawn(async move {
        wr.write_all(&sent).await.expect("write");
        wr
    });
    let mut echoed = vec![0u8; PAYLOAD_LEN];
    timeout(WAIT, rd.read_exact(&mut echoed))
        .await
        .expect("echo in time")
        .expect("read");
    assert_eq!(echoed, payload);
    let _wr = writer.await.expect("join");

    handle.close(CloseReason::Normal);
    // The worker answers the Close and ends, which fails the bridge.
    let err = timeout(WAIT, bridge.wait_closed())
        .await
        .expect("the bridge noticed the worker end");
    assert!(
        matches!(err, BridgeError::Closed(_) | BridgeError::Io(_)),
        "{err:?}"
    );
    join(worker).await;
    let item = timeout(WAIT, incoming.next()).await.expect("incoming ends");
    assert!(matches!(item, Some(Err(_))), "{item:?}");
    let end = timeout(WAIT, incoming.next()).await.expect("incoming ends");
    assert!(end.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_workers_split_topics_and_connections() {
    const XKR: [u8; 32] = [5; 32];
    const HUGIN: [u8; 32] = [6; 32];
    const GROUP: [u8; 32] = [7; 32];
    let (s0, p0) = mem_channel(64 * 1024);
    let (s1, p1) = mem_channel(64 * 1024);
    let (seen_tx, seen_rx) = mpsc::channel::<(u16, Frame)>();
    let spawn = |peer: MemPeer, worker: u16| {
        let seen = seen_tx.clone();
        thread::spawn(move || {
            let mut w = FakeWorker::new(peer, worker);
            w.hello(2);
            if worker == 1 {
                // An inbound connection on the second worker.
                w.send(Frame::conn(
                    ConnId::new(true, 1, 1).0,
                    opened(0, HUGIN, false),
                ));
            }
            while let Some(f) = w.recv(WAIT) {
                // Recorded before answering, so the test sees it once the answer arrived.
                if seen.send((worker, f.clone())).is_err() {
                    return;
                }
                match f.body {
                    Body::Announce(a) => w.send(Frame {
                        shard: worker,
                        ..Frame::control(Body::Ack(Ack {
                            request_id: a.request_id,
                            tlvs: Vec::new(),
                        }))
                    }),
                    Body::Open(o) => {
                        w.send(Frame::conn(f.conn_id, opened(o.request_id, o.topic, true)));
                    }
                    Body::Ping(n) => w.send(Frame {
                        shard: worker,
                        ..Frame::control(Body::Pong(n))
                    }),
                    _ => {}
                }
            }
        })
    };
    let t0 = spawn(p0, 0);
    let t1 = spawn(p1, 1);
    drop(seen_tx);

    let bridge = Bridge::connect_streams(vec![s0, s1], config())
        .await
        .expect("connect");
    assert_eq!(bridge.workers(), 2);
    assert_eq!(bridge.hugin_worker(), 1);
    assert_eq!(bridge.node_public_key(), Some(worker_key(0)));
    assert_eq!(
        bridge.worker_public_keys(),
        vec![Some(worker_key(0)), Some(worker_key(1))]
    );

    // A plain topic goes to worker 0, a Hugin topic to the last worker (and stays there),
    // an assigned topic where it was assigned.
    timeout(WAIT, bridge.announce(XKR, AnnounceMode::Both, None))
        .await
        .expect("in time")
        .expect("XKR announce");
    timeout(
        WAIT,
        bridge.announce(
            HUGIN,
            AnnounceMode::Server,
            Some(HuginMid::from_topic_key([9; 32])),
        ),
    )
    .await
    .expect("in time")
    .expect("Hugin announce");
    assert_eq!(bridge.worker_for(&XKR), 0);
    assert_eq!(bridge.worker_for(&HUGIN), 1);
    assert!(bridge.assign_topic(GROUP, 1));
    assert!(!bridge.assign_topic(GROUP, 2), "there is no worker 2");
    timeout(WAIT, bridge.announce(GROUP, AnnounceMode::Server, None))
        .await
        .expect("in time")
        .expect("group announce");

    // Connections carry their worker in conn_id.
    let hugin_conn = timeout(WAIT, bridge.open(REMOTE, Some(HUGIN), OpenFlags::NONE))
        .await
        .expect("in time")
        .expect("open on the Hugin topic");
    assert_eq!(ConnId(hugin_conn.conn_id()).shard(), 1);
    let plain_conn = timeout(WAIT, bridge.open(REMOTE, None, OpenFlags::NONE))
        .await
        .expect("in time")
        .expect("open without topic");
    assert_eq!(ConnId(plain_conn.conn_id()).shard(), 0);

    let mut incoming = bridge.incoming().expect("incoming");
    let inbound = timeout(WAIT, incoming.next())
        .await
        .expect("inbound in time")
        .expect("stream open")
        .expect("connection");
    assert_eq!(ConnId(inbound.conn_id()).shard(), 1);

    let mut announced = Vec::new();
    while let Ok((worker, f)) = seen_rx.try_recv() {
        match &f.body {
            Body::Announce(a) => {
                assert_eq!(f.shard, worker, "control frames name their worker");
                announced.push((a.topic, worker));
            }
            Body::Open(_) => {
                assert_eq!(ConnId(f.conn_id).shard(), worker);
                assert_eq!(f.shard, worker);
            }
            _ => {}
        }
    }
    announced.sort_unstable();
    assert_eq!(announced, vec![(XKR, 0), (HUGIN, 1), (GROUP, 1)]);

    drop((hugin_conn, plain_conn, inbound));
    bridge.close();
    // Closing the bridge closes every link: both fake workers see the end of their channel.
    join(t0).await;
    join(t1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_worker_ending_fails_the_whole_bridge() {
    let (s0, p0) = mem_channel(64 * 1024);
    let (s1, p1) = mem_channel(64 * 1024);
    let t0 = thread::spawn(move || {
        let mut w = FakeWorker::new(p0, 0);
        w.hello(2);
        // Runs until the bridge closes this link.
        while w.recv(WAIT).is_some() {}
    });
    let t1 = thread::spawn(move || {
        let mut w = FakeWorker::new(p1, 1);
        w.hello(2);
        // Ends right after the Hello.
    });
    let bridge = Bridge::connect_streams(vec![s0, s1], config())
        .await
        .expect("connect");
    let mut incoming = bridge.incoming().expect("incoming");
    let err = timeout(WAIT, bridge.wait_closed())
        .await
        .expect("worker 0's link fails too");
    assert!(
        matches!(err, BridgeError::Closed(_) | BridgeError::Io(_)),
        "{err:?}"
    );
    assert!(bridge.error().is_some());
    let item = timeout(WAIT, incoming.next()).await.expect("incoming ends");
    assert!(matches!(item, Some(Err(_))), "{item:?}");
    let end = timeout(WAIT, incoming.next()).await.expect("incoming ends");
    assert!(end.is_none(), "one Err, then the end");
    // Requests on the surviving worker fail at once.
    let t = Instant::now();
    assert!(
        bridge
            .announce([5; 32], AnnounceMode::Both, None)
            .await
            .is_err()
    );
    assert!(t.elapsed() < Duration::from_secs(1));
    join(t0).await;
    join(t1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_with_the_wrong_index_is_refused() {
    let (stream, peer) = mem_channel(64 * 1024);
    let t = thread::spawn(move || {
        let mut w = FakeWorker::new(peer, 0);
        let f = w.expect();
        assert!(matches!(f.body, Body::Hello(_)));
        // Answers as worker 1 of 2 on a single-worker link.
        w.send(hello(1, 2));
        while w.recv(Duration::from_secs(2)).is_some() {}
    });
    let err = Bridge::connect_stream(stream, config())
        .await
        .expect_err("a worker index mismatch must fail");
    assert!(matches!(err, BridgeError::Protocol(_)), "{err:?}");
    join(t).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_status_frames_reach_the_bridge() {
    let (stream, peer) = mem_channel(64 * 1024);
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let t = thread::spawn(move || {
        let mut w = FakeWorker::new(peer, 0);
        w.hello(1);
        let status = |state: u8, udp_port: u16| Frame {
            flags: flags::IGNORABLE,
            ..Frame::control(Body::DhtStatus(DhtStatus {
                state,
                firewalled: true,
                nat: nat::RANDOM,
                udp_port,
                tlvs: Vec::new(),
            }))
        };
        w.send(status(dht_state::STARTING, 21_890));
        go_rx.recv_timeout(WAIT).expect("go");
        w.send(status(dht_state::READY, 21_890));
        // Until the bridge closes the link.
        while w.recv(WAIT).is_some() {}
    });
    let bridge = Bridge::connect_stream(stream, config())
        .await
        .expect("connect");
    assert!(bridge.dht_status(1).is_none(), "one worker only");
    let mut rx = bridge.dht_status(0).expect("worker 0");
    let first = timeout(WAIT, rx.wait_for(Option::is_some))
        .await
        .expect("a DhtStatus in time")
        .expect("watch open")
        .clone()
        .expect("some");
    assert_eq!(first.state, dht_state::STARTING);
    assert_eq!(first.udp_port, 21_890);
    go_tx.send(()).expect("worker waits");
    let ready = timeout(
        WAIT,
        rx.wait_for(|s| s.as_ref().is_some_and(|s| s.state == dht_state::READY)),
    )
    .await
    .expect("ready in time")
    .expect("watch open")
    .clone()
    .expect("some");
    assert!(ready.firewalled);
    assert_eq!(ready.nat, nat::RANDOM);
    assert!(bridge.error().is_none(), "DhtStatus is no protocol error");
    bridge.close();
    join(t).await;
}
