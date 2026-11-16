//! The stratum server against a stand-in block template: login answers and their exact bytes,
//! login variants, target texts, autodiff, pushed jobs, the share checks in order (job id,
//! stale, duplicate, low difficulty, wrong result), the score that closes a miner, bans, and a
//! real CN-Turtle share from the xkr-stratum pool client that reaches the share sink.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::sync::CancellationToken;
use xkr_crypto::cn_turtle_lite_slow_hash_v2;
use xkr_p2pool::Difficulty;
use xkr_p2pool::stratum::job::{
    AUTODIFF_START, MAX_TARGET, MIN_DIFF, TARGET_4_BYTES_LIMIT, custom_diff, custom_user,
    share_target, target_hex,
};
use xkr_p2pool::stratum::{
    AutoDiff, HashingBlob, ShareSink, StratumServer, StratumServerConfig, TemplateDifficulties,
    TemplateSource,
};
use xkr_stratum::messages::{JobParams, LoginResult, Message, decode_line};
use xkr_stratum::target::{parse_target, target_for_difficulty};
use xkr_stratum::{ShareSubmit, StratumConfig, StratumPool, SubmitOutcome};
use xkr_types::Hash;

const WAIT: Duration = Duration::from_secs(20);

/// Header of the stand-in blobs: major 5, minor 16, a 5-byte timestamp varint; the nonce
/// follows the 32-byte `prev_id` at offset 39.
const HEADER: [u8; 7] = [5, 16, 0x80, 0x80, 0x80, 0x80, 0x01];
const NONCE_OFFSET: usize = 39;

/// The template state of the stand-in.
struct Templates {
    current: u32,
    live: Vec<u32>,
    mainchain: Difficulty,
    sidechain: Difficulty,
    disconnected: bool,
}

/// A stand-in block template: blobs differ by template id and extra nonce.
struct FakeTemplate {
    t: Mutex<Templates>,
}

impl FakeTemplate {
    fn new(mainchain: u64, sidechain: u64) -> Arc<Self> {
        Arc::new(Self {
            t: Mutex::new(Templates {
                current: 1,
                live: vec![1],
                mainchain: Difficulty::from_u64(mainchain),
                sidechain: Difficulty::from_u64(sidechain),
                disconnected: false,
            }),
        })
    }

    /// A new template; the old ones stay until [`Self::drop_template`].
    fn next_template(&self) -> u32 {
        let mut t = self.t.lock().unwrap();
        t.current += 1;
        let id = t.current;
        t.live.push(id);
        id
    }

    fn drop_template(&self, id: u32) {
        self.t.lock().unwrap().live.retain(|x| *x != id);
    }

    fn set_disconnected(&self, on: bool) {
        self.t.lock().unwrap().disconnected = on;
    }

    fn blob(template_id: u32, extra_nonce: u32) -> Vec<u8> {
        let mut b = HEADER.to_vec();
        b.extend_from_slice(&[0x11; 32]);
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&template_id.to_le_bytes());
        b.extend_from_slice(&extra_nonce.to_le_bytes());
        b.extend_from_slice(&[0x22; 24]);
        b.push(1);
        b
    }

    fn make(t: &Templates, template_id: u32, extra_nonce: u32) -> HashingBlob {
        HashingBlob {
            blob: Self::blob(template_id, extra_nonce),
            nonce_offset: NONCE_OFFSET,
            template_id,
            height: 2_000_000 + u64::from(template_id),
            sidechain_height: 30_000 + u64::from(template_id),
            mainchain_difficulty: t.mainchain,
            sidechain_difficulty: t.sidechain,
            seed_hash: Hash::ZERO,
        }
    }
}

impl TemplateSource for FakeTemplate {
    fn hashing_blob(&self, extra_nonce: u32) -> Option<HashingBlob> {
        let t = self.t.lock().unwrap();
        Some(Self::make(&t, t.current, extra_nonce))
    }

    fn hashing_blob_for(&self, template_id: u32, extra_nonce: u32) -> Option<HashingBlob> {
        let t = self.t.lock().unwrap();
        t.live
            .contains(&template_id)
            .then(|| Self::make(&t, template_id, extra_nonce))
    }

    fn difficulties(&self, template_id: u32) -> Option<TemplateDifficulties> {
        self.hashing_blob_for(template_id, 0)
            .map(|b| TemplateDifficulties::from(&b))
    }

    fn sidechain_difficulty(&self) -> Difficulty {
        self.t.lock().unwrap().sidechain
    }

    fn sidechain_tip_height(&self) -> Option<u64> {
        Some(30_000 + u64::from(self.t.lock().unwrap().current))
    }

    fn chain_window_size(&self) -> u64 {
        2160
    }

    fn disconnected(&self) -> bool {
        self.t.lock().unwrap().disconnected
    }
}

/// What reached the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Block {
        template_id: u32,
        nonce: u32,
        extra_nonce: u32,
    },
    Share {
        template_id: u32,
        nonce: u32,
        extra_nonce: u32,
    },
}

#[derive(Default)]
struct FakeSink {
    found: Mutex<Vec<Found>>,
}

impl ShareSink for FakeSink {
    fn submit_block(&self, template_id: u32, nonce: u32, extra_nonce: u32) {
        self.found.lock().unwrap().push(Found::Block {
            template_id,
            nonce,
            extra_nonce,
        });
    }

    fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool {
        self.found.lock().unwrap().push(Found::Share {
            template_id,
            nonce,
            extra_nonce,
        });
        true
    }
}

fn config() -> StratumServerConfig {
    StratumServerConfig {
        listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        ..StratumServerConfig::default()
    }
}

async fn server(template: &Arc<FakeTemplate>, sink: &Arc<FakeSink>) -> StratumServer {
    StratumServer::bind(config(), template.clone(), sink.clone())
        .await
        .unwrap()
}

/// A raw line client.
struct Miner {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
    next_id: u64,
}

impl Miner {
    async fn connect(addr: SocketAddr) -> Self {
        let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
        Self {
            lines: BufReader::new(r).lines(),
            w,
            next_id: 1,
        }
    }

    async fn send_raw(&mut self, line: &str) {
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }

    async fn call(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "id": id, "jsonrpc": "2.0", "method": method, "params": params });
        self.send_raw(&line.to_string()).await;
        id
    }

    /// The next line, `None` when the server closed the connection.
    async fn line(&mut self) -> Option<String> {
        tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("no answer in time")
            .ok()
            .flatten()
    }

    async fn value(&mut self) -> Value {
        serde_json::from_str(&self.line().await.expect("connection closed")).unwrap()
    }

    /// Logs in; returns the raw answer.
    async fn login(&mut self, login: &str) -> String {
        self.call(
            "login",
            json!({ "login": login, "pass": "x", "agent": "test/1.0" }),
        )
        .await;
        self.line().await.expect("connection closed")
    }

    async fn submit(
        &mut self,
        session: &str,
        job_id: &str,
        nonce: u32,
        result: &[u8; 32],
    ) -> Value {
        self.call(
            "submit",
            json!({
                "id": session,
                "job_id": job_id,
                "nonce": hex::encode(nonce.to_le_bytes()),
                "result": hex::encode(result),
            }),
        )
        .await;
        self.value().await
    }
}

fn error_message(v: &Value) -> Option<&str> {
    v.get("error")?.get("message")?.as_str()
}

/// A claimed result whose last 8 bytes (little-endian) are `tail`.
fn result_with_tail(tail: u64) -> [u8; 32] {
    let mut r = [0u8; 32];
    r[24..].copy_from_slice(&tail.to_le_bytes());
    r
}

#[tokio::test]
async fn login_answer_has_the_cpp_bytes_and_reads_as_stratum_types() {
    let template = FakeTemplate::new(1_000_000_000, 1_000_000);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;
    let mut m = Miner::connect(srv.local_addr()).await;
    let raw = m.login("SEKRminer+5000").await;

    let msg = decode_line(&raw).unwrap();
    let login: LoginResult = msg.result().unwrap();
    assert_eq!(login.status, "OK");
    let job: JobParams = login.job.clone();
    assert_eq!(job.job_id, "1");
    assert_eq!(job.algo.as_deref(), Some("cryptonight_turtle"));
    assert_eq!(job.height, Some(2_000_001));
    let target = Difficulty::from_u64(5001).target();
    assert_eq!(job.target, target_hex(target));
    assert_eq!(job.target.len(), 8);

    // exactly the C++ answer, field order included
    let expected = format!(
        "{{\"id\":1,\"jsonrpc\":\"2.0\",\"result\":{{\"id\":\"{}\",\"job\":{{\"blob\":\"{}\",\"job_id\":\"1\",\"target\":\"{}\",\"algo\":\"cryptonight_turtle\",\"height\":2000001,\"seed_hash\":\"{}\"}},\"extensions\":[\"algo\"],\"status\":\"OK\"}}}}",
        login.id,
        job.blob,
        job.target,
        "0".repeat(64)
    );
    assert_eq!(raw, expected);
    assert!(u32::from_str_radix(&login.id, 16).unwrap() != 0);

    // the blob is template 1's, with a zero nonce
    let bytes = hex::decode(&job.blob).unwrap();
    assert_eq!(xkr_stratum::blob::nonce_offset(&bytes), NONCE_OFFSET);
    assert_eq!(&bytes[NONCE_OFFSET..NONCE_OFFSET + 4], &[0; 4]);
    assert_eq!(
        &bytes[NONCE_OFFSET + 4..NONCE_OFFSET + 8],
        &1u32.to_le_bytes()
    );

    // keepalived is answered; a second login closes the connection
    m.call("keepalived", json!({ "id": login.id })).await;
    let v = m.value().await;
    assert_eq!(v["result"]["status"], "KEEPALIVED");
    m.call("login", json!({ "login": "again" })).await;
    assert_eq!(m.line().await, None);

    tokio::time::timeout(WAIT, async {
        while srv.stats().connections != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("connection still open");
}

#[tokio::test]
async fn autodiff_login_starts_at_500k_and_other_logins() {
    let template = FakeTemplate::new(1_000_000_000, 1_000_000);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;

    // no difficulty in the login: autodiff starts at 500 000
    let mut m = Miner::connect(srv.local_addr()).await;
    let raw = m.login("SEKRminer.rig1").await;
    let login: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    assert_eq!(login.job.target, target_hex(AUTODIFF_START));
    let stats = srv.stats();
    assert_eq!(stats.workers.len(), 1);
    assert_eq!(stats.workers[0].name.as_deref(), Some("SEKRminer"));

    // a share difficulty below the custom one wins (the target is the easier one)
    let easy = FakeTemplate::new(1_000_000_000, 2000);
    let easy_server = server(&easy, &sink).await;
    let mut easy_miner = Miner::connect(easy_server.local_addr()).await;
    let raw = easy_miner.login("x+50000").await;
    let login: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    assert_eq!(
        login.job.target,
        target_hex(Difficulty::from_u64(2000).target())
    );

    // a template below MIN_DIFF gives MIN_DIFF
    let trivial = FakeTemplate::new(1, 1);
    let trivial_server = server(&trivial, &sink).await;
    let mut trivial_miner = Miner::connect(trivial_server.local_addr()).await;
    let raw = trivial_miner.login("x+1").await;
    let login: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    assert_eq!(login.job.target, target_hex(MAX_TARGET));
    assert_eq!(login.job.target, "37894100");

    // login refused while the P2P side is disconnected
    template.set_disconnected(true);
    let mut refused = Miner::connect(srv.local_addr()).await;
    let raw = refused.login("x").await;
    let v: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(error_message(&v), Some("Disconnected from P2Pool network"));
    assert_eq!(refused.line().await, None);

    // malformed requests close the connection
    for bad in [
        "not json",
        "[1]",
        r#"{"method":"login","params":{"login":"x"}}"#,
        r#"{"id":-1,"method":"login","params":{"login":"x"}}"#,
        r#"{"id":"1","method":"login","params":{"login":"x"}}"#,
        r#"{"id":1,"method":"mining.subscribe","params":[]}"#,
        r#"{"id":1,"method":"login","params":{"login":5}}"#,
        r#"{"id":1,"method":"login"}"#,
    ] {
        let mut b = Miner::connect(srv.local_addr()).await;
        b.send_raw(bad).await;
        assert_eq!(b.line().await, None, "{bad}");
    }

    // a line longer than the buffer closes the connection
    let mut big = Miner::connect(srv.local_addr()).await;
    big.w.write_all(&[b' '; 2048]).await.unwrap();
    assert_eq!(big.line().await, None);

    // HTTP GET gets a short page
    let mut http = Miner::connect(srv.local_addr()).await;
    http.send_raw("GET / HTTP/1.1\r").await;
    let mut page = Vec::new();
    while let Some(l) = http.line().await {
        page.push(l);
    }
    assert_eq!(page.first().map(String::as_str), Some("HTTP/1.1 200 OK"));
    assert_eq!(
        page.last().map(String::as_str),
        Some("P2Pool Stratum online")
    );
}

#[test]
fn login_variants() {
    assert_eq!(custom_user("SEKRabc+1000").as_deref(), Some("SEKRabc"));
    assert_eq!(
        custom_diff("SEKRabc+1000"),
        Some(Difficulty::from_u64(1001))
    );
    assert_eq!(custom_user("x").as_deref(), Some("x"));
    assert_eq!(custom_diff("x"), None);
    assert_eq!(custom_user("wallet.rig1").as_deref(), Some("wallet"));
    assert_eq!(custom_diff("wallet.rig1"), None);
    assert_eq!(
        custom_diff("wallet+500"),
        Some(Difficulty::from_u64(MIN_DIFF))
    );
    assert_eq!(custom_diff("a+b+3000"), Some(Difficulty::from_u64(3001)));
    assert_eq!(custom_diff("a.rig.5000"), Some(Difficulty::from_u64(5001)));
    assert_eq!(custom_diff("a+5000x"), Some(Difficulty::from_u64(5001)));
    assert_eq!(custom_user("+1000"), None);
    assert_eq!(custom_user("a\"b,c\\d\u{7f}e+1").as_deref(), Some("abcde"));
    assert_eq!(custom_user(&"w".repeat(40)).map(|u| u.len()), Some(31));
    assert_eq!(custom_diff("x+0"), None);
    assert_eq!(custom_diff("x+"), None);
    // the number wraps to 0 past u64::MAX - 1, which the floor lifts to MIN_DIFF
    assert_eq!(
        custom_diff("x+99999999999999999999999"),
        Some(Difficulty::from_u64(MIN_DIFF))
    );
    assert_eq!(custom_diff("x+-1"), Some(Difficulty::from_u64(MIN_DIFF)));
}

#[test]
fn target_texts() {
    assert_eq!(target_hex(Difficulty::from_u64(1000).target()), "37894100");
    assert_eq!(target_for_difficulty(1000), "37894100");
    assert_eq!(target_hex(MAX_TARGET), "37894100");
    // 4 bytes up to difficulty 4 000 001, 8 bytes from 4 000 002 on
    assert_eq!(
        target_hex(Difficulty::from_u64(4_000_001).target()).len(),
        8
    );
    assert_eq!(
        target_hex(Difficulty::from_u64(4_000_002).target()).len(),
        16
    );
    assert_eq!(target_hex(TARGET_4_BYTES_LIMIT).len(), 8);
    assert_eq!(target_hex(TARGET_4_BYTES_LIMIT - 1).len(), 16);
    // what a miner reads from the text is what the server checks shares against
    for d in [1000u64, 1001, 5000, 77_777, 500_000, 1_234_567, 4_000_001] {
        let t = Difficulty::from_u64(d).target();
        let text = target_hex(t);
        assert_eq!(parse_target(&text), Some(share_target(t)), "{d}");
    }
    for d in [4_000_002u64, 10_000_000, 150_802_240, 1 << 40] {
        let t = Difficulty::from_u64(d).target();
        let text = target_hex(t);
        assert_eq!(text, hex::encode(t.to_le_bytes()));
        assert_eq!(parse_target(&text), Some(t), "{d}");
        assert_eq!(share_target(t), t);
    }
}

#[test]
fn autodiff_follows_the_hashrate() {
    // 10 kH/s at difficulty 300 000: a share every 30 s
    let mut a = AutoDiff::new();
    assert_eq!(a.difficulty(), 0);
    let mut t = 1_700_000_000u64;
    a.update(t, 300_000);
    assert_eq!(a.difficulty(), 300_000);
    for _ in 0..9 {
        t += 30;
        a.update(t, 300_000);
        assert_eq!(a.difficulty(), 300_000);
    }
    for _ in 0..200 {
        t += 30;
        a.update(t, 300_000);
        let d = a.difficulty();
        assert!((297_000..=300_000).contains(&d), "{d}");
    }
    assert!((9_900..=10_000).contains(&a.hashrate()));

    // the miner gets 4x faster: autodiff follows within a window
    for _ in 0..64 {
        t += 7;
        a.update(t, 300_000);
    }
    let d = a.difficulty();
    assert!((1_250_000..=1_300_000).contains(&d), "{d}");

    // no share for more than 150 s: an eighth less per job, never below MIN_DIFF
    a.decay(t + 150);
    assert_eq!(a.difficulty(), d);
    a.decay(t + 151);
    assert_eq!(a.difficulty(), d - d / 8);
    for _ in 0..200 {
        a.decay(t + 151);
    }
    assert_eq!(a.difficulty(), MIN_DIFF);

    // a miner far below MIN_DIFF stays at MIN_DIFF
    let mut slow = AutoDiff::new();
    let mut ts = 0;
    for _ in 0..20 {
        ts += 600;
        slow.update(ts, 1000);
    }
    assert_eq!(slow.difficulty(), MIN_DIFF);
    // decay before the first share does nothing
    let mut fresh = AutoDiff::new();
    fresh.decay(1_000_000);
    assert_eq!(fresh.difficulty(), 0);
}

#[tokio::test]
async fn new_template_pushes_a_job() {
    let template = FakeTemplate::new(1_000_000_000, 1_000_000);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;
    let mut m = Miner::connect(srv.local_addr()).await;
    let raw = m.login("x+20000").await;
    let first: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    // a second, not logged in connection gets nothing
    let _idle = Miner::connect(srv.local_addr()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let id = template.next_template();
    srv.on_block();
    let line = m.line().await.unwrap();
    let msg = decode_line(&line).unwrap();
    assert!(matches!(msg, Message::Notification { .. }));
    assert_eq!(msg.method(), Some("job"));
    let job: JobParams = msg.params().unwrap();
    assert_eq!(job.job_id, "2");
    assert_eq!(job.height, Some(2_000_000 + u64::from(id)));
    assert_eq!(job.target, first.job.target);
    assert_ne!(job.blob, first.job.blob);
    let bytes = hex::decode(&job.blob).unwrap();
    assert_eq!(
        &bytes[NONCE_OFFSET + 4..NONCE_OFFSET + 8],
        &id.to_le_bytes()
    );

    // getjob answers with a fresh job
    m.call("getjob", json!({ "id": first.id })).await;
    let v = m.value().await;
    assert_eq!(v["result"]["job_id"], "3");
    assert_eq!(v["result"]["algo"], "cryptonight_turtle");
}

#[tokio::test]
async fn share_checks_in_order_and_the_score() {
    // shares below the sidechain difficulty are not recomputed (unless full validation)
    let template = FakeTemplate::new(u64::MAX, 1 << 40);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;
    let mut m = Miner::connect(srv.local_addr()).await;
    let raw = m.login("x+1000").await;
    let login: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    let sid = login.id.clone();
    let good = result_with_tail(1 << 50);

    // unknown job id
    let v = m.submit(&sid, "ff", 1, &good).await;
    assert_eq!(error_message(&v), Some("Invalid job id"));
    // a share under the target counts
    let v = m.submit(&sid, "1", 1, &good).await;
    assert_eq!(v["result"]["status"], "OK", "{v}");
    assert!(v["error"].is_null());
    // the same nonce again
    let v = m.submit(&sid, "1", 1, &good).await;
    assert_eq!(error_message(&v), Some("Duplicate share"));
    // job ids are hex in either case
    let v = m.submit(&sid, "01", 2, &good).await;
    assert_eq!(v["result"]["status"], "OK");
    // stale: the template is gone
    template.next_template();
    template.drop_template(1);
    let v = m.submit(&sid, "1", 3, &good).await;
    assert_eq!(error_message(&v), Some("Stale share"));
    srv.on_block();
    let job: JobParams = decode_line(&m.line().await.unwrap())
        .unwrap()
        .params()
        .unwrap();
    assert_eq!(job.job_id, "2");
    assert!(sink.found.lock().unwrap().is_empty());
    let stats = srv.stats();
    assert_eq!(stats.total_stratum_shares, 2);
    assert_eq!(stats.workers[0].stratum_shares, 2);

    // three low-difficulty shares bring the score to -15 and close the connection
    let low = result_with_tail(u64::MAX);
    for nonce in 10..12 {
        let v = m.submit(&sid, "2", nonce, &low).await;
        assert_eq!(error_message(&v), Some("Low diff share"));
    }
    let v = m.submit(&sid, "2", 12, &low).await;
    // 2 good shares earned no points (not recomputed): 0 - 15
    assert_eq!(error_message(&v), Some("Low diff share"));
    assert_eq!(m.line().await, None);
    // localhost is never banned
    assert!(!srv.is_banned(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let mut again = Miner::connect(srv.local_addr()).await;
    assert!(
        decode_line(&again.login("x").await)
            .unwrap()
            .result::<LoginResult>()
            .is_ok()
    );

    // other addresses are banned for the ban time, IPv6 by its /64
    let ip: IpAddr = "203.0.113.5".parse().unwrap();
    assert!(!srv.is_banned(ip));
    srv.ban(ip);
    assert!(srv.is_banned(ip));
    srv.ban("2001:db8:1:2:3:4:5:6".parse().unwrap());
    assert!(srv.is_banned("2001:db8:1:2:ffff::1".parse().unwrap()));
    assert!(!srv.is_banned("2001:db8:1:3::1".parse().unwrap()));

    // malformed submits close the connection
    for (job_id, nonce, result) in [
        ("0", "01000000", "00".repeat(32)),
        ("zz", "01000000", "00".repeat(32)),
        ("2", "0100000", "00".repeat(32)),
        ("2", "0100000g", "00".repeat(32)),
        ("2", "01000000", "00".repeat(31)),
        ("2", "01000000", format!("{}zz", "00".repeat(31))),
    ] {
        let mut b = Miner::connect(srv.local_addr()).await;
        b.login("x").await;
        b.call(
            "submit",
            json!({ "id": "1", "job_id": job_id, "nonce": nonce, "result": result }),
        )
        .await;
        assert_eq!(b.line().await, None, "{job_id} {nonce} {result}");
    }
}

#[tokio::test]
async fn wrong_results_at_the_share_difficulty_are_recomputed() {
    let template = FakeTemplate::new(u64::MAX, 1);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;
    let mut m = Miner::connect(srv.local_addr()).await;
    let raw = m.login("x+1000").await;
    let login: LoginResult = decode_line(&raw).unwrap().result().unwrap();
    // at the share difficulty, not at the main-chain one
    let claimed = result_with_tail(2);
    for nonce in 0..2 {
        let v = m.submit(&login.id, "1", nonce, &claimed).await;
        assert_eq!(error_message(&v), Some("Invalid PoW"));
    }
    let v = m.submit(&login.id, "1", 2, &claimed).await;
    assert_eq!(error_message(&v), Some("Invalid PoW"));
    assert_eq!(m.line().await, None);
    assert!(sink.found.lock().unwrap().is_empty());
}

/// Hashes nonces from 0 until one meets `target`; returns the nonce and the hash.
fn mine(blob_hex: &str, target: u64) -> (u32, [u8; 32]) {
    let mut blob = hex::decode(blob_hex).unwrap();
    let off = xkr_stratum::blob::nonce_offset(&blob);
    for nonce in 0u32.. {
        blob[off..off + 4].copy_from_slice(&nonce.to_le_bytes());
        let h = cn_turtle_lite_slow_hash_v2(&blob).0;
        if u64::from_le_bytes(h[24..].try_into().unwrap()) < target {
            return (nonce, h);
        }
    }
    unreachable!()
}

#[tokio::test]
async fn a_real_share_from_the_pool_client_reaches_the_sink() {
    // difficulty 1 on both chains: every valid share is a block and a sidechain share
    let template = FakeTemplate::new(1, 1);
    let sink = Arc::new(FakeSink::default());
    let srv = server(&template, &sink).await;
    let shutdown = CancellationToken::new();
    let pool = StratumPool::spawn(
        StratumConfig {
            pools: vec![srv.local_addr().to_string()],
            login: "SEKRrelay+1000".into(),
            pass: "x".into(),
            agent: "test/1.0".into(),
        },
        shutdown.clone(),
    );
    let mut job_rx = pool.jobs();
    let job = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(j) = job_rx.borrow_and_update().clone() {
                return j;
            }
            job_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("no job");
    assert_eq!(job.raw["algo"], "cryptonight_turtle");
    let target = parse_target(&job.target).unwrap();
    assert_eq!(job.target, "37894100");
    assert_eq!(target, share_target(MAX_TARGET));

    let blob = job.blob.clone();
    let (nonce, hash) = tokio::task::spawn_blocking(move || mine(&blob, target))
        .await
        .unwrap();
    let outcome = pool
        .submit(ShareSubmit {
            job_id: job.job_id.clone(),
            nonce: hex::encode(nonce.to_le_bytes()),
            result: hex::encode(hash),
        })
        .await;
    assert_eq!(outcome, SubmitOutcome::Accepted);

    let bytes = hex::decode(&job.blob).unwrap();
    let extra_nonce = u32::from_le_bytes(
        bytes[NONCE_OFFSET + 8..NONCE_OFFSET + 12]
            .try_into()
            .unwrap(),
    );
    let found = sink.found.lock().unwrap().clone();
    assert_eq!(
        found,
        vec![
            Found::Block {
                template_id: 1,
                nonce,
                extra_nonce
            },
            Found::Share {
                template_id: 1,
                nonce,
                extra_nonce
            },
        ]
    );
    let stats = srv.stats();
    assert_eq!(stats.shares_found, 1);
    assert_eq!(stats.total_stratum_shares, 1);
    assert_eq!(stats.workers[0].sidechain_shares, 1);
    assert_eq!(stats.workers[0].name.as_deref(), Some("SEKRrelay"));
    assert_eq!(stats.to_json()["shares_found"], 1);

    // the same share again is a duplicate for the client too
    let again = pool
        .submit(ShareSubmit {
            job_id: job.job_id.clone(),
            nonce: hex::encode(nonce.to_le_bytes()),
            result: hex::encode(hash),
        })
        .await;
    assert_eq!(again, SubmitOutcome::Duplicate);
    shutdown.cancel();
}
