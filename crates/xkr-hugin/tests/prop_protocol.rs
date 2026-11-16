//! Property and limit tests for the Hugin relay inputs (`docs/hugin-relay.md`):
//!
//! * bare-rpc messages round-trip; garbage frames and streams never panic; the frame limit is
//!   enforced on the length prefix before anything is buffered;
//! * the PoW / message-shape helpers never panic on arbitrary JSON or blobs;
//! * the message pool keeps its limits whatever is inserted;
//! * JSON nesting is bounded (`serde_json`'s recursion limit of 128 is on);
//! * relay limits: oversized and too-deep packets ban, `some` syncs are rate limited, an
//!   `xkrd_sync_request` is answered once per connection.
//!
//! Cases: `PROPTEST_CASES` (default 128).

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use proptest::collection::vec;
use proptest::prelude::*;
use serde_json::{Map, Value, json};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio_util::codec::{Decoder, FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use xkr_hugin::pool::ManualJobSource;
use xkr_hugin::pow::{self, PowJob, PrevIds};
use xkr_hugin::protocol::{
    BareRpcCodec, RpcCodecError, RpcError, RpcMessage, decode_message, encode_message, stream_flag,
};
use xkr_hugin::relay::{CLIENT_SYNC_REQUESTS_PER_WINDOW, HUGIN_INBOUND_LIMIT};
use xkr_hugin::store::{MessagePool, ONE_DAY_MS, StoreLimits};
use xkr_hugin::{HuginStats, PeerMeta, Relay, RelayConfig, Role};

fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ------------------------------------------------------------------ strategies

fn data() -> impl Strategy<Value = Bytes> {
    vec(any::<u8>(), 0..300).prop_map(Bytes::from)
}

fn rpc_error() -> impl Strategy<Value = RpcError> {
    (any::<String>(), any::<String>(), any::<i64>()).prop_map(|(message, code, errno)| RpcError {
        message,
        code,
        errno,
    })
}

/// Messages in the shape the decoder produces (`data` present iff `stream == 0`, ...).
fn rpc_message() -> impl Strategy<Value = RpcMessage> {
    let stream = prop_oneof![Just(0u64), any::<u64>()];
    prop_oneof![
        (any::<u64>(), any::<u64>(), stream.clone(), data()).prop_map(
            |(id, command, stream, d)| {
                RpcMessage::Request {
                    id,
                    command,
                    stream,
                    data: (stream == 0).then_some(d),
                }
            }
        ),
        (
            any::<u64>(),
            stream.clone(),
            any::<bool>(),
            rpc_error(),
            data()
        )
            .prop_map(|(id, stream, err, e, d)| RpcMessage::Response {
                id,
                stream,
                result: if err {
                    Err(e)
                } else {
                    Ok((stream == 0).then_some(d))
                },
            }),
        (
            any::<u64>(),
            prop_oneof![
                any::<u64>(),
                Just(stream_flag::DATA),
                Just(stream_flag::ERROR)
            ],
            rpc_error(),
            data()
        )
            .prop_map(|(id, stream, e, d)| {
                let error = (stream & stream_flag::ERROR != 0).then_some(e);
                let data = (error.is_none() && stream & stream_flag::DATA != 0).then_some(d);
                RpcMessage::Stream {
                    id,
                    stream,
                    error,
                    data,
                }
            }),
    ]
}

const KEYS: [&str; 24] = [
    "hash",
    "timestamp",
    "cipher",
    "pow",
    "job",
    "job_id",
    "blob",
    "target",
    "shares",
    "nonce",
    "result",
    "auth",
    "pub",
    "sig",
    "version",
    "type",
    "id",
    "message",
    "signal",
    "register",
    "request",
    "kind",
    "xkrd",
    "since",
];

/// Arbitrary JSON biased towards the keys and hex strings the Hugin checks look at.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        "[0-9a-f]{0,130}".prop_map(Value::String),
        any::<String>().prop_map(Value::String),
        Just(json!(2)),
    ];
    leaf.prop_recursive(5, 96, 8, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..6).prop_map(Value::Array),
            vec(
                (
                    prop_oneof![
                        (0..KEYS.len()).prop_map(|i| KEYS[i].to_owned()),
                        any::<String>()
                    ],
                    inner
                ),
                0..8
            )
            .prop_map(|kv| Value::Object(kv.into_iter().collect::<Map<_, _>>())),
        ]
    })
}

/// A well-shaped `pow` payload with random hex: reaches the deepest checks.
fn pow_payload() -> impl Strategy<Value = Value> {
    (
        "[0-9a-f]{0,40}",
        "([0-9a-f]{2}){0,120}",
        "[0-9a-f]{8}",
        vec(("[0-9a-f]{8}", "[0-9a-f]{64}"), 0..7),
        "[0-9a-f]{64}",
        "[0-9a-f]{128}",
    )
        .prop_map(|(job_id, blob, target, shares, public, sig)| {
            let shares: Vec<Value> = shares
                .into_iter()
                .map(
                    |(nonce, result)| json!({ "job_id": job_id, "nonce": nonce, "result": result }),
                )
                .collect();
            json!({
                "version": 2,
                "job": { "job_id": job_id, "blob": blob, "target": target },
                "shares": shares,
                "auth": { "pub": public, "sig": sig },
            })
        })
}

fn exercise_pow_helpers(message: &Value, pow_v: &Value) {
    let hash = message.get("hash");
    let ts = message.get("timestamp");
    let _ = pow::check_message(message, 1.7e12, pow::MAX_MESSAGE_PAST_MS);
    let _ = pow::check_message(message, f64::MAX, f64::MAX);
    let _ = pow::message_kind(message);
    let _ = pow::truthy(Some(message));
    let _ = pow::pow_version(Some(pow_v));
    let _ = pow::pow_version_is_current(Some(pow_v));
    let _ = pow::auth_cache_key(hash, Some(pow_v));
    let _ = pow::auth_context_message(message);
    let _ = pow::auth_context_register(message);
    let _ = pow::js_display(hash);
    let _ = pow::verify_pow_auth(hash, ts, pow_v, "dm:aa");
    let prev = PrevIds::default();
    for fresh in [None, Some(&prev)] {
        if let Ok(v) = pow::validate_pow_payload(hash, pow_v, fresh) {
            for s in v.shares {
                let _ = pow::validate_share_fast(s, &v.job.job_id).map(|f| f.key());
            }
            let _ = pow::nonce_offset(&v.job.blob);
        }
    }
    let _ = PowJob::from_value(
        pow_v.get("job").unwrap_or(pow_v),
        Some(pow::MAX_JOB_BLOB_BYTES),
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases(128)))]

    #[test]
    fn rpc_messages_round_trip(msg in rpc_message()) {
        let mut out = BytesMut::new();
        encode_message(&msg, &mut out);
        let wire = out.to_vec();
        let mut codec = BareRpcCodec::new(1 << 20);
        prop_assert_eq!(codec.decode(&mut out).unwrap(), Some(msg.clone()));
        prop_assert!(out.is_empty());
        prop_assert_eq!(decode_message(&wire[4..]).unwrap(), msg);
        for cut in 0..wire.len() {
            let mut part = BytesMut::from(&wire[..cut]);
            prop_assert!(matches!(BareRpcCodec::new(1 << 20).decode(&mut part), Ok(None)), "cut {}", cut);
        }
    }

    #[test]
    fn rpc_garbage_never_panics(frame in vec(any::<u8>(), 0..512), chunk in 1usize..64) {
        let _ = decode_message(&frame);
        let mut codec = BareRpcCodec::new(HUGIN_INBOUND_LIMIT + 64);
        let mut buf = BytesMut::new();
        for part in frame.chunks(chunk) {
            buf.extend_from_slice(part);
            while let Ok(Some(_)) = codec.decode(&mut buf) {}
        }
    }

    #[test]
    fn pow_helpers_never_panic_on_json(message in json_value(), pow_v in json_value()) {
        exercise_pow_helpers(&message, &pow_v);
        exercise_pow_helpers(&pow_v, &message);
    }

    #[test]
    fn pow_helpers_never_panic_on_shaped_payloads(
        pow_v in pow_payload(),
        hash in "[0-9a-f]{64}",
        cipher in "[0-9a-f]{0,64}",
        ts in any::<f64>(),
    ) {
        let message = json!({ "hash": hash, "cipher": cipher, "timestamp": ts, "pow": pow_v });
        exercise_pow_helpers(&message, &pow_v);
    }

    #[test]
    fn blob_helpers_never_panic(blob in vec(any::<u8>(), 0..200), nonce in any::<[u8; 4]>(), target in "[0-9a-f]{0,18}", h in any::<[u8; 32]>()) {
        let off = pow::nonce_offset(&blob);
        if let Some(b) = pow::insert_nonce(&blob, &nonce) {
            prop_assert_eq!(b.len(), blob.len());
            prop_assert_eq!(&b[off..off + 4], &nonce[..]);
        }
        let _ = pow::extract_prev_id(&blob);
        let _ = pow::parse_target(&target);
        let _ = pow::meets_target(&h, &target);
        let _ = pow::target_difficulty(&target);
        let _ = pow::target_for_difficulty(u64::from_le_bytes(h[..8].try_into().unwrap()));
    }

    #[test]
    fn message_pool_keeps_its_limits(msgs in vec(json_value(), 0..40), since in any::<f64>()) {
        let limits = StoreLimits { max_messages: 16, max_bytes: 4096, ttl_ms: ONE_DAY_MS };
        let mut pool = MessagePool::new(limits);
        let now = 1.7e12;
        for (i, mut m) in msgs.into_iter().enumerate() {
            if i % 2 == 0 && let Some(o) = m.as_object_mut() {
                o.insert("hash".into(), Value::String(format!("{i:064x}")));
            }
            let _ = pool.insert(m, now);
            prop_assert!(pool.len() <= limits.max_messages);
            prop_assert!(pool.bytes() <= limits.max_bytes);
        }
        prop_assert!(pool.since(since, 500).len() <= pool.len());
        prop_assert!(pool.since_full(since, 3).len() <= 3);
        // The pool expires by the message's own timestamp. It does not bound future timestamps:
        // every network path runs `pow::check_message` first (at most MAX_MESSAGE_FUTURE_MS
        // ahead), so a day later only messages dated after `now` may remain (give or take the
        // millisecond bucket the eviction order works in).
        let _ = pool.evict_expired(now + ONE_DAY_MS);
        for (ts, _) in pool.since_full(f64::NEG_INFINITY, usize::MAX) {
            prop_assert!(ts > now - 1.0, "message dated {} survived the day", ts);
        }
        let _ = pool.evict_expired(f64::MAX);
        prop_assert!(pool.is_empty());
        prop_assert_eq!(pool.bytes(), 0);
    }
}

// ------------------------------------------------------------------ codec / JSON limits

#[test]
fn frame_limit_is_checked_on_the_length_prefix() {
    let mut codec = BareRpcCodec::new(HUGIN_INBOUND_LIMIT + 64);
    let mut buf = BytesMut::from(&u32::MAX.to_le_bytes()[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(RpcCodecError::FrameTooLarge(_))
    ));
    // one byte over
    let len = u32::try_from(HUGIN_INBOUND_LIMIT + 64 - 3).unwrap();
    let mut buf = BytesMut::from(&len.to_le_bytes()[..]);
    assert!(matches!(
        codec.decode(&mut buf),
        Err(RpcCodecError::FrameTooLarge(_))
    ));
    // a buffer length beyond the frame is malformed, not a panic
    assert!(
        decode_message(&[
            1, 1, 1, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ])
        .is_err()
    );
}

#[test]
fn json_nesting_is_bounded() {
    for depth in [129usize, 4_500, 100_000] {
        let deep = "[".repeat(depth);
        let t = Instant::now();
        assert!(
            serde_json::from_str::<Value>(&deep).is_err(),
            "depth {depth}"
        );
        assert!(t.elapsed() < Duration::from_secs(1));
        let deep_obj = "{\"a\":".repeat(depth);
        assert!(
            serde_json::from_str::<Value>(&deep_obj).is_err(),
            "depth {depth}"
        );
    }
    // 127 levels are fine (the recursion limit is 128)
    let ok = format!("{}{}", "[".repeat(127), "]".repeat(127));
    assert!(serde_json::from_str::<Value>(&ok).is_ok());
}

// ------------------------------------------------------------------ relay limits

const ADDRESS: &str = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";

struct Peer {
    rd: FramedRead<ReadHalf<DuplexStream>, BareRpcCodec>,
    wr: FramedWrite<WriteHalf<DuplexStream>, BareRpcCodec>,
    next_id: u64,
}

impl Peer {
    async fn send_raw(&mut self, data: Vec<u8>) {
        self.next_id += 1;
        let _ = self.wr.send(RpcMessage::packet(self.next_id, data)).await;
    }

    async fn send(&mut self, v: &Value) {
        self.send_raw(serde_json::to_vec(v).unwrap()).await;
    }

    /// Next JSON packet within `wait` (replies skipped); `None` on close or timeout.
    async fn next_within(&mut self, wait: Duration) -> Option<Value> {
        tokio::time::timeout(wait, async {
            loop {
                match self.rd.next().await {
                    Some(Ok(RpcMessage::Request { data: Some(d), .. })) => {
                        return serde_json::from_slice(&d).ok();
                    }
                    Some(Ok(_)) => {}
                    None | Some(Err(_)) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn until_id(&mut self, id: u64) -> Value {
        loop {
            let v = self
                .next_within(Duration::from_secs(10))
                .await
                .expect("answer");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    async fn until_closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.rd.next().await {
                    None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .is_ok()
    }
}

fn relay(shutdown: &CancellationToken) -> Relay {
    relay_with(shutdown, |_| {})
}

fn relay_with(shutdown: &CancellationToken, f: impl FnOnce(&mut RelayConfig)) -> Relay {
    let mut cfg = RelayConfig::new(ADDRESS.into());
    cfg.flush_interval = Duration::from_millis(100);
    f(&mut cfg);
    Relay::new(
        cfg,
        std::sync::Arc::new(ManualJobSource::default()),
        std::sync::Arc::new(HuginStats::default()),
        shutdown.clone(),
    )
    .unwrap()
}

fn connect(relay: &Relay, role: Role, key: u8) -> Peer {
    let (a, b) = tokio::io::duplex(1 << 20);
    let r = relay.clone();
    tokio::spawn(async move {
        r.serve(
            a,
            PeerMeta {
                role,
                remote_key: [key; 32],
                remote_ip: None,
            },
        )
        .await;
    });
    let (rd, wr) = tokio::io::split(b);
    Peer {
        rd: FramedRead::new(rd, BareRpcCodec::new(16 << 20)),
        wr: FramedWrite::new(wr, BareRpcCodec::new(16 << 20)),
        next_id: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_and_too_deep_packets_ban() {
    let shutdown = CancellationToken::new();
    let a = relay(&shutdown);

    let mut big = connect(&a, Role::Client, 1);
    big.send_raw(vec![b' '; HUGIN_INBOUND_LIMIT + 1]).await;
    assert!(big.until_closed().await);
    assert!(a.is_banned(&[1; 32]));

    let mut deep = connect(&a, Role::Client, 2);
    deep.send_raw("[".repeat(4_000).into_bytes()).await;
    assert!(deep.until_closed().await);
    assert!(a.is_banned(&[2; 32]));
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_sync_requests_are_rate_limited() {
    let shutdown = CancellationToken::new();
    let a = relay(&shutdown);
    let mut c = connect(&a, Role::Client, 3);
    for id in 1..=u64::from(CLIENT_SYNC_REQUESTS_PER_WINDOW) {
        c.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": id }))
            .await;
        let v = c.until_id(id).await;
        assert!(v["response"].is_array(), "{v}");
    }
    let id = u64::from(CLIENT_SYNC_REQUESTS_PER_WINDOW) + 1;
    c.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": id }))
        .await;
    let v = c.until_id(id).await;
    assert_eq!(v["reason"], "rate_limit", "{v}");
    assert!(!a.is_banned(&[3; 32]));
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xkrd_sync_is_served_once_per_connection() {
    let shutdown = CancellationToken::new();
    let a = relay(&shutdown);
    let mut n = connect(&a, Role::Node, 4);
    let hello = n.next_within(Duration::from_secs(10)).await.expect("hello");
    assert_eq!(hello["xkrd"]["sync"], 1);
    n.send(&json!({ "version": "2.0.0", "xkrd": { "version": "test", "sync": 1 } }))
        .await;
    for id in [77u64, 78] {
        n.send(&json!({ "type": "xkrd_sync_request", "id": id, "since": 0 }))
            .await;
    }
    let mut answered = Vec::new();
    while let Some(v) = n.next_within(Duration::from_millis(1500)).await {
        if v["type"] == "xkrd_sync" && v["done"] == true {
            answered.push(v["id"].as_u64().unwrap_or(0));
        }
    }
    assert_eq!(answered, vec![77]);
    assert!(!a.is_banned(&[4; 32]));
    shutdown.cancel();
}

/// A connection that sends nothing within the hello timeout is closed (no ban); one
/// that sent a packet stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_connections_hit_the_hello_timeout() {
    let shutdown = CancellationToken::new();
    let a = relay_with(&shutdown, |c| c.hello_timeout = Duration::from_millis(300));
    let mut silent = connect(&a, Role::Client, 20);
    assert!(silent.until_closed().await);
    assert!(!a.is_banned(&[20; 32]));

    let mut talker = connect(&a, Role::Client, 21);
    talker
        .send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 1 }))
        .await;
    assert!(talker.until_id(1).await["response"].is_array());
    tokio::time::sleep(Duration::from_millis(600)).await;
    talker
        .send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 2 }))
        .await;
    assert!(talker.until_id(2).await["response"].is_array());
    shutdown.cancel();
}

/// When the client slots are full, the longest-silent client (silent for at least
/// `idle_evict_after`) makes room; without such a client the newcomer is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_client_is_evicted_when_full() {
    let shutdown = CancellationToken::new();
    let a = relay_with(&shutdown, |c| {
        c.max_clients = 1;
        c.idle_evict_after = Duration::from_millis(300);
    });
    let mut old = connect(&a, Role::Client, 22);
    old.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 1 }))
        .await;
    assert!(old.until_id(1).await["response"].is_array());
    // A newcomer right away: the old client is not idle long enough, so it is refused.
    let mut early = connect(&a, Role::Client, 23);
    assert!(early.until_closed().await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut late = connect(&a, Role::Client, 24);
    let hello = late
        .next_within(Duration::from_secs(10))
        .await
        .expect("hello");
    assert_eq!(hello["address"], ADDRESS);
    assert!(old.until_closed().await);
    shutdown.cancel();
}

/// With the client slots full, a client that sent only `pow_tag` packets since its sync is
/// evicted for a newcomer once its sync is `idle_evict_after` old.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pow_tag_packets_do_not_keep_a_slot() {
    let shutdown = CancellationToken::new();
    let a = relay_with(&shutdown, |c| {
        c.max_clients = 1;
        c.idle_evict_after = Duration::from_millis(300);
    });
    let mut old = connect(&a, Role::Client, 25);
    old.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 1 }))
        .await;
    assert!(old.until_id(1).await["response"].is_array());
    for id in 10..16u64 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        old.send(&json!({ "type": "pow_tag", "id": id })).await;
    }
    let mut late = connect(&a, Role::Client, 26);
    let hello = late
        .next_within(Duration::from_secs(10))
        .await
        .expect("hello");
    assert_eq!(hello["address"], ADDRESS);
    assert!(old.until_closed().await);
    assert!(!a.is_banned(&[25; 32]));
    shutdown.cancel();
}
