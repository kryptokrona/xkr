//! One miner connection: reading request lines, `login`, `submit`, `keepalived`, `getjob`, the
//! share checks and the score (`StratumServer::StratumClient`, `on_login`, `on_submit`,
//! `on_share_found`, `on_after_share_found`, `process_request`, `on_read`).

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use xkr_crypto::cn_turtle_lite_slow_hash_v2_reused;
use xkr_stratum::messages::SubmitParams;
use xkr_types::Hash;

use super::autodiff::AutoDiff;
use super::job::{
    AUTODIFF_START, BAD_SHARE_POINTS, BAN_THRESHOLD_POINTS, GOOD_SHARE_POINTS, HTTP_RESPONSE,
    JOBS_SIZE, JobFields, MAX_SCORE, MAX_TARGET, TARGET_4_BYTES_LIMIT, custom_diff, custom_user,
    error_line, getjob_line, hash_tail, hashes_for_target, login_line, parse_job_id, parse_nonce,
    parse_result, share_target, status_line, target_difficulty, template_target,
};
use super::server::{Client, Inner, lock, now_secs, random32};
use super::stats::WorkerStats;
use super::{HashingBlob, LOGIN_TIMEOUT, STRATUM_BUF_SIZE, TemplateDifficulties};
use crate::difficulty::Difficulty;

/// Nonces remembered per job for the duplicate check.
const MAX_NONCES_PER_JOB: usize = 4096;

/// Lines for a connection's writer.
#[derive(Debug)]
pub(crate) enum Out {
    /// A line to send (with its `\n`).
    Line(String),
}

/// How a connection ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Close it.
    Close,
    /// Ban the address for the ban time and close it.
    Ban,
}

/// A job sent to the miner (`SavedJob`), with the nonces submitted for it.
#[derive(Clone, Debug, Default)]
struct SavedJob {
    job_id: u32,
    extra_nonce: u32,
    template_id: u32,
    target: u64,
    nonces: HashSet<u32>,
}

/// The state of one miner (`StratumClient` fields).
#[derive(Debug)]
pub(crate) struct ClientState {
    rpc_id: u32,
    job_counter: u32,
    connected_time: u64,
    jobs: [SavedJob; JOBS_SIZE],
    auto_diff: AutoDiff,
    custom_diff: Difficulty,
    custom_user: String,
    last_job_target: u64,
    score: i32,
    stratum_shares: u32,
    sidechain_shares: u32,
}

impl ClientState {
    /// A new connection at `now` (`StratumClient::on_connect`).
    pub(crate) fn new(now: u64) -> Self {
        Self {
            rpc_id: 0,
            job_counter: 0,
            connected_time: now,
            jobs: Default::default(),
            auto_diff: AutoDiff::new(),
            custom_diff: Difficulty::ZERO,
            custom_user: String::new(),
            last_job_target: 0,
            score: 0,
            stratum_shares: 0,
            sidechain_shares: 0,
        }
    }

    /// The miner has logged in.
    pub(crate) fn logged_in(&self) -> bool {
        self.rpc_id != 0
    }

    /// The target of a new job from a template target `base` (`on_blobs_ready`): the custom
    /// difficulty, else autodiff (at most 4 million; before the first share 500 000, halved
    /// every 16 s connected), never below [`super::job::MIN_DIFF`].
    pub(crate) fn job_target(&self, base: u64, now: u64, auto_diff: bool) -> u64 {
        let mut target = base;
        if self.custom_diff.lo() != 0 {
            target = target.max(self.custom_diff.target());
        } else if auto_diff {
            target = target.max(TARGET_4_BYTES_LIMIT);
            if self.auto_diff.difficulty() != 0 {
                target = target.max(Difficulty::from_u64(self.auto_diff.difficulty()).target());
            } else {
                target = target.max(AUTODIFF_START);
                let halvings = now.saturating_sub(self.connected_time) / 16;
                let mut i = 0;
                while i < halvings && target < MAX_TARGET {
                    target *= 2;
                    i += 1;
                }
            }
        }
        target.min(MAX_TARGET)
    }

    /// Before a pushed job: autodiff without a share for a while comes down
    /// ([`AutoDiff::decay`]), unless the miner set its own difficulty.
    pub(crate) fn decay_auto_diff(&mut self, now: u64, auto_diff: bool) {
        if auto_diff && self.custom_diff.lo() == 0 {
            self.auto_diff.decay(now);
        }
    }

    /// Remembers a new job and returns its id (`++m_perConnectionJobId`).
    pub(crate) fn save_job(&mut self, extra_nonce: u32, template_id: u32, target: u64) -> u32 {
        self.job_counter = self.job_counter.wrapping_add(1);
        let job_id = self.job_counter;
        self.jobs[job_id as usize % JOBS_SIZE] = SavedJob {
            job_id,
            extra_nonce,
            template_id,
            target,
            nonces: HashSet::new(),
        };
        self.last_job_target = target;
        job_id
    }

    /// `on_reset_share_counters`.
    pub(crate) fn reset_share_counters(&mut self) {
        self.stratum_shares = 0;
        self.sidechain_shares = 0;
    }

    /// The worker list entry (`api_update_local_stats`).
    pub(crate) fn worker(&self, addr: SocketAddr, now: u64, pool_diff: Difficulty) -> WorkerStats {
        WorkerStats {
            addr,
            uptime: now.saturating_sub(self.connected_time),
            difficulty: target_difficulty(self.last_job_target, pool_diff),
            hashrate: self.auto_diff.hashrate(),
            name: self.logged_in().then(|| self.custom_user.clone()),
            stratum_shares: self.stratum_shares,
            sidechain_shares: self.sidechain_shares,
        }
    }
}

/// The job fields of a hashing blob.
fn fields(hb: &HashingBlob, job_id: u32, target: u64) -> JobFields<'_> {
    JobFields {
        blob: &hb.blob,
        job_id,
        target,
        height: hb.height,
        seed_hash: &hb.seed_hash,
    }
}

/// Runs one connection until it closes, then forgets it (and bans its address when it broke
/// the rules and `ip_bans`: the address is the miner's own). Returns how it ended.
pub(crate) async fn run<S>(inner: Arc<Inner>, stream: S, addr: SocketAddr, ip_bans: bool) -> End
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = Arc::new(Client {
        addr,
        out: tx,
        state: Mutex::new(ClientState::new(now_secs())),
    });
    let id = inner.add_client(client.clone());
    let end = serve(&inner, &client, stream, &mut rx).await;
    inner.remove_client(id);
    if end == End::Ban && ip_bans {
        inner.ban(addr.ip());
    }
    end
}

/// What handling one request line gives: lines to send, then maybe the end.
#[derive(Debug, Default)]
struct Reply {
    lines: Vec<String>,
    end: Option<End>,
}

impl Reply {
    fn line(line: String) -> Self {
        Self {
            lines: vec![line],
            end: None,
        }
    }

    fn ban() -> Self {
        Self {
            lines: Vec::new(),
            end: Some(End::Ban),
        }
    }

    fn close_with(line: String) -> Self {
        Self {
            lines: vec![line],
            end: Some(End::Close),
        }
    }
}

/// `StratumClient::on_read`: lines of at most [`STRATUM_BUF_SIZE`] bytes; an HTTP `GET` or
/// `HEAD` gets [`HTTP_RESPONSE`] and the connection closes.
async fn serve<S>(
    inner: &Arc<Inner>,
    client: &Arc<Client>,
    stream: S,
    rx: &mut mpsc::UnboundedReceiver<Out>,
) -> End
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let mut stop = inner.stop.subscribe();
    if *stop.borrow_and_update() {
        return End::Close;
    }
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut buf = vec![0u8; STRATUM_BUF_SIZE];
    let mut len = 0usize;
    let login_deadline = tokio::time::sleep(LOGIN_TIMEOUT);
    tokio::pin!(login_deadline);
    let mut waiting_for_login = true;

    loop {
        tokio::select! {
            _ = stop.changed() => return End::Close,
            () = &mut login_deadline, if waiting_for_login => {
                if !lock(&client.state).logged_in() {
                    return End::Ban;
                }
                waiting_for_login = false;
            }
            out = rx.recv() => {
                let Some(Out::Line(line)) = out else {
                    return End::Close;
                };
                if wr.write_all(line.as_bytes()).await.is_err() {
                    return End::Close;
                }
            }
            r = rd.read(&mut buf[len..]) => {
                let n = match r {
                    Ok(0) | Err(_) => return End::Close,
                    Ok(n) => n,
                };
                let mut scan = len;
                len += n;
                let mut start = 0;
                while let Some(pos) = buf[scan..len].iter().position(|&c| c == b'\n') {
                    let end_of_line = scan + pos;
                    let line = &buf[start..end_of_line];
                    if inner.config.http && (line.starts_with(b"GET ") || line.starts_with(b"HEAD")) {
                        let text = if line.starts_with(b"GET ") {
                            HTTP_RESPONSE
                        } else {
                            &HTTP_RESPONSE[..HTTP_RESPONSE.len() - 21]
                        };
                        let _ = wr.write_all(text.as_bytes()).await;
                        return End::Close;
                    }
                    let reply = handle_line(inner, client, line).await;
                    for l in &reply.lines {
                        if wr.write_all(l.as_bytes()).await.is_err() {
                            return End::Close;
                        }
                    }
                    if let Some(end) = reply.end {
                        return end;
                    }
                    start = end_of_line + 1;
                    scan = start;
                }
                buf.copy_within(start..len, 0);
                len -= start;
                if len == buf.len() {
                    // a line longer than the buffer
                    return End::Ban;
                }
            }
        }
    }
}

/// `process_request`: a JSON object with a `u32` `id` and a known `method`; anything else bans.
async fn handle_line(inner: &Arc<Inner>, client: &Arc<Client>, line: &[u8]) -> Reply {
    let Ok(doc) = serde_json::from_slice::<Value>(line) else {
        return Reply::ban();
    };
    let Some(obj) = doc.as_object() else {
        return Reply::ban();
    };
    let Some(id) = obj
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
    else {
        return Reply::ban();
    };
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Reply::ban();
    };
    let params = obj.get("params").filter(|p| p.is_object());
    match method {
        "login" => {
            let Some(login) = params.and_then(|p| p.get("login")).and_then(Value::as_str) else {
                return Reply::ban();
            };
            on_login(inner, client, id, login)
        }
        "submit" => {
            let Some(p) =
                params.and_then(|p| serde_json::from_value::<SubmitParams>(p.clone()).ok())
            else {
                return Reply::ban();
            };
            if p.nonce.len() != 8 || p.result.len() != 64 {
                return Reply::ban();
            }
            on_submit(inner, client, id, &p).await
        }
        "keepalived" => Reply::line(status_line(id, "KEEPALIVED")),
        "getjob" => on_getjob(inner, client, id),
        _ => Reply::ban(),
    }
}

/// `on_login`: refused while the P2P side is disconnected or there is no template; a second
/// login bans. The answer carries the session id and the first job.
fn on_login(inner: &Inner, client: &Client, id: u32, login: &str) -> Reply {
    if inner.source.disconnected() {
        return Reply::close_with(error_line(id, "Disconnected from P2Pool network"));
    }
    let mut st = lock(&client.state);
    if st.logged_in() {
        return Reply::ban();
    }
    let extra_nonce = inner.next_extra_nonce();
    let Some(hb) = inner.source.hashing_blob(extra_nonce) else {
        return Reply::close_with(error_line(id, "No block template yet"));
    };
    let mut target = template_target(hb.mainchain_difficulty, hb.sidechain_difficulty);
    if let Some(d) = custom_diff(login) {
        st.custom_diff = d;
        target = target.max(d.target());
    } else if inner.config.auto_diff {
        target = target.max(AUTODIFF_START.max(TARGET_4_BYTES_LIMIT));
    }
    let target = target.min(MAX_TARGET);
    st.custom_user = custom_user(login).unwrap_or_default();
    let job_id = st.save_job(extra_nonce, hb.template_id, target);
    let rpc_id = loop {
        let r = random32();
        if r != 0 {
            break r;
        }
    };
    st.rpc_id = rpc_id;
    Reply::line(login_line(id, rpc_id, &fields(&hb, job_id, target)))
}

/// `getjob`: a new job for a logged-in miner, as the answer.
fn on_getjob(inner: &Inner, client: &Client, id: u32) -> Reply {
    let mut st = lock(&client.state);
    if !st.logged_in() {
        return Reply::line(error_line(id, "Unauthenticated"));
    }
    let extra_nonce = inner.next_extra_nonce();
    let Some(hb) = inner.source.hashing_blob(extra_nonce) else {
        return Reply::line(error_line(id, "No block template yet"));
    };
    let base = template_target(hb.mainchain_difficulty, hb.sidechain_difficulty).min(MAX_TARGET);
    let target = st.job_target(base, now_secs(), inner.config.auto_diff);
    let job_id = st.save_job(extra_nonce, hb.template_id, target);
    Reply::line(getjob_line(id, &fields(&hb, job_id, target)))
}

/// A submitted share that passed the job lookup (`SubmittedShare`).
#[derive(Clone, Copy, Debug)]
struct Share {
    template_id: u32,
    nonce: u32,
    extra_nonce: u32,
    target: u64,
    hashes: u64,
    result: Hash,
    difficulties: TemplateDifficulties,
    high_enough: bool,
    timestamp: u64,
}

/// What became of a share (`SubmittedShare::Result`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShareResult {
    Stale,
    CouldntCheckPow,
    LowDiff,
    InvalidPow,
    Banned,
    SubmitFailed,
    Ok,
}

impl ShareResult {
    /// The answer to the miner (`on_after_share_found`).
    fn line(self, id: u32) -> String {
        let message = match self {
            Self::Ok => return status_line(id, "OK"),
            Self::Stale => "Stale share",
            Self::CouldntCheckPow => "Couldn't check PoW",
            Self::LowDiff => "Low diff share",
            Self::InvalidPow => "Invalid PoW",
            Self::Banned => "Banned",
            Self::SubmitFailed => "Submit failed",
        };
        error_line(id, message)
    }
}

/// `on_submit` and `on_after_share_found`: job id, nonce and result (bans when malformed),
/// the job (`Invalid job id`), its template (`Stale share`), a nonce sent before
/// (`Duplicate share`); a result at the main-chain difficulty goes to
/// [`super::ShareSink::submit_block`] at once; autodiff counts the share; then the share check
/// and the score (a bad share at [`BAN_THRESHOLD_POINTS`] or below bans).
async fn on_submit(inner: &Arc<Inner>, client: &Arc<Client>, id: u32, p: &SubmitParams) -> Reply {
    let Some(job_id) = parse_job_id(&p.job_id) else {
        return Reply::ban();
    };
    let Some(nonce) = parse_nonce(&p.nonce) else {
        return Reply::ban();
    };
    let Some(result) = parse_result(&p.result) else {
        return Reply::ban();
    };
    let now = now_secs();
    let share = {
        let mut guard = lock(&client.state);
        let st = &mut *guard;
        let slot = &mut st.jobs[job_id as usize % JOBS_SIZE];
        if slot.job_id != job_id {
            return Reply::line(error_line(id, "Invalid job id"));
        }
        let Some(difficulties) = inner.source.difficulties(slot.template_id) else {
            return Reply::line(error_line(id, "Stale share"));
        };
        if slot.nonces.contains(&nonce) {
            return Reply::line(error_line(id, "Duplicate share"));
        }
        if slot.nonces.len() < MAX_NONCES_PER_JOB {
            slot.nonces.insert(nonce);
        }
        let (template_id, extra_nonce) = (slot.template_id, slot.extra_nonce);
        if difficulties.mainchain_difficulty.check_pow(&result) {
            inner.sink.submit_block(template_id, nonce, extra_nonce);
        }
        let target = share_target(slot.target);
        let hashes = hashes_for_target(target);
        // shares of a template far below the sidechain tip (found while syncing) stay local
        let behind = inner.source.sidechain_tip_height().is_some_and(|tip| {
            difficulties
                .sidechain_height
                .saturating_add(inner.source.chain_window_size())
                < tip
        });
        let high_enough = difficulties.sidechain_difficulty.check_pow(&result) && !behind;
        st.auto_diff.update(now, hashes);
        st.stratum_shares += 1;
        Share {
            template_id,
            nonce,
            extra_nonce,
            target,
            hashes,
            result,
            difficulties,
            high_enough,
            timestamp: now,
        }
    };

    let (outcome, points) = check_share(inner, client.addr.ip(), share).await;

    let mut st = lock(&client.state);
    st.score = (st.score + points).min(MAX_SCORE);
    if outcome == ShareResult::Ok && share.high_enough {
        st.sidechain_shares += 1;
    }
    let bad = matches!(outcome, ShareResult::LowDiff | ShareResult::InvalidPow);
    Reply {
        lines: vec![outcome.line(id)],
        end: (bad && st.score <= BAN_THRESHOLD_POINTS).then_some(End::Ban),
    }
}

/// `on_share_found`: the proof of work is recomputed (CN-Turtle-Lite v2 over the hashing blob
/// with the nonce) for a share at the sidechain difficulty, or for every share with full
/// validation; a checked sidechain share goes to [`super::ShareSink::submit_share`]. The share
/// counts when its result is below the job target. Returns the result and the points.
async fn check_share(inner: &Arc<Inner>, ip: IpAddr, share: Share) -> (ShareResult, i32) {
    if inner.is_banned(ip) {
        return (ShareResult::Banned, 0);
    }
    let mut points = 0;
    let mut submit_failed = false;
    if share.high_enough || inner.config.full_validation {
        // one check at a time, like the C++ queue
        let _permit = inner.pow.acquire().await;
        let source = inner.source.clone();
        let (template_id, extra_nonce, nonce) = (share.template_id, share.extra_nonce, share.nonce);
        let pow = tokio::task::spawn_blocking(move || {
            let hb = source.hashing_blob_for(template_id, extra_nonce)?;
            let mut blob = hb.blob;
            let off = hb.nonce_offset;
            blob.get_mut(off..off.checked_add(4)?)?
                .copy_from_slice(&nonce.to_le_bytes());
            Some(cn_turtle_lite_slow_hash_v2_reused(&blob))
        })
        .await;
        let pow = match pow {
            Ok(Some(h)) => h,
            Ok(None) => return (ShareResult::Stale, 0),
            Err(_) => return (ShareResult::CouldntCheckPow, 0),
        };
        if pow != share.result {
            return (ShareResult::InvalidPow, BAD_SHARE_POINTS);
        }
        points = GOOD_SHARE_POINTS;
        if share.high_enough {
            let diff = share.difficulties.sidechain_difficulty.to_f64();
            let found = lock(&inner.stats).share_found(share.hashes, diff, now_secs());
            let sink = inner.sink.clone();
            let added = tokio::task::spawn_blocking(move || {
                sink.submit_share(template_id, nonce, extra_nonce)
            })
            .await
            .unwrap_or(false);
            if !added {
                submit_failed = true;
                lock(&inner.stats).share_failed(found);
            }
        }
    }
    if hash_tail(&share.result) < share.target {
        lock(&inner.stats).update(share.hashes, share.timestamp);
        let r = if submit_failed {
            ShareResult::SubmitFailed
        } else {
            ShareResult::Ok
        };
        (r, points)
    } else {
        (ShareResult::LowDiff, BAD_SHARE_POINTS)
    }
}
