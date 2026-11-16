//! Pool mining against a fake stratum pool (the pattern of the pool tests that moved from
//! `xkr-hugin` to `xkr-stratum`), written with the server-side message types of
//! `xkr_stratum::messages`: login with a job, a second job pushed after the first share, every
//! share checked by hashing it again.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xkr_miner::{AesImpl, Event, MinerOptions, StratumSource, run};
use xkr_stratum::messages::{
    JobParams, LoginParams, LoginResult, Message, SubmitParams, decode_line, encode_line,
};
use xkr_stratum::{LoginExtras, StratumConfig};

/// A 76-byte major-7 blob (CN-Turtle-Lite v2): header, `prev_id`, nonce at 35, the rest.
fn blob(fill: u8) -> String {
    format!(
        "0700{}{}{}{}",
        "01",
        hex::encode([fill; 32]),
        "00000000",
        hex::encode([fill; 37])
    )
}

fn job(id: &str, fill: u8) -> JobParams {
    JobParams {
        job_id: id.into(),
        blob: blob(fill),
        // difficulty 1: every hash is a share
        target: "ffffffff".into(),
        height: Some(10),
        algo: None,
    }
}

/// Whether `share` is the CN-Turtle-Lite v2 hash of `job`'s blob with its nonce and meets the
/// target.
fn valid(job: &JobParams, share: &SubmitParams) -> bool {
    let mut b = hex::decode(&job.blob).unwrap();
    let Ok(nonce) = hex::decode(&share.nonce) else {
        return false;
    };
    if nonce.len() != 4 {
        return false;
    }
    b[35..39].copy_from_slice(&nonce);
    let hash = xkr_crypto::cn_turtle_lite_slow_hash_v2(&b);
    hex::encode(hash.0) == share.result && xkr_stratum::target::meets_target(&hash.0, &job.target)
}

/// The fake pool: answers `login` with job `j1`, pushes `j2` after the first valid share,
/// reports every valid share (`job_id`) on `shares`, refuses invalid ones.
async fn fake_pool(
    listener: TcpListener,
    logins: mpsc::UnboundedSender<LoginParams>,
    shares: mpsc::UnboundedSender<String>,
) {
    let (sock, _) = listener.accept().await.unwrap();
    let (r, mut w) = sock.into_split();
    let mut lines = BufReader::new(r).lines();
    let jobs = [job("j1", 0x11), job("j2", 0x22)];
    let mut pushed = false;
    while let Ok(Some(line)) = lines.next_line().await {
        let msg = decode_line(&line).unwrap();
        let Message::Request { id, method, .. } = &msg else {
            continue;
        };
        let mut out = Vec::new();
        match method.as_str() {
            "login" => {
                let login: LoginParams = msg.params().unwrap();
                let _ = logins.send(login);
                out.push(Message::login_ok(
                    id.clone(),
                    &LoginResult {
                        id: "sess".into(),
                        job: jobs[0].clone(),
                        status: "OK".into(),
                    },
                ));
            }
            "submit" => {
                let share: SubmitParams = msg.params().unwrap();
                assert_eq!(share.id, "sess");
                let job = jobs.iter().find(|j| j.job_id == share.job_id);
                if job.is_some_and(|j| valid(j, &share)) {
                    out.push(Message::status(id.clone(), "OK"));
                    let _ = shares.send(share.job_id.clone());
                    if !pushed {
                        pushed = true;
                        out.push(Message::job(&jobs[1]));
                    }
                } else {
                    out.push(Message::error(id.clone(), -1, "Low difficulty share"));
                }
            }
            _ => out.push(Message::status(id.clone(), "KEEPALIVED")),
        }
        for m in out {
            w.write_all(format!("{}\n", encode_line(&m)).as_bytes())
                .await
                .unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_shares_follow_the_jobs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (login_tx, mut login_rx) = mpsc::unbounded_channel();
    let (share_tx, mut share_rx) = mpsc::unbounded_channel();
    tokio::spawn(fake_pool(listener, login_tx, share_tx));

    let shutdown = CancellationToken::new();
    let mut source = StratumSource::spawn(
        StratumConfig {
            pools: vec![format!("stratum+tcp://{addr}")],
            login: "SEKRpayout".into(),
            pass: "x".into(),
            agent: "xkr-miner-test".into(),
        },
        LoginExtras {
            rig_id: Some("rig1".into()),
        },
        shutdown.clone(),
    );
    // Stop once the pool has a valid share of the pushed job.
    let stopper = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(job_id) = share_rx.recv().await {
                seen.push(job_id);
                if seen.last().is_some_and(|j| j == "j2") {
                    break;
                }
            }
            shutdown.cancel();
            seen
        })
    };
    let opts = MinerOptions {
        threads: 1,
        aes: AesImpl::detect(),
        report_interval: Duration::from_secs(60),
        limit: 0,
        thread_name: "test-pool".into(),
    };
    let mut jobs_seen = Vec::new();
    let mut observer = |e: Event<'_>| {
        if let Event::Job(job) = e {
            jobs_seen.push((job.id.clone(), job.height, job.target.difficulty()));
        }
    };
    let stats = tokio::time::timeout(
        Duration::from_secs(120),
        run(&mut source, &opts, &mut observer, &shutdown),
    )
    .await
    .expect("a share of the second job within 120 s")
    .expect("hashing threads");
    let seen = stopper.await.unwrap();

    let login = login_rx.recv().await.unwrap();
    assert_eq!(login.login, "SEKRpayout");
    assert_eq!(login.rigid.as_deref(), Some("rig1"));
    assert_eq!(login.agent.as_deref(), Some("xkr-miner-test"));
    assert_eq!(seen.first().map(String::as_str), Some("j1"));
    assert_eq!(seen.last().map(String::as_str), Some("j2"));
    assert_eq!(
        &jobs_seen[..2],
        &[("j1".to_owned(), 10, 1), ("j2".to_owned(), 10, 1)]
    );
    assert_eq!(stats.rejected, 0, "every share hashed correctly");
    assert!(stats.accepted >= 2, "{stats:?}");
}
