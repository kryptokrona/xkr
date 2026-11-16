//! In-process relays over `tokio::io::duplex`: a client posts a paid message to relay A, A
//! gossips it to relay B, a client of B finds it, a third relay C gets it through the xkrd sync,
//! a reused share and protocol violations are refused; gossip is checked like hugin-node does;
//! an app that is silent while it searches a share keeps its connection and then posts; gossip
//! on XKR jobs (zero `prev_id`) needs no job of our own; a post on a job the source did not hand
//! out is refused without a strike, and without a fee address nothing is submitted.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;
use xkr_hugin::pool::ManualJobSource;
use xkr_hugin::pool::{NoJobSource, ShareSubmit, SubmitOutcome};
use xkr_hugin::pow;
use xkr_hugin::protocol::{BareRpcCodec, RpcMessage};
use xkr_hugin::relay::{
    DEFAULT_APP_DIFFICULTY, GOSSIP_MIN_DIFFICULTY, HELLO_TIMEOUT, MIN_SHARE_DIFFICULTY, now_ms,
};
use xkr_hugin::{HuginStats, JobSource, PeerMeta, PoolJob, Relay, RelayConfig, Role};

const VECTORS: &str = include_str!("../../../docs/test-vectors/hugin-relay.json");
const ADDRESS: &str = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";
const WAIT: Duration = Duration::from_secs(10);

fn vectors() -> Value {
    serde_json::from_str(VECTORS).unwrap()
}

/// The job, nonce and result of the CN-Turtle vector (target ffffffff: every hash is a share).
fn share_vector() -> (Value, Value) {
    let v = vectors();
    let p = &v["pow"];
    let share = json!({ "job_id": p["job"]["job_id"], "nonce": p["nonce"], "result": p["result"] });
    (p["job"].clone(), share)
}

/// A post packet as hugin-desktop/hugin-native build it, signed with a one-time key.
fn post(hash: &str, cipher: &str, ts: f64) -> Value {
    let (job, share) = share_vector();
    let context = format!("dm:{cipher}");
    let payload = pow::pow_sig_payload(hash, &format!("{ts:.0}"), &share, &context);
    let (public, sig) = pow::ed25519_sign(&[9u8; 32], payload.as_bytes());
    json!({
        "type": "post",
        "message": {
            "cipher": cipher,
            "timestamp": ts as u64,
            "hash": hash,
            "id": ts as u64,
            "push": true,
            "viewtag": "abc",
            "kind": "dm",
            "pow": {
                "version": 2,
                "job": job,
                "shares": [share],
                "auth": { "pub": hex::encode(public), "sig": hex::encode(sig), "nonce": share["nonce"] },
            },
        },
    })
}

/// A relay whose app difficulty and gossip difficulty floor are 1 (the vector job's target is
/// ffffffff).
fn relay(jobs: Arc<ManualJobSource>, shutdown: &CancellationToken) -> Relay {
    relay_with(jobs, shutdown, 1)
}

fn relay_with(
    jobs: Arc<dyn JobSource>,
    shutdown: &CancellationToken,
    min_difficulty: u64,
) -> Relay {
    let mut cfg = RelayConfig::new(ADDRESS.into());
    cfg.flush_interval = Duration::from_millis(100);
    cfg.min_share_difficulty = min_difficulty;
    cfg.app_difficulty = min_difficulty;
    Relay::new(cfg, jobs, Arc::new(HuginStats::default()), shutdown.clone()).unwrap()
}

/// A share of `job` for `nonce` (every hash meets target ffffffff).
fn mined_share(job: &Value, nonce: u32) -> Value {
    let blob = hex::decode(job["blob"].as_str().unwrap()).unwrap();
    let nonce_hex = format!("{nonce:08x}");
    let bytes: [u8; 4] = hex::decode(&nonce_hex).unwrap().try_into().unwrap();
    let hashing = pow::insert_nonce(&blob, &bytes).unwrap();
    let result = hex::encode(xkr_crypto::cn_turtle_lite_slow_hash_v2(&hashing).0);
    json!({ "job_id": job["job_id"], "nonce": nonce_hex, "result": result })
}

/// The first share of `job` from nonce 0 up that meets the job target.
fn first_share(job: &Value) -> Value {
    let target = job["target"].as_str().unwrap();
    (0..=u32::MAX)
        .map(|nonce| mined_share(job, nonce))
        .find(|share| {
            let hash: [u8; 32] = hex::decode(share["result"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            pow::meets_target(&hash, target)
        })
        .unwrap()
}

/// A job as `bridge/test/fake-stratum-pool.js` issues it: difficulty 100 (target `285c8f02`,
/// hugin-node's least gossip difficulty), with `height`; the vector job's blob.
fn sim_pool_job() -> Value {
    let (mut job, _) = share_vector();
    job["job_id"] = json!("sim-1001");
    job["target"] = json!("285c8f02");
    job["height"] = json!(1001);
    job
}

/// A share of `job` whose result is wrong.
fn bogus_share(job: &Value, nonce: u32) -> Value {
    json!({ "job_id": job["job_id"], "nonce": format!("{nonce:08x}"), "result": "00".repeat(32) })
}

/// A DM carrying `job` and `shares`, signed for `shares[auth]` with a one-time key from `seed`.
fn message_with(
    hash: &str,
    cipher: &str,
    job: &Value,
    shares: &[Value],
    auth: usize,
    seed: u8,
) -> Value {
    let ts = now_ms() as u64;
    let share = &shares[auth];
    let payload = pow::pow_sig_payload(hash, &ts.to_string(), share, &format!("dm:{cipher}"));
    let (public, sig) = pow::ed25519_sign(&[seed; 32], payload.as_bytes());
    json!({
        "cipher": cipher,
        "timestamp": ts,
        "hash": hash,
        "id": ts,
        "push": true,
        "viewtag": "abc",
        "kind": "dm",
        "pow": {
            "version": 2,
            "job": job,
            "shares": shares,
            "auth": { "pub": hex::encode(public), "sig": hex::encode(sig), "nonce": share["nonce"] },
        },
    })
}

/// The vector job with another `prev_id` (bytes 0x5a after the three header varints).
fn job_with_other_prev_id() -> Value {
    let (mut job, _) = share_vector();
    let mut blob = hex::decode(job["blob"].as_str().unwrap()).unwrap();
    let off = pow::nonce_offset(&blob) - 32;
    blob[off..off + 32].fill(0x5a);
    job["blob"] = json!(hex::encode(blob));
    job["job_id"] = json!("job-2");
    job
}

fn jobs() -> Arc<ManualJobSource> {
    let (job, _) = share_vector();
    let src = Arc::new(ManualJobSource::default());
    src.set_job(PoolJob::from_value(job).unwrap());
    src
}

/// A bare-rpc peer driven by the test.
struct Peer {
    rd: FramedRead<ReadHalf<DuplexStream>, BareRpcCodec>,
    wr: FramedWrite<WriteHalf<DuplexStream>, BareRpcCodec>,
    next_id: u64,
}

impl Peer {
    fn new(stream: DuplexStream) -> Self {
        let (r, w) = tokio::io::split(stream);
        Self {
            rd: FramedRead::new(r, BareRpcCodec::new(16 << 20)),
            wr: FramedWrite::new(w, BareRpcCodec::new(16 << 20)),
            next_id: 0,
        }
    }

    async fn send(&mut self, v: &Value) {
        self.send_raw(serde_json::to_vec(v).unwrap()).await;
    }

    async fn send_raw(&mut self, data: Vec<u8>) {
        self.next_id += 1;
        self.wr
            .send(RpcMessage::packet(self.next_id, data))
            .await
            .unwrap();
    }

    /// Next JSON packet (replies skipped); `None` when the relay closed the connection.
    async fn next(&mut self) -> Option<Value> {
        loop {
            match tokio::time::timeout(WAIT, self.rd.next())
                .await
                .expect("timeout")?
            {
                Ok(RpcMessage::Request { data: Some(d), .. }) => {
                    return Some(serde_json::from_slice(&d).unwrap());
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    async fn until(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if pred(&v) {
                return v;
            }
        }
    }
}

fn client(relay: &Relay, key: u8) -> Peer {
    let (a, b) = tokio::io::duplex(1 << 20);
    let r = relay.clone();
    tokio::spawn(async move {
        r.serve(
            a,
            PeerMeta {
                role: Role::Client,
                remote_key: [key; 32],
                remote_ip: None,
            },
        )
        .await;
    });
    Peer::new(b)
}

/// A test-driven node peer of `relay` (a hugin-node: no xkrd sync).
async fn node_peer(relay: &Relay, key: u8) -> Peer {
    let (a, b) = tokio::io::duplex(1 << 20);
    let r = relay.clone();
    tokio::spawn(async move {
        r.serve(
            a,
            PeerMeta {
                role: Role::Node,
                remote_key: [key; 32],
                remote_ip: None,
            },
        )
        .await;
    });
    let mut p = Peer::new(b);
    p.next().await.expect("hello");
    p.send(&json!({ "version": "2.0.0" })).await;
    p
}

/// Waits until `relay` received `n` gossip messages, then a little for their checks.
async fn gossip_checked(relay: &Relay, n: u64) {
    eventually("gossip received", || {
        relay.stats().snapshot().gossip_received >= n
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
}

fn link(a: &Relay, a_key: u8, b: &Relay, b_key: u8) {
    let (x, y) = tokio::io::duplex(1 << 20);
    let (ra, rb) = (a.clone(), b.clone());
    tokio::spawn(async move {
        ra.serve(
            x,
            PeerMeta {
                role: Role::Node,
                remote_key: [b_key; 32],
                remote_ip: None,
            },
        )
        .await;
    });
    tokio::spawn(async move {
        rb.serve(
            y,
            PeerMeta {
                role: Role::Node,
                remote_key: [a_key; 32],
                remote_ip: None,
            },
        )
        .await;
    });
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A client's first steps: hello, then a job.
async fn hello_and_job(c: &mut Peer) {
    let hello = c.next().await.unwrap();
    assert_eq!(hello["address"], ADDRESS);
    assert_eq!(hello["version"], "2.0.0");
    c.send(&json!({ "type": "job_request", "id": "j1" })).await;
    // `{ type: 'job', id }`, or `job_pending` and then the pushed job once the relay has it
    let job = c.until(|v| v["type"] == "job").await;
    assert_eq!(job["job"]["job_id"], "job-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_gossip_sync_and_reused_share() {
    let shutdown = CancellationToken::new();
    let (jobs_a, jobs_b, jobs_c) = (jobs(), jobs(), jobs());
    let a = relay(jobs_a.clone(), &shutdown);
    let b = relay(jobs_b, &shutdown);
    link(&a, 0xa0, &b, 0xb0);

    let mut alice = client(&a, 1);
    hello_and_job(&mut alice).await;
    let mut bob = client(&b, 2);
    hello_and_job(&mut bob).await;

    let hash = "ab".repeat(32);
    let ts = now_ms();
    alice.send(&post(&hash, "c0ffee", ts)).await;
    let answer = alice.until(|v| v.get("success").is_some()).await;
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["id"], ts as u64);
    assert_eq!(jobs_a.submitted().len(), 1, "the share went to A's pool");

    // A pushes it to its clients (client view: no pow.job)
    let pushed = alice.until(|v| v["type"] == "new-message").await;
    assert_eq!(pushed["messages"][0]["hash"], hash.as_str());
    assert!(pushed["messages"][0]["pow"].get("job").is_none());

    // gossip reached B
    eventually("gossip to B", || b.has_message(&hash)).await;
    bob.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 7 }))
        .await;
    let list = bob.until(|v| v.get("response").is_some()).await;
    assert_eq!(list["id"], 7);
    assert_eq!(list["response"][0]["hash"], hash.as_str());
    assert_eq!(list["response"][0]["cipher"], "c0ffee");
    assert_eq!(list["response"][0]["viewtag"], "abc");

    // a resend of the same message is a duplicate, not a second payment
    let mut alice2 = client(&a, 3);
    hello_and_job(&mut alice2).await;
    alice2.send(&post(&hash, "c0ffee", ts)).await;
    let dup = alice2.until(|v| v.get("success").is_some()).await;
    assert_eq!(dup["duplicate"], true, "{dup}");

    // the same share for another message is refused
    let mut charlie = client(&a, 4);
    hello_and_job(&mut charlie).await;
    charlie
        .send(&post(&"cd".repeat(32), "beef", now_ms()))
        .await;
    let reused = charlie.until(|v| v.get("success").is_some()).await;
    assert_eq!(reused["success"], false);
    assert_eq!(reused["reason"], "share_reused");
    assert_eq!(jobs_a.submitted().len(), 1);

    // a fresh relay C joins the mesh and gets the pool through the xkrd sync
    let c = relay(jobs_c, &shutdown);
    link(&a, 0xa0, &c, 0xc0);
    eventually("sync to C", || c.stats().snapshot().synced_in == 1).await;
    assert!(c.has_message(&hash));
    assert_eq!(a.stats().snapshot().xkrd_nodes, 2);
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn violations_are_banned() {
    let shutdown = CancellationToken::new();
    let a = relay(jobs(), &shutdown);

    // not JSON
    let mut c = client(&a, 1);
    c.next().await.unwrap();
    c.send_raw(b"{not json".to_vec()).await;
    assert!(c.until_closed().await);
    assert!(a.is_banned(&[1; 32]));
    // a banned key cannot come back
    let mut again = client(&a, 1);
    assert!(again.next().await.is_none());

    // more than 9000 bytes of PACKET data
    let mut big = client(&a, 2);
    big.next().await.unwrap();
    big.send(&json!({ "x": "y".repeat(9100) })).await;
    assert!(big.until_closed().await);
    assert!(a.is_banned(&[2; 32]));

    // an unknown request type
    let mut odd = client(&a, 3);
    odd.next().await.unwrap();
    odd.send(&json!({ "request": true, "type": "all", "timestamp": 0 }))
        .await;
    assert!(odd.until_closed().await);

    // unknown packet type: answered, not banned
    let mut fine = client(&a, 4);
    fine.next().await.unwrap();
    fine.send(&json!({ "type": "nope", "id": 1 })).await;
    let r = fine.until(|v| v.get("reason").is_some()).await;
    assert_eq!(r["reason"], "unknown_type");
    assert!(!a.is_banned(&[4; 32]));

    // a post without shares: 60 s timeout
    fine.send(&json!({ "type": "post", "message": { "id": 5 } }))
        .await;
    let r = fine.until(|v| v.get("reason").is_some()).await;
    assert_eq!(r["reason"], "no_share");
    assert!(a.is_banned(&[4; 32]));

    // a bad signature twice from one client is a ban
    let mut bad = client(&a, 5);
    hello_and_job(&mut bad).await;
    for hash in ["11".repeat(32), "22".repeat(32)] {
        let mut p = post(&hash, "aa", now_ms());
        p["message"]["pow"]["auth"]["sig"] = json!("00".repeat(64));
        bad.send(&p).await;
        let r = bad.until(|v| v.get("success").is_some()).await;
        assert_eq!(r["reason"], "invalid_pow_signature");
    }
    assert!(bad.until_closed().await);
    assert!(a.is_banned(&[5; 32]));
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_gossip_bans_the_node() {
    let shutdown = CancellationToken::new();
    let a = relay(jobs(), &shutdown);
    let (x, y) = tokio::io::duplex(1 << 20);
    let ra = a.clone();
    tokio::spawn(async move {
        ra.serve(
            x,
            PeerMeta {
                role: Role::Node,
                remote_key: [0xee; 32],
                remote_ip: None,
            },
        )
        .await;
    });
    let mut node = Peer::new(y);
    let hello = node.next().await.unwrap();
    assert_eq!(hello["version"], "2.0.0");
    assert_eq!(hello["xkrd"]["sync"], 1);
    // hugin-node's own hello, then a signal whose share does not hash to its result
    node.send(&json!({ "version": "2.0.0" })).await;
    let mut m = post(&"33".repeat(32), "aa", now_ms())["message"].clone();
    m["pow"]["shares"][0]["result"] = json!("00".repeat(32));
    // re-sign for the changed result so only the PoW is wrong
    let share = m["pow"]["shares"][0].clone();
    let payload = pow::pow_sig_payload(
        m["hash"].as_str().unwrap(),
        &m["timestamp"].to_string(),
        &share,
        "dm:aa",
    );
    let (public, sig) = pow::ed25519_sign(&[3u8; 32], payload.as_bytes());
    m["pow"]["auth"] =
        json!({ "pub": hex::encode(public), "sig": hex::encode(sig), "nonce": share["nonce"] });
    node.send(&json!({ "signal": true, "message": m })).await;
    assert!(node.until_closed().await);
    assert!(a.is_banned(&[0xee; 32]));
    assert_eq!(a.pool_len(), 0);
    shutdown.cancel();
}

impl Peer {
    /// Reads until the relay closes the connection (true) or the wait times out (false).
    async fn until_closed(&mut self) -> bool {
        tokio::time::timeout(WAIT, async {
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

/// A post whose signed share is valid is stored and gossiped with that share only; peers keep
/// it and ban nobody, also when the post carried a second, wrong share.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_forward_only_the_signed_share() {
    let shutdown = CancellationToken::new();
    let a = relay(jobs(), &shutdown);
    let b = relay(jobs(), &shutdown);
    link(&a, 0xa0, &b, 0xb0);
    let (job, _) = share_vector();
    for i in 0..3u8 {
        let hash = format!("{:064x}", 0x700 + u32::from(i));
        let shares = [
            mined_share(&job, 100 + u32::from(i)),
            bogus_share(&job, 200 + u32::from(i)),
        ];
        let mut c = client(&a, 10 + i);
        hello_and_job(&mut c).await;
        c.send(&json!({ "type": "post", "message": message_with(&hash, "c0ffee", &job, &shares, 0, 30 + i) }))
            .await;
        let answer = c.until(|v| v.get("success").is_some()).await;
        assert_eq!(answer["success"], true, "{answer}");
        eventually("gossip to B", || b.has_message(&hash)).await;
    }
    let mut bob = client(&b, 20);
    hello_and_job(&mut bob).await;
    bob.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 1 }))
        .await;
    let list = bob.until(|v| v.get("response").is_some()).await;
    let messages = list["response"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    for m in messages {
        assert_eq!(m["pow"]["shares"].as_array().unwrap().len(), 1, "{m}");
    }
    assert!(!a.is_banned(&[0xb0; 32]));
    assert!(!b.is_banned(&[0xa0; 32]));
    shutdown.cancel();
}

/// Gossip from a hugin-node peer: only the share `pow.auth` signs is hashed (valid: stored with
/// that share alone, whatever the others; wrong: ban), too easy a job target is dropped, a zero
/// target bans, and a job `prev_id` we have not seen waits for our next pool job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gossip_checks_follow_hugin_node() {
    let shutdown = CancellationToken::new();
    let src = jobs();
    let a = relay(src.clone(), &shutdown);
    let (job, _) = share_vector();
    let mut app = client(&a, 0x30);
    hello_and_job(&mut app).await;
    let mut n = node_peer(&a, 0xe1).await;

    // a valid signed share and a wrong unsigned one: stored with the signed share, no ban
    let signed = mined_share(&job, 300);
    let mixed = message_with(
        &"71".repeat(32),
        "aa",
        &job,
        &[signed.clone(), bogus_share(&job, 301)],
        0,
        40,
    );
    n.send(&json!({ "signal": true, "message": mixed })).await;
    eventually("stored", || a.has_message(&"71".repeat(32))).await;
    assert!(!a.is_banned(&[0xe1; 32]));
    let mut probe = client(&a, 0x32);
    probe.next().await.expect("hello");
    probe
        .send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 3 }))
        .await;
    let list = probe.until(|v| v.get("response").is_some()).await;
    let stored = list["response"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["hash"] == "71".repeat(32).as_str())
        .expect("in the pool")
        .clone();
    assert_eq!(stored["pow"]["shares"], json!([signed]));

    // two valid shares: stored
    let both = message_with(
        &"72".repeat(32),
        "ab",
        &job,
        &[mined_share(&job, 302), mined_share(&job, 303)],
        1,
        41,
    );
    n.send(&json!({ "signal": true, "message": both })).await;
    eventually("stored", || a.has_message(&"72".repeat(32))).await;

    // a job prev_id we have not seen: kept until our pool moves to it
    let other = job_with_other_prev_id();
    let later = message_with(
        &"73".repeat(32),
        "ac",
        &other,
        &[mined_share(&other, 304)],
        0,
        42,
    );
    n.send(&json!({ "signal": true, "message": later })).await;
    gossip_checked(&a, 3).await;
    assert!(!a.has_message(&"73".repeat(32)));
    src.set_job(PoolJob::from_value(other).unwrap());
    eventually("stored after the pool job", || {
        a.has_message(&"73".repeat(32))
    })
    .await;
    assert!(!a.is_banned(&[0xe1; 32]));

    // too easy for the default floor: dropped, no ban
    let strict = relay_with(jobs(), &shutdown, GOSSIP_MIN_DIFFICULTY);
    let mut strict_app = client(&strict, 0x31);
    hello_and_job(&mut strict_app).await;
    let mut s = node_peer(&strict, 0xe2).await;
    let easy = message_with(
        &"74".repeat(32),
        "ad",
        &job,
        &[mined_share(&job, 305)],
        0,
        43,
    );
    s.send(&json!({ "signal": true, "message": easy })).await;
    gossip_checked(&strict, 1).await;
    assert!(!strict.has_message(&"74".repeat(32)));
    assert!(!strict.is_banned(&[0xe2; 32]));

    // a zero target bans
    let mut zero_job = job.clone();
    zero_job["target"] = json!("00000000");
    let zero = message_with(
        &"75".repeat(32),
        "ae",
        &zero_job,
        &[mined_share(&job, 306)],
        0,
        44,
    );
    s.send(&json!({ "signal": true, "message": zero })).await;
    assert!(s.until_closed().await);
    assert!(strict.is_banned(&[0xe2; 32]));

    // the signed share is wrong: ban, also next to a valid one
    let wrong = message_with(
        &"76".repeat(32),
        "af",
        &job,
        &[mined_share(&job, 308), bogus_share(&job, 307)],
        1,
        45,
    );
    n.send(&json!({ "signal": true, "message": wrong })).await;
    assert!(n.until_closed().await);
    assert!(a.is_banned(&[0xe1; 32]));
    shutdown.cancel();
}

/// A Hugin app as `bridge/test/hugin-client-sim.js` runs it against a difficulty-100 pool job,
/// handed out with the default app difficulty (200, so every app share is a pool share too):
/// hello, `job_request`, then no packet for longer than [`HELLO_TIMEOUT`] (which only bounds the
/// wait for the first packet) while it searches a share, then a post of the sim's shape. The
/// connection stays open through the search, also when another app arrives at the client limit
/// (the searching app connected recently), and the post is accepted, pushed, synced back and
/// gossiped to a relay with the default difficulty floor.
#[tokio::test]
async fn app_silent_while_searching_a_share_then_posts() {
    let shutdown = CancellationToken::new();
    let job = sim_pool_job();
    assert_eq!(
        pow::share_difficulty("285c8f02"),
        Some(MIN_SHARE_DIFFICULTY)
    );
    let src = Arc::new(ManualJobSource::default());
    src.set_job(PoolJob::from_value(job.clone()).unwrap());
    let mut cfg = RelayConfig::new(ADDRESS.into());
    cfg.flush_interval = Duration::from_millis(100);
    cfg.max_clients = 1;
    let a = Relay::new(
        cfg,
        src.clone(),
        Arc::new(HuginStats::default()),
        shutdown.clone(),
    )
    .unwrap();
    let b = relay_with(jobs(), &shutdown, GOSSIP_MIN_DIFFICULTY);
    link(&a, 0xa0, &b, 0xb0);
    eventually("A and B linked", || {
        a.stats().snapshot().xkrd_nodes == 1 && b.stats().snapshot().xkrd_nodes == 1
    })
    .await;

    let mut app = client(&a, 1);
    let hello = app.next().await.unwrap();
    assert_eq!(hello["address"], ADDRESS);
    app.send(&json!({ "type": "job_request", "id": "j1" }))
        .await;
    let got = app.until(|v| v["type"] == "job").await;
    assert_eq!(got["job"]["job_id"], "sim-1001");
    assert_eq!(got["job"]["target"], "14ae4701");
    assert_eq!(
        pow::share_difficulty("14ae4701"),
        Some(DEFAULT_APP_DIFFICULTY)
    );
    let job = got["job"].clone();

    // the share search: no packet from the app for HELLO_TIMEOUT + 30 s
    tokio::time::pause();
    tokio::time::advance(HELLO_TIMEOUT + Duration::from_secs(30)).await;
    tokio::time::resume();
    let mut other = client(&a, 2);
    assert!(other.next().await.is_none(), "the client limit holds");

    // the post as the sim builds it: random-looking hash, 48-byte hex cipher, id = timestamp
    let share = first_share(&job);
    let hash = "5e".repeat(32);
    let cipher = "c1".repeat(48);
    let message = message_with(&hash, &cipher, &job, &[share], 0, 7);
    let ts = message["timestamp"].clone();
    assert_eq!(message["id"], ts);
    app.send(&json!({ "type": "post", "message": message }))
        .await;
    let answer = app.until(|v| v.get("success").is_some()).await;
    assert_eq!(answer["success"], true, "{answer}");
    assert_eq!(answer["id"], ts);
    assert_eq!(src.submitted().len(), 1, "the share went to the pool");
    assert_eq!(src.submitted()[0].job_id, "sim-1001");

    let pushed = app.until(|v| v["type"] == "new-message").await;
    assert_eq!(pushed["messages"][0]["hash"], hash.as_str());
    app.send(&json!({ "request": true, "type": "some", "timestamp": 0, "id": 8 }))
        .await;
    let list = app.until(|v| v.get("response").is_some()).await;
    let mine = list["response"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["hash"] == hash.as_str())
        .expect("the message is in the pool");
    assert!(mine["pow"].get("job").is_none());
    assert_eq!(mine["cipher"], cipher.as_str());

    eventually("gossip to B", || b.has_message(&hash)).await;
    shutdown.cancel();
}

/// Sends one post from a new client connection and returns the answer.
async fn post_once(relay: &Relay, key: u8, message: Value) -> Value {
    let mut c = client(relay, key);
    c.next().await.expect("hello");
    c.send(&json!({ "type": "post", "message": message })).await;
    c.until(|v| v.get("success").is_some()).await
}

/// The vector job with a zero `prev_id`, as every XKR hashing blob (p2pool, any XKR pool) has.
fn job_with_zero_prev_id() -> Value {
    let (mut job, _) = share_vector();
    let mut blob = hex::decode(job["blob"].as_str().unwrap()).unwrap();
    let off = pow::nonce_offset(&blob) - 32;
    blob[off..off + 32].fill(0);
    job["blob"] = json!(hex::encode(blob));
    job["job_id"] = json!("zero-1");
    job
}

/// A relay without any job (no p2pool reachable) takes gossip on a zero `prev_id` job at once;
/// gossip on an unknown non-zero `prev_id` still waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gossip_with_a_zero_prev_id_needs_no_job() {
    let shutdown = CancellationToken::new();
    let a = relay_with(Arc::new(NoJobSource::default()), &shutdown, 1);
    let mut n = node_peer(&a, 0xe7).await;
    let job = job_with_zero_prev_id();
    let m = message_with(
        &"81".repeat(32),
        "aa",
        &job,
        &[mined_share(&job, 40)],
        0,
        80,
    );
    n.send(&json!({ "signal": true, "message": m })).await;
    eventually("zero prev_id gossip stored", || {
        a.has_message(&"81".repeat(32))
    })
    .await;

    let other = job_with_other_prev_id();
    let m = message_with(
        &"82".repeat(32),
        "ab",
        &other,
        &[mined_share(&other, 41)],
        0,
        81,
    );
    n.send(&json!({ "signal": true, "message": m })).await;
    gossip_checked(&a, 2).await;
    assert!(!a.has_message(&"82".repeat(32)));
    assert!(!a.is_banned(&[0xe7; 32]));
    shutdown.cancel();
}

/// A job source like the p2pool one: it knows only the jobs it handed out, and submits only
/// with a fee address.
#[derive(Debug)]
struct Picky {
    inner: Arc<ManualJobSource>,
    known: Value,
    submits: bool,
}

impl JobSource for Picky {
    fn jobs(&self) -> tokio::sync::watch::Receiver<Option<Arc<PoolJob>>> {
        self.inner.jobs()
    }

    fn submit(&self, share: ShareSubmit) -> futures::future::BoxFuture<'static, SubmitOutcome> {
        self.inner.submit(share)
    }

    fn is_ready(&self) -> bool {
        true
    }

    fn submits_shares(&self) -> bool {
        self.submits
    }

    fn job_known(&self, job_id: &str, blob: &[u8]) -> bool {
        self.known["job_id"] == job_id
            && self.known["blob"].as_str() == Some(hex::encode(blob).as_str())
    }
}

/// A post on a job the source did not hand out: `stale_job` and the current job, no strike.
/// Without a fee address the post is checked and stored, and nothing is submitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_jobs_and_posts_without_submits() {
    let shutdown = CancellationToken::new();
    let (job, _) = share_vector();
    let inner = jobs();
    let src = Arc::new(Picky {
        inner: inner.clone(),
        known: job.clone(),
        submits: false,
    });
    let a = relay_with(src, &shutdown, 1);
    // not handed out (same template, another job id): refused, no strike, the current job
    // follows
    let mut other = job.clone();
    other["job_id"] = json!("other-1");
    let mut c = client(&a, 0x40);
    c.next().await.expect("hello");
    let m = message_with(
        &"91".repeat(32),
        "aa",
        &other,
        &[mined_share(&other, 50)],
        0,
        90,
    );
    c.send(&json!({ "type": "post", "message": m })).await;
    let r = c.until(|v| v.get("success").is_some()).await;
    assert_eq!(r["reason"], "stale_job", "{r}");
    let pushed = c.until(|v| v["type"] == "job").await;
    assert_eq!(pushed["job"]["job_id"], job["job_id"]);
    assert!(!a.is_banned(&[0x40; 32]));

    // the job handed out: accepted without a submit
    let m = message_with(
        &"92".repeat(32),
        "ab",
        &job,
        &[mined_share(&job, 51)],
        0,
        91,
    );
    let r = post_once(&a, 0x41, m).await;
    assert_eq!(r["success"], true, "{r}");
    assert!(a.has_message(&"92".repeat(32)));
    assert!(inner.submitted().is_empty());
    let s = a.stats().snapshot();
    assert_eq!(s.shares_accepted, 1);
    assert_eq!(s.shares_submitted, 0);
    assert_eq!(s.shares_rejected_by_pool, 0);
    shutdown.cancel();
}
