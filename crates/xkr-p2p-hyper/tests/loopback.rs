//! Loopback test of two in-process bridges (Bare workers) over a local HyperDHT testnet. Needs
//! `node` (for the testnet's bootstrap script), the installed `bridge/` dependencies and
//! libbare-kit (`scripts/build-bare-kit.sh`), so it is ignored by default:
//!
//! ```text
//! cargo test -p xkr-p2p-hyper --test loopback -- --ignored --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use xkr_p2p_hyper::{
    AnnounceMode, BareConfig, BareWorkers, Bridge, BridgeConfig, BridgeError, CloseReason,
    HuginTopic, OpenFlags, PrivateSecret, XkrNetwork, topic,
};

fn bridge_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bridge")
}

/// Starts `bridge/test/testnet-bootstrap.js` and returns it with its `host:port`.
async fn start_testnet() -> (Child, String) {
    let mut child = Command::new("node")
        .arg(bridge_dir().join("test/testnet-bootstrap.js"))
        .current_dir(bridge_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn node testnet-bootstrap.js");
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .expect("testnet start timed out")
        .unwrap()
        .expect("testnet printed nothing");
    let addr = line
        .strip_prefix("BOOTSTRAP ")
        .expect("BOOTSTRAP line")
        .trim()
        .to_owned();
    (child, addr)
}

fn config(bootstrap: &str, initial_credit: u32) -> BridgeConfig {
    BridgeConfig {
        bridge_dir: Some(bridge_dir()),
        bootstrap: vec![bootstrap.to_owned()],
        initial_credit,
        ..BridgeConfig::default()
    }
}

/// One in-process bridge with a single Bare worker.
async fn start(bootstrap: &str, initial_credit: u32) -> (Bridge, BareWorkers) {
    Bridge::start_in_process(config(bootstrap, initial_credit), &BareConfig::default())
        .await
        .expect("start Bare worker")
}

/// Retries a lookup until `want` shows up.
async fn find(bridge: &Bridge, topic: [u8; 32], want: [u8; 32]) {
    for _ in 0..40 {
        let peers = bridge.lookup(topic, 16).await.expect("lookup");
        if peers.iter().any(|p| p.public_key == want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("peer never showed up in lookup");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs node, bridge/node_modules and libbare-kit"]
async fn announce_lookup_open_and_stream_both_ways() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let (_testnet, bootstrap) = start_testnet().await;

    // A small window on A forces many credit round trips for the 1 MiB transfer below.
    let (a, mut workers_a) = start(&bootstrap, 64 * 1024).await;
    let (b, mut workers_b) = start(&bootstrap, 1024 * 1024).await;
    assert_eq!(a.params().initial_credit, 64 * 1024);
    let a_key = a.node_public_key().expect("node key in Hello");

    let t = topic::xkr_topic("xkr/test/loopback/v1");
    let ack = a
        .announce(t, AnnounceMode::Server, None)
        .await
        .expect("announce");
    assert_eq!(ack.local_public_key(), Some(a_key));
    find(&b, t, a_key).await;

    let mut a_incoming = a.incoming().expect("incoming");
    assert!(a.incoming().is_none(), "incoming() is take-once");

    let mut conn_b = b.open(a_key, Some(t), OpenFlags::NONE).await.expect("open");
    assert!(conn_b.info().initiator);
    assert_eq!(conn_b.info().remote_public_key, a_key);
    assert_eq!(conn_b.info().topic, Some(t));

    let mut conn_a = tokio::time::timeout(Duration::from_secs(20), a_incoming.next())
        .await
        .expect("inbound timed out")
        .expect("stream ended")
        .expect("bridge error");
    assert!(!conn_a.info().initiator);
    assert_eq!(conn_a.info().handshake_hash, conn_b.info().handshake_hash);

    // small message both ways
    conn_b.write_all(b"hello a\n").await.unwrap();
    let mut buf = [0u8; 8];
    conn_a.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello a\n");
    conn_a.write_all(b"hello b\n").await.unwrap();
    conn_b.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello b\n");

    // 1 MiB from B to A through A's 64 KiB window: only completes if A returns credits
    let payload: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let writer = tokio::spawn(async move {
        conn_b.write_all(&payload).await.unwrap();
        conn_b.flush().await.unwrap();
        conn_b
    });
    let mut received = vec![0u8; expected.len()];
    tokio::time::timeout(Duration::from_secs(60), conn_a.read_exact(&mut received))
        .await
        .expect("transfer timed out")
        .unwrap();
    assert!(received == expected, "payload mismatch");
    let conn_b = writer.await.unwrap();

    // Credits flow back to both Rust writers: B's window towards its worker refills once the
    // worker has written everything to the secret stream, A's after its 8-byte write.
    for (conn, full) in [
        (&conn_b, i64::from(b.params().initial_credit)),
        (&conn_a, i64::from(a.params().initial_credit)),
    ] {
        let mut ok = false;
        for _ in 0..100 {
            if conn.send_window() == full {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            ok,
            "send window did not return to {full}: {}",
            conn.send_window()
        );
    }

    // peer info
    let pi = conn_a.peer_info().await.expect("peer info");
    assert!(pi.bytes_in > 1024 * 1024);

    // close from B: A reads EOF
    conn_b.close(CloseReason::Normal);
    let mut rest = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(20), conn_a.read_to_end(&mut rest))
        .await
        .expect("close not seen")
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert!(conn_a.is_closed());

    // a dial to nobody fails with a Close
    match b.open([7u8; 32], None, OpenFlags::NONE).await {
        Err(BridgeError::ConnClosed(info)) => assert_ne!(info.reason, CloseReason::Normal),
        Err(BridgeError::Timeout) => {}
        other => panic!("expected failure, got {other:?}"),
    }

    // closing the bridge ends A's incoming stream with an error
    a.close();
    let end = tokio::time::timeout(Duration::from_secs(5), a_incoming.next())
        .await
        .expect("incoming end");
    assert!(matches!(end, Some(Err(_)) | None));
    b.close();
    workers_a.stop(Duration::from_secs(5)).await;
    workers_b.stop(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs node, bridge/node_modules and libbare-kit"]
async fn hugin_identity_connects_with_mid() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let (_testnet, bootstrap) = start_testnet().await;
    let (a, mut workers_a) = start(&bootstrap, 1024 * 1024).await;
    let (b, mut workers_b) = start(&bootstrap, 1024 * 1024).await;

    let h = HuginTopic::public_nodes();
    let ack = a
        .announce(h.topic, AnnounceMode::Server, Some(h.mid()))
        .await
        .expect("announce");
    let a_identity = ack.local_public_key().expect("identity key");
    assert_ne!(
        Some(a_identity),
        a.node_public_key(),
        "hugin topics get their own identity"
    );
    find(&b, h.topic, a_identity).await;

    let mut incoming = a.incoming().unwrap();
    // with the mid
    let conn = b
        .open_hugin(a_identity, h.topic, OpenFlags::NONE, &h.mid())
        .await
        .expect("open with mid");
    assert_eq!(conn.info().topic, Some(h.topic));
    let inbound = tokio::time::timeout(Duration::from_secs(20), incoming.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(inbound.info().topic, Some(h.topic));

    // without the mid the hugin identity refuses us
    match b.open(a_identity, None, OpenFlags::NONE).await {
        Ok(_) => panic!("mid-less connection must be refused"),
        Err(BridgeError::ConnClosed(_) | BridgeError::Timeout) => {}
        Err(e) => panic!("unexpected error {e}"),
    }
    a.close();
    b.close();
    workers_a.stop(Duration::from_secs(5)).await;
    workers_b.stop(Duration::from_secs(5)).await;
}

/// A refused dial: the handshake fails, so the dial ends with a Close (or times out).
fn refused(result: &Result<xkr_p2p_hyper::HyperConn, BridgeError>, what: &str) {
    match result {
        Ok(_) => panic!("{what}: the connection must be refused"),
        Err(BridgeError::ConnClosed(_) | BridgeError::Timeout) => {}
        Err(e) => panic!("{what}: unexpected error {e}"),
    }
}

/// A private node (`xkrd --private`): its sync and Hugin identities, announced in server mode
/// with the topic keys and identity seeds of its secret, take a peer that dials them with the
/// invite's `mid`; a peer without a `mid`, or with the `mid` of another secret, is refused, and
/// the node key, which only joins topics as a client, takes nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs node, bridge/node_modules and libbare-kit"]
async fn private_identities_take_only_invite_holders() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let (_testnet, bootstrap) = start_testnet().await;
    let (a, mut workers_a) = start(&bootstrap, 1024 * 1024).await;
    let (b, mut workers_b) = start(&bootstrap, 1024 * 1024).await;
    let a_key = a.node_public_key().expect("node key");

    let secret = PrivateSecret::generate();
    let topics = secret.topics(XkrNetwork::Testnet);
    let invite = secret.invite();
    let sync = a
        .announce(
            topics.sync.topic,
            AnnounceMode::Server,
            Some(secret.sync_mid(&topics)),
        )
        .await
        .expect("announce sync");
    assert_eq!(sync.local_public_key(), Some(invite.sync_key));
    let hugin = a
        .announce(
            topics.hugin.topic,
            AnnounceMode::Server,
            Some(secret.hugin_mid(&topics)),
        )
        .await
        .expect("announce hugin");
    assert_eq!(hugin.local_public_key(), Some(invite.hugin_key));
    let public = topic::xkr_topic("xkr/test/private/v1");
    a.announce(public, AnnounceMode::Client, None)
        .await
        .expect("announce public client-only");
    let mut incoming = a.incoming().unwrap();

    // the invite as a peer gets it: both topics derive from its secret
    let parsed: xkr_p2p_hyper::Invite = invite.to_string().parse().expect("invite");
    let theirs = parsed.topics(XkrNetwork::Testnet);
    for (key, t) in [
        (parsed.sync_key, &theirs.sync),
        (parsed.hugin_key, &theirs.hugin),
    ] {
        let conn = b
            .open_hugin(key, t.topic, OpenFlags::NONE, &t.mid())
            .await
            .expect("open with the invite's mid");
        assert_eq!(conn.info().remote_public_key, key);
        let inbound = tokio::time::timeout(Duration::from_secs(20), incoming.next())
            .await
            .expect("inbound timed out")
            .unwrap()
            .unwrap();
        assert_eq!(inbound.info().topic, Some(t.topic));
        assert!(!inbound.info().initiator);
    }

    // no mid
    refused(
        &b.open(invite.sync_key, None, OpenFlags::NONE).await,
        "no mid",
    );
    // the mid of another secret
    let other = PrivateSecret::generate().topics(XkrNetwork::Testnet);
    refused(
        &b.open_hugin(
            invite.sync_key,
            other.sync.topic,
            OpenFlags::NONE,
            &other.sync.mid(),
        )
        .await,
        "another secret",
    );
    refused(
        &b.open_hugin(
            invite.hugin_key,
            other.hugin.topic,
            OpenFlags::NONE,
            &other.hugin.mid(),
        )
        .await,
        "another secret (hugin)",
    );
    // the node key joined its topic client-only: nobody can dial it
    refused(
        &b.open(a_key, Some(public), OpenFlags::NONE).await,
        "node key",
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), incoming.next())
            .await
            .is_err(),
        "no refused peer reached the node"
    );

    a.close();
    b.close();
    workers_a.stop(Duration::from_secs(5)).await;
    workers_b.stop(Duration::from_secs(5)).await;
}
