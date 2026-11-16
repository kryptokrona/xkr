//! The stratum pool client (hugin-node `PoolConnector` in `pools.js`, the xmrig / cryptonote
//! pool `login` / `job` / `submit` / `keepalived` set).
//!
//! The protocol is line-delimited JSON-RPC over plain TCP, or TLS for a pool given as
//! `stratum+ssl://host:port`: the client logs in with a payout address, the pool pushes jobs
//! (`job` notifications, and the first one in the login answer), and the client submits shares
//! (`job_id`, `nonce`, `result`) under its session id. [`StratumPool`] keeps one connection with
//! reconnects and fail-over over a list of pools ([`Failover`]), can change its login while it
//! runs ([`StratumPool::set_login`]) and publishes where it is connected ([`PoolStatus`]).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::TlsConnector;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use tokio_util::sync::CancellationToken;

use crate::blob::{extract_prev_id, is_hex};

/// Port used when a pool is given without one (hugin-node `DEFAULT_POOL_PORT`).
pub const DEFAULT_POOL_PORT: u16 = 3333;
/// The last 8 job ids stay valid for submits (as in hugin-node).
pub const VALID_JOBS: usize = 8;

/// Longest line accepted from a pool.
pub const MAX_LINE: usize = 64 * 1024;
/// Wait before the next connection attempt (hugin-node: 2 s).
pub const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_mins(5);
/// Connection failures in a row before moving to the next pool (the default [`Failover`]).
const SWITCH_AFTER_FAILURES: u32 = 3;

/// A pool job as handed to clients.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolJob {
    pub job_id: String,
    pub blob: String,
    pub target: String,
    /// `prev_id` of the block template (freshness of client shares).
    pub prev_id: Option<[u8; 32]>,
    /// The job object exactly as the pool sent it (forwarded to clients verbatim).
    pub raw: Value,
}

impl PoolJob {
    /// A job object `{ job_id, blob, target, ... }`; `None` if it is not usable.
    #[must_use]
    pub fn from_value(raw: Value) -> Option<Self> {
        let job_id = raw.get("job_id")?.as_str()?.to_owned();
        let blob = raw.get("blob")?.as_str()?.to_owned();
        let target = raw.get("target")?.as_str()?.to_owned();
        if job_id.is_empty() || !is_hex(&blob) || blob.len() % 2 != 0 || !is_hex(&target) {
            return None;
        }
        let prev_id = hex::decode(&blob).ok().and_then(|b| extract_prev_id(&b));
        Some(Self {
            job_id,
            blob,
            target,
            prev_id,
            raw,
        })
    }
}

/// A share to submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSubmit {
    pub job_id: String,
    pub nonce: String,
    pub result: String,
}

/// What the pool said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    Accepted,
    /// The pool already has this share.
    Duplicate,
    /// `reason` as in hugin-node (`not_logged_in`, `stale_job`, `pool`, `timeout`, ...),
    /// `error` the pool's error text.
    Rejected {
        reason: String,
        error: Option<String>,
    },
}

impl SubmitOutcome {
    #[must_use]
    pub fn rejected(reason: &str, error: Option<String>) -> Self {
        Self::Rejected {
            reason: reason.to_owned(),
            error,
        }
    }

    /// hugin-node `has_auth_reject` for one outcome.
    #[must_use]
    pub fn is_auth_reject(&self) -> bool {
        match self {
            Self::Rejected { reason, error } => {
                reason == "not_logged_in"
                    || (reason == "pool"
                        && error
                            .as_deref()
                            .is_some_and(|e| e.to_lowercase().contains("unauthenticated")))
            }
            _ => false,
        }
    }
}

/// Settings of [`StratumPool`].
#[derive(Debug, Clone)]
pub struct StratumConfig {
    /// `host:port` (port defaults to 3333) or `stratum+ssl://host:port` (TLS), tried in order
    /// with fail-over ([`PoolAddr`]).
    pub pools: Vec<String>,
    /// Payout address (the pool login).
    pub login: String,
    pub pass: String,
    pub agent: String,
}

/// Optional login fields beyond [`StratumConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginExtras {
    /// Rig name sent as `rigid` in the `login` parameters (xmrig `--rig-id`); pools that know
    /// it show per-rig statistics.
    pub rig_id: Option<String>,
}

/// How [`StratumPool`] moves between its pools. The default is hugin-node's behaviour: the
/// next pool after a failed login or after 3 failed connections in a row, 2 s between attempts,
/// no way back to an earlier pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failover {
    /// Connection failures in a row before the next pool is tried (at least 1).
    pub switch_after: u32,
    /// While another pool is in use, how often the first pool is tried again (a connection and
    /// a login on a second socket); the client moves back to it as soon as that works. `None`:
    /// stay on the pool that works.
    pub return_to_first: Option<Duration>,
    /// Longest wait between two rounds over all pools without a login: the wait doubles from
    /// [`RECONNECT_DELAY`] up to this after every such round, and drops back after a login.
    pub max_backoff: Duration,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            switch_after: SWITCH_AFTER_FAILURES,
            return_to_first: None,
            max_backoff: RECONNECT_DELAY,
        }
    }
}

/// Where the client is ([`StratumPool::status`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolStatus {
    /// The pool in use or being tried ([`PoolAddr`] as text); `None` before the first attempt.
    pub pool: Option<String>,
    /// Logged in to `pool`.
    pub connected: bool,
    /// A whole round over all pools ended without a login, and no login worked since.
    pub unreachable: bool,
}

/// A pool address: `host:port` or `stratum+tcp://host:port` (plain TCP), or
/// `stratum+ssl://host:port` / `stratum+tls://host:port` (TLS, server certificate checked
/// against the system roots for `host`). The port defaults to [`DEFAULT_POOL_PORT`]; an IPv6
/// host is written in brackets (`[::1]:3333`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolAddr {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

/// `s` without the ASCII-case-insensitive `prefix`.
fn strip_scheme<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

impl PoolAddr {
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        let (tls, rest) = strip_scheme(s, "stratum+ssl://")
            .or_else(|| strip_scheme(s, "stratum+tls://"))
            .map_or_else(
                || (false, strip_scheme(s, "stratum+tcp://").unwrap_or(s)),
                |r| (true, r),
            );
        let (host, port) = rest
            .rsplit_once(':')
            .filter(|(h, _)| !h.is_empty() && (!h.contains(':') || h.ends_with(']')))
            .and_then(|(h, p)| p.parse::<u16>().ok().map(|port| (h, port)))
            .unwrap_or((rest, DEFAULT_POOL_PORT));
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        Self {
            host: host.to_owned(),
            port,
            tls,
        }
    }
}

impl fmt::Display for PoolAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.tls {
            f.write_str("stratum+ssl://")?;
        }
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

/// A pool address with its default port filled in ([`PoolAddr`] as text).
#[must_use]
pub fn pool_addr(s: &str) -> String {
    PoolAddr::parse(s).to_string()
}

/// TLS client settings for `stratum+ssl://` pools: the ring provider, the safe default protocol
/// versions and the system root certificates (`rustls-native-certs`).
pub fn tls_connector() -> Result<TlsConnector, String> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for e in &loaded.errors {
        tracing::debug!(error = %e, "loading a system root certificate failed");
    }
    let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        tracing::warn!("no system root certificates found: TLS pools cannot be verified");
    } else {
        tracing::debug!(added, ignored, "system root certificates for TLS pools");
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(cfg)))
}

type PoolReader = Box<dyn AsyncRead + Send + Unpin>;
type PoolWriter = Box<dyn AsyncWrite + Send + Unpin>;

struct Submit {
    share: ShareSubmit,
    /// The job blob the share was mined on (lowercase hex): the share is only sent when the
    /// current session issued `share.job_id` with this blob.
    blob: Option<String>,
    reply: oneshot::Sender<SubmitOutcome>,
}

/// A stratum pool connection with hugin-node's behaviour: login with the payout address, keep
/// the last [`VALID_JOBS`] job ids, reconnect after 2 s, move to the next pool when the login
/// fails or a pool keeps refusing connections ([`Failover`]), re-login when the pool says
/// `unauthenticated`.
#[derive(Debug)]
pub struct StratumPool {
    jobs: watch::Receiver<Option<Arc<PoolJob>>>,
    cmd: mpsc::Sender<SubmitCmd>,
    ready: Arc<AtomicBool>,
    login: watch::Sender<String>,
    status: watch::Receiver<PoolStatus>,
}

/// Command for the pool task.
struct SubmitCmd(Submit);

impl fmt::Debug for SubmitCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubmitCmd")
            .field("share", &self.0.share)
            .finish()
    }
}

impl StratumPool {
    /// Starts the pool task (needs a tokio runtime); it stops with `shutdown`.
    #[must_use]
    pub fn spawn(cfg: StratumConfig, shutdown: CancellationToken) -> Arc<Self> {
        Self::spawn_with(cfg, LoginExtras::default(), shutdown)
    }

    /// [`Self::spawn`] with extra login fields.
    #[must_use]
    pub fn spawn_with(
        cfg: StratumConfig,
        extras: LoginExtras,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        Self::spawn_with_failover(cfg, extras, Failover::default(), shutdown)
    }

    /// [`Self::spawn_with`] with a [`Failover`] policy.
    #[must_use]
    pub fn spawn_with_failover(
        cfg: StratumConfig,
        extras: LoginExtras,
        failover: Failover,
        shutdown: CancellationToken,
    ) -> Arc<Self> {
        let (jobs_tx, jobs) = watch::channel(None);
        let (cmd, cmd_rx) = mpsc::channel(256);
        let (login, login_rx) = watch::channel(cfg.login.clone());
        let (status_tx, status) = watch::channel(PoolStatus::default());
        let ready = Arc::new(AtomicBool::new(false));
        let tls = if cfg.pools.iter().any(|p| PoolAddr::parse(p).tls) {
            match tls_connector() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "TLS for the stratum pools unavailable");
                    None
                }
            }
        } else {
            None
        };
        let task = PoolTask {
            cfg,
            extras,
            failover,
            jobs_tx,
            cmd_rx,
            login_rx,
            status_tx,
            ready: ready.clone(),
            shutdown,
            tls,
            logged_in: false,
        };
        tokio::spawn(task.run());
        Arc::new(Self {
            jobs,
            cmd,
            ready,
            login,
            status,
        })
    }

    /// The latest job (`None` until the first one). The pool keeps the last job while it
    /// reconnects.
    #[must_use]
    pub fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>> {
        self.jobs.clone()
    }

    /// Submits one share under the current login. Refused at once with `not_logged_in` while
    /// there is no session, and with `stale_job` for a job id that is not among the last
    /// [`VALID_JOBS`].
    pub fn submit(&self, share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome> {
        self.submit_cmd(share, None)
    }

    /// [`Self::submit`] for a share mined on `blob` (hex): refused with `stale_job` unless the
    /// current session issued the share's job id with exactly this blob. Job ids restart with
    /// every connection on some pools (p2pool), so a share of an earlier connection could
    /// otherwise reach the pool under a job id it now uses for another job.
    pub fn submit_for_blob(
        &self,
        share: ShareSubmit,
        blob: &str,
    ) -> BoxFuture<'static, SubmitOutcome> {
        self.submit_cmd(share, Some(blob.to_ascii_lowercase()))
    }

    fn submit_cmd(
        &self,
        share: ShareSubmit,
        blob: Option<String>,
    ) -> BoxFuture<'static, SubmitOutcome> {
        if !self.ready.load(Ordering::Relaxed) {
            return Box::pin(async { SubmitOutcome::rejected("not_logged_in", None) });
        }
        let cmd = self.cmd.clone();
        Box::pin(async move {
            let (reply, rx) = oneshot::channel();
            if cmd
                .send(SubmitCmd(Submit { share, blob, reply }))
                .await
                .is_err()
            {
                return SubmitOutcome::rejected("socket_not_writable", None);
            }
            match tokio::time::timeout(SUBMIT_TIMEOUT + Duration::from_secs(1), rx).await {
                Ok(Ok(o)) => o,
                Ok(Err(_)) => SubmitOutcome::rejected("socket_not_writable", None),
                Err(_) => SubmitOutcome::rejected("timeout", None),
            }
        })
    }

    /// Logged in (shares can be submitted).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Logs in with `login` from now on. A different login ends the current connection and
    /// logs in again on a new one (pools refuse a second `login` on one connection), so the job
    /// ids of the old connection are no longer valid. The same login changes nothing.
    pub fn set_login(&self, login: String) {
        self.login.send_if_modified(|current| {
            if *current == login {
                false
            } else {
                *current = login;
                true
            }
        });
    }

    /// The current login.
    #[must_use]
    pub fn login(&self) -> String {
        self.login.borrow().clone()
    }

    /// Where the client is now.
    #[must_use]
    pub fn status(&self) -> PoolStatus {
        self.status.borrow().clone()
    }

    /// [`Self::status`] as a watch channel (changes on every connection attempt and login).
    #[must_use]
    pub fn status_changes(&self) -> watch::Receiver<PoolStatus> {
        self.status.clone()
    }
}

/// Why a pool session ended.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Shutdown,
    ConnectFailed,
    LoginFailed,
    Disconnected,
    /// [`StratumPool::set_login`] changed the login.
    LoginChanged,
    /// The first pool accepted a login again ([`Failover::return_to_first`]).
    Preferred,
}

struct PoolTask {
    cfg: StratumConfig,
    extras: LoginExtras,
    failover: Failover,
    jobs_tx: watch::Sender<Option<Arc<PoolJob>>>,
    cmd_rx: mpsc::Receiver<SubmitCmd>,
    login_rx: watch::Receiver<String>,
    status_tx: watch::Sender<PoolStatus>,
    ready: Arc<AtomicBool>,
    shutdown: CancellationToken,
    /// For `stratum+ssl://` pools (`None` when no pool uses TLS or TLS is unavailable).
    tls: Option<TlsConnector>,
    /// The session that just ended had logged in.
    logged_in: bool,
}

enum Pending {
    Login,
    Submit {
        reply: oneshot::Sender<SubmitOutcome>,
        at: Instant,
    },
    Keepalive,
}

fn request(id: u64, method: &str, params: &Value) -> String {
    json!({ "id": id, "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

/// hugin-node's normalisation of a pool error (`error.message || error.code || String(error)`).
fn error_text(e: &Value) -> String {
    match e {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| o.get("code").map(ToString::to_string))
            .unwrap_or_else(|| "[object Object]".into()),
        other => other.to_string(),
    }
}

/// hugin-node `is_duplicate_share_reject`: the words "duplicate" and "share".
fn is_duplicate(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .collect();
    words.contains(&"duplicate") && words.contains(&"share")
}

/// The session id of a `login` answer (`result.id` without an `error`).
fn login_session_id(msg: &Value) -> Option<&str> {
    if msg.get("error").is_some_and(|e| !e.is_null()) {
        return None;
    }
    msg.get("result")
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
}

/// Connects to `addr` (TCP, then the TLS handshake for a `stratum+ssl://` pool), each step
/// within [`CONNECT_TIMEOUT`].
async fn open_pool(
    addr: &PoolAddr,
    tls: Option<&TlsConnector>,
) -> Result<(PoolReader, PoolWriter), String> {
    let stream = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((addr.host.as_str(), addr.port)),
    )
    .await
    .map_err(|_| "connection timed out".to_owned())?
    .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    if !addr.tls {
        let (r, w) = stream.into_split();
        return Ok((Box::new(r), Box::new(w)));
    }
    let connector = tls.ok_or("TLS unavailable")?;
    let name = ServerName::try_from(addr.host.clone()).map_err(|e| e.to_string())?;
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, stream))
        .await
        .map_err(|_| "TLS handshake timed out".to_owned())?
        .map_err(|e| format!("TLS: {e}"))?;
    let (r, w) = tokio::io::split(tls);
    Ok((Box::new(r), Box::new(w)))
}

/// Whether `addr` accepts a connection and a login with `params` right now (a separate socket,
/// closed after the answer). Used to move back to the first pool.
async fn probe_login(addr: &PoolAddr, tls: Option<&TlsConnector>, params: &Value) -> bool {
    let Ok((r, w)) = open_pool(addr, tls).await else {
        return false;
    };
    let mut lines = FramedRead::new(r, LinesCodec::new_with_max_length(MAX_LINE));
    let mut out = FramedWrite::new(w, LinesCodec::new());
    if out.send(request(1, "login", params)).await.is_err() {
        return false;
    }
    let answer = tokio::time::timeout(LOGIN_TIMEOUT, async {
        while let Some(Ok(line)) = lines.next().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("id").and_then(Value::as_u64) == Some(1) {
                return login_session_id(&msg).is_some();
            }
        }
        false
    })
    .await;
    matches!(answer, Ok(true))
}

impl PoolTask {
    fn publish(&self, f: impl FnOnce(&mut PoolStatus)) {
        self.status_tx.send_modify(f);
    }

    fn login_params(&self, login: &str) -> Value {
        let mut params = json!({
            "login": login,
            "pass": self.cfg.pass,
            "agent": self.cfg.agent,
        });
        if let Some(rig) = &self.extras.rig_id {
            params["rigid"] = json!(rig);
        }
        params
    }

    async fn run(mut self) {
        if self.cfg.pools.is_empty() {
            tracing::warn!("no stratum pool configured");
            self.publish(|s| s.unreachable = true);
            return;
        }
        let n = self.cfg.pools.len();
        let switch_after = self.failover.switch_after.max(1);
        // Sessions without a login that make up one round over all pools.
        let round = switch_after.saturating_mul(u32::try_from(n).unwrap_or(u32::MAX));
        let mut index = 0usize;
        let mut failures = 0u32;
        let mut failed_sessions = 0u32;
        let mut backoff = RECONNECT_DELAY;
        loop {
            let addr = PoolAddr::parse(&self.cfg.pools[index]);
            let shown = addr.to_string();
            self.publish(|s| {
                s.pool = Some(shown);
                s.connected = false;
            });
            self.logged_in = false;
            let end = self.session(&addr, index == 0).await;
            self.ready.store(false, Ordering::Relaxed);
            self.publish(|s| s.connected = false);
            let had_login = std::mem::take(&mut self.logged_in);
            if had_login {
                failed_sessions = 0;
                backoff = RECONNECT_DELAY;
            }
            match end {
                End::Shutdown => return,
                End::LoginChanged => {
                    tracing::info!(pool = %addr, "stratum login changed; logging in again");
                    failures = 0;
                    continue;
                }
                End::Preferred => {
                    tracing::info!(pool = %addr, "the first stratum pool is reachable again; switching back");
                    index = 0;
                    failures = 0;
                    continue;
                }
                End::LoginFailed => {
                    tracing::warn!(pool = %addr, "stratum pool login failed; switching pool");
                    index = (index + 1) % n;
                    failures = 0;
                    failed_sessions += 1;
                }
                End::ConnectFailed => {
                    failures += 1;
                    failed_sessions += 1;
                    if failures >= switch_after {
                        if n > 1 {
                            tracing::warn!(pool = %addr, "stratum pool unreachable; switching pool");
                        }
                        index = (index + 1) % n;
                        failures = 0;
                    }
                }
                End::Disconnected => {
                    tracing::info!(pool = %addr, "stratum pool disconnected; reconnecting");
                    failures = 0;
                    if !had_login {
                        failed_sessions += 1;
                    }
                }
            }
            if failed_sessions >= round {
                self.publish(|s| s.unreachable = true);
            }
            let wait = if failed_sessions > 0 && failed_sessions.is_multiple_of(round) {
                let w = backoff;
                backoff = (backoff * 2).min(self.failover.max_backoff.max(RECONNECT_DELAY));
                w
            } else {
                RECONNECT_DELAY
            };
            // Wait, answering submits that arrive meanwhile.
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                tokio::select! {
                    () = self.shutdown.cancelled() => return,
                    () = tokio::time::sleep_until(deadline) => break,
                    Some(SubmitCmd(s)) = self.cmd_rx.recv() => {
                        let _ = s.reply.send(SubmitOutcome::rejected("not_logged_in", None));
                    }
                }
            }
        }
    }

    fn on_job(&self, job: &Value, valid: &mut VecDeque<(String, String)>) {
        let Some(j) = PoolJob::from_value(job.clone()) else {
            tracing::debug!("unusable job from the stratum pool");
            return;
        };
        valid.push_back((j.job_id.clone(), j.blob.to_ascii_lowercase()));
        while valid.len() > VALID_JOBS {
            valid.pop_front();
        }
        tracing::debug!(job_id = %j.job_id, target = %j.target, "stratum pool job");
        self.jobs_tx.send_replace(Some(Arc::new(j)));
    }

    #[allow(clippy::too_many_lines)]
    async fn session(&mut self, addr: &PoolAddr, on_first: bool) -> End {
        let opened = tokio::select! {
            () = self.shutdown.cancelled() => return End::Shutdown,
            r = open_pool(addr, self.tls.as_ref()) => r,
        };
        let (r, w) = match opened {
            Ok(rw) => rw,
            Err(e) => {
                tracing::debug!(pool = %addr, error = %e, "stratum pool connection failed");
                return End::ConnectFailed;
            }
        };
        tracing::info!(pool = %addr, tls = addr.tls, "connected to the stratum pool");
        let mut lines = FramedRead::new(r, LinesCodec::new_with_max_length(MAX_LINE));
        let mut out = FramedWrite::new(w, LinesCodec::new());

        let mut next_id = 1u64;
        let mut pending: HashMap<u64, Pending> = HashMap::new();
        let mut session: Option<String> = None;
        let mut valid: VecDeque<(String, String)> = VecDeque::new();
        let login = self.login_rx.borrow_and_update().clone();
        let login_params = self.login_params(&login);
        if out
            .send(request(next_id, "login", &login_params))
            .await
            .is_err()
        {
            return End::Disconnected;
        }
        pending.insert(next_id, Pending::Login);
        next_id += 1;
        let login_deadline = tokio::time::sleep(LOGIN_TIMEOUT);
        tokio::pin!(login_deadline);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_keepalive = Instant::now();
        // Trying the first pool again while on another one.
        let probe_every = self
            .failover
            .return_to_first
            .filter(|_| !on_first)
            .map(|d| d.max(Duration::from_secs(1)));
        let mut probe_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + probe_every.unwrap_or(KEEPALIVE),
            probe_every.unwrap_or(KEEPALIVE),
        );
        let (probe_tx, mut probe_rx) = mpsc::channel::<bool>(1);
        let mut probing = false;

        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return End::Shutdown,
                () = &mut login_deadline, if session.is_none() => {
                    return End::LoginFailed;
                }
                changed = self.login_rx.changed() => {
                    return if changed.is_ok() { End::LoginChanged } else { End::Shutdown };
                }
                _ = probe_tick.tick(), if probe_every.is_some() && !probing => {
                    probing = true;
                    let first = PoolAddr::parse(&self.cfg.pools[0]);
                    let params = self.login_params(&self.login_rx.borrow().clone());
                    let tls = self.tls.clone();
                    let tx = probe_tx.clone();
                    tokio::spawn(async move {
                        let ok = probe_login(&first, tls.as_ref(), &params).await;
                        let _ = tx.send(ok).await;
                    });
                }
                Some(ok) = probe_rx.recv() => {
                    probing = false;
                    if ok {
                        return End::Preferred;
                    }
                }
                line = lines.next() => {
                    let Some(Ok(line)) = line else {
                        return End::Disconnected;
                    };
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let id = msg.get("id").and_then(Value::as_u64);
                    if let Some(p) = id.and_then(|id| pending.remove(&id)) {
                        let error = msg.get("error").filter(|e| !e.is_null());
                        match p {
                            Pending::Login => {
                                let Some(sid) = login_session_id(&msg) else {
                                    if session.is_none() {
                                        return End::LoginFailed;
                                    }
                                    continue;
                                };
                                session = Some(sid.to_owned());
                                self.ready.store(true, Ordering::Relaxed);
                                self.logged_in = true;
                                self.publish(|s| {
                                    s.connected = true;
                                    s.unreachable = false;
                                });
                                tracing::info!(pool = %addr, "logged in to the stratum pool");
                                if let Some(job) = msg.get("result").and_then(|r| r.get("job")) {
                                    self.on_job(job, &mut valid);
                                }
                            }
                            Pending::Submit { reply, .. } => {
                                let outcome = match error {
                                    None => SubmitOutcome::Accepted,
                                    Some(e) => {
                                        let text = error_text(e);
                                        if text.to_lowercase().contains("unauthenticated") {
                                            // Lost the session: log in again on this connection.
                                            session = None;
                                            self.ready.store(false, Ordering::Relaxed);
                                            if out.send(request(next_id, "login", &login_params)).await.is_err() {
                                                return End::Disconnected;
                                            }
                                            pending.insert(next_id, Pending::Login);
                                            next_id += 1;
                                            login_deadline.as_mut().reset(tokio::time::Instant::now() + LOGIN_TIMEOUT);
                                        }
                                        if is_duplicate(&text) {
                                            SubmitOutcome::Duplicate
                                        } else {
                                            SubmitOutcome::rejected("pool", Some(text))
                                        }
                                    }
                                };
                                let _ = reply.send(outcome);
                            }
                            Pending::Keepalive => {}
                        }
                    } else if msg.get("method").and_then(Value::as_str) == Some("job")
                        && let Some(params) = msg.get("params")
                    {
                        self.on_job(params, &mut valid);
                    }
                }
                Some(SubmitCmd(s)) = self.cmd_rx.recv() => {
                    let Some(sid) = session.clone() else {
                        let _ = s.reply.send(SubmitOutcome::rejected("not_logged_in", None));
                        continue;
                    };
                    let issued = valid.iter().any(|(id, blob)| {
                        *id == s.share.job_id && s.blob.as_ref().is_none_or(|b| b == blob)
                    });
                    if !issued {
                        let _ = s.reply.send(SubmitOutcome::rejected("stale_job", None));
                        continue;
                    }
                    let params = json!({
                        "id": sid,
                        "job_id": s.share.job_id,
                        "nonce": s.share.nonce,
                        "result": s.share.result,
                    });
                    if out.send(request(next_id, "submit", &params)).await.is_err() {
                        let _ = s.reply.send(SubmitOutcome::rejected("socket_not_writable", None));
                        return End::Disconnected;
                    }
                    pending.insert(next_id, Pending::Submit { reply: s.reply, at: Instant::now() });
                    next_id += 1;
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    let expired: Vec<u64> = pending
                        .iter()
                        .filter(|(_, p)| matches!(p, Pending::Submit { at, .. } if now.duration_since(*at) > SUBMIT_TIMEOUT))
                        .map(|(id, _)| *id)
                        .collect();
                    for id in expired {
                        if let Some(Pending::Submit { reply, .. }) = pending.remove(&id) {
                            let _ = reply.send(SubmitOutcome::rejected("timeout", None));
                        }
                    }
                    if let Some(sid) = &session
                        && now.duration_since(last_keepalive) >= KEEPALIVE
                    {
                        last_keepalive = now;
                        if out.send(request(next_id, "keepalived", &json!({ "id": sid }))).await.is_err() {
                            return End::Disconnected;
                        }
                        pending.insert(next_id, Pending::Keepalive);
                        next_id += 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn addresses_and_errors() {
        assert_eq!(pool_addr("kalf.org"), "kalf.org:3333");
        assert_eq!(pool_addr("kalf.org:4444"), "kalf.org:4444");
        assert_eq!(pool_addr("stratum+tcp://h:1"), "h:1");
        assert_eq!(pool_addr("stratum+ssl://h:1"), "stratum+ssl://h:1");
        assert_eq!(pool_addr("Stratum+TLS://h"), "stratum+ssl://h:3333");
        assert_eq!(pool_addr("[::1]:5"), "[::1]:5");
        assert_eq!(pool_addr("[::1]"), "[::1]:3333");
        assert_eq!(
            PoolAddr::parse(" stratum+ssl://pool.example:4443 "),
            PoolAddr {
                host: "pool.example".into(),
                port: 4443,
                tls: true
            }
        );
        assert!(!PoolAddr::parse("pool.example:4443").tls);
        assert_eq!(PoolAddr::parse("h:notaport").host, "h:notaport");
        assert!(is_duplicate("Duplicate share"));
        assert!(!is_duplicate("duplicate job"));
        assert_eq!(
            error_text(&json!({ "code": -1, "message": "Low difficulty share" })),
            "Low difficulty share"
        );
        assert_eq!(error_text(&json!({ "code": -1 })), "-1");
        assert!(SubmitOutcome::rejected("pool", Some("Unauthenticated".into())).is_auth_reject());
        assert!(!SubmitOutcome::Accepted.is_auth_reject());
        let job = PoolJob::from_value(json!({
            "job_id": "1", "blob": format!("0500{}{}", "01", "22".repeat(32)), "target": "ffffffff"
        }))
        .unwrap();
        assert_eq!(job.prev_id, Some([0x22; 32]));
        assert!(
            PoolJob::from_value(json!({ "job_id": "1", "blob": "zz", "target": "ff" })).is_none()
        );
    }

    /// A minimal stratum pool: login with a job, accepts the first submit, rejects the second as
    /// a duplicate, answers "Unauthenticated" to the third (which makes the client log in again).
    async fn fake_pool(listener: TcpListener) {
        let (sock, _) = listener.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut lines = BufReader::new(r).lines();
        let mut submits = 0;
        let job = json!({ "job_id": "j1", "blob": format!("0500{}{}", "01", "33".repeat(32)), "target": "ffffffff" });
        while let Ok(Some(line)) = lines.next_line().await {
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["id"].clone();
            let resp = match req["method"].as_str().unwrap() {
                "login" => {
                    assert_eq!(req["params"]["login"], "SEKRpayout");
                    json!({ "id": id, "jsonrpc": "2.0", "error": null, "result": { "id": "sess", "job": job, "status": "OK" } })
                }
                "submit" => {
                    submits += 1;
                    assert_eq!(req["params"]["id"], "sess");
                    match submits {
                        1 => json!({ "id": id, "error": null, "result": { "status": "OK" } }),
                        2 => {
                            json!({ "id": id, "error": { "code": -1, "message": "Duplicate share" } })
                        }
                        _ => {
                            json!({ "id": id, "error": { "code": -1, "message": "Unauthenticated" } })
                        }
                    }
                }
                _ => json!({ "id": id, "error": null, "result": { "status": "KEEPALIVED" } }),
            };
            w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn stratum_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(fake_pool(listener));
        let shutdown = CancellationToken::new();
        let pool = StratumPool::spawn(
            StratumConfig {
                pools: vec![addr],
                login: "SEKRpayout".into(),
                pass: "x".into(),
                agent: "test".into(),
            },
            shutdown.clone(),
        );
        let mut jobs = pool.jobs();
        tokio::time::timeout(Duration::from_secs(5), jobs.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        assert!(pool.is_ready());
        let share = |job: &str| ShareSubmit {
            job_id: job.into(),
            nonce: "00000000".into(),
            result: "00".repeat(32),
        };
        assert_eq!(
            pool.submit(share("nope")).await,
            SubmitOutcome::rejected("stale_job", None)
        );
        assert_eq!(pool.submit(share("j1")).await, SubmitOutcome::Accepted);
        assert_eq!(pool.submit(share("j1")).await, SubmitOutcome::Duplicate);
        let third = pool.submit(share("j1")).await;
        assert!(third.is_auth_reject(), "{third:?}");
        // logged in again on the same connection
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pool.is_ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
    }

    /// `spawn_with` sends the rig id as `rigid` in the login parameters.
    #[tokio::test]
    async fn login_sends_the_rig_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (seen_tx, seen_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, _w) = sock.into_split();
            let mut lines = BufReader::new(r).lines();
            let line = lines.next_line().await.unwrap().unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let _ = seen_tx.send(req);
        });
        let shutdown = CancellationToken::new();
        let _pool = StratumPool::spawn_with(
            StratumConfig {
                pools: vec![addr],
                login: "SEKRpayout".into(),
                pass: "x".into(),
                agent: "test".into(),
            },
            LoginExtras {
                rig_id: Some("rig7".into()),
            },
            shutdown.clone(),
        );
        let req = tokio::time::timeout(Duration::from_secs(5), seen_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(req["method"], "login");
        assert_eq!(req["params"]["rigid"], "rig7");
        assert_eq!(req["params"]["login"], "SEKRpayout");
        shutdown.cancel();
    }

    /// A `stratum+ssl://` pool is spoken to with TLS: the first bytes on the wire are a TLS
    /// handshake record (`0x16`), and a server that answers in plain text never logs us in.
    #[tokio::test]
    async fn ssl_pools_speak_tls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut first = [0u8; 1];
            tokio::io::AsyncReadExt::read_exact(&mut sock, &mut first)
                .await
                .unwrap();
            let _ = seen_tx.send(first[0]);
            let _ = sock.write_all(b"{\"id\":1,\"result\":{}}\n").await;
        });
        let shutdown = CancellationToken::new();
        let pool = StratumPool::spawn(
            StratumConfig {
                pools: vec![format!("stratum+ssl://127.0.0.1:{port}")],
                login: "SEKRpayout".into(),
                pass: "x".into(),
                agent: "test".into(),
            },
            shutdown.clone(),
        );
        let first = tokio::time::timeout(Duration::from_secs(5), seen_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 0x16, "TLS handshake record");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pool.is_ready());
        assert!(pool.jobs().borrow().is_none());
        shutdown.cancel();
    }

    /// A stratum server that answers every `login` with a session and job `1`, every other
    /// request with OK, and reports `<tag> <login>` for each login.
    async fn login_server(
        listener: TcpListener,
        logins: mpsc::UnboundedSender<String>,
        tag: &'static str,
    ) {
        while let Ok((sock, _)) = listener.accept().await {
            let logins = logins.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["id"].clone();
                    let resp = if req["method"] == "login" {
                        let login = req["params"]["login"].as_str().unwrap_or_default();
                        let _ = logins.send(format!("{tag} {login}"));
                        json!({ "id": id, "error": null, "result": { "id": "s", "job": job_one(), "status": "OK" } })
                    } else {
                        json!({ "id": id, "error": null, "result": { "status": "OK" } })
                    };
                    if w.write_all(format!("{resp}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    fn blob_one() -> String {
        format!("0500{}{}", "01", "44".repeat(32))
    }

    fn job_one() -> Value {
        json!({ "job_id": "1", "blob": blob_one(), "target": "ffffffff" })
    }

    async fn wait_status(pool: &StratumPool, f: impl Fn(&PoolStatus) -> bool) {
        let mut rx = pool.status_changes();
        tokio::time::timeout(Duration::from_secs(20), rx.wait_for(|s| f(s)))
            .await
            .expect("status within 20 s")
            .expect("pool task alive");
    }

    /// The first pool is down: the client logs in to the second; once the first is up again
    /// it moves back. A new login reconnects with it; a share is only sent for the blob the
    /// current session issued under its job id.
    #[tokio::test]
    async fn fails_over_and_returns_to_the_first_pool() {
        let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = gone.local_addr().unwrap();
        drop(gone);
        let fallback = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fallback_addr = fallback.local_addr().unwrap().to_string();
        let (tx, mut logins) = mpsc::unbounded_channel();
        tokio::spawn(login_server(fallback, tx.clone(), "fallback"));
        let shutdown = CancellationToken::new();
        let pool = StratumPool::spawn_with_failover(
            StratumConfig {
                pools: vec![local.to_string(), fallback_addr.clone()],
                login: "SEKRfee+1000".into(),
                pass: "x".into(),
                agent: "test".into(),
            },
            LoginExtras::default(),
            Failover {
                switch_after: 1,
                return_to_first: Some(Duration::from_secs(1)),
                max_backoff: Duration::from_secs(4),
            },
            shutdown.clone(),
        );
        wait_status(&pool, |s| {
            s.connected && s.pool.as_deref() == Some(fallback_addr.as_str())
        })
        .await;
        assert_eq!(logins.recv().await.unwrap(), "fallback SEKRfee+1000");

        // the first pool comes up: the client moves back to it
        let first = TcpListener::bind(local).await.unwrap();
        tokio::spawn(login_server(first, tx.clone(), "local"));
        let local_text = local.to_string();
        wait_status(&pool, |s| {
            s.connected && s.pool.as_deref() == Some(local_text.as_str())
        })
        .await;
        assert!(!pool.status().unreachable);

        // a new login: a new connection with it
        pool.set_login("SEKRother+1000".into());
        assert_eq!(pool.login(), "SEKRother+1000");
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(l) = logins.recv().await {
                if l == "local SEKRother+1000" {
                    return;
                }
            }
        })
        .await
        .expect("login with the new address");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pool.is_ready() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let share = ShareSubmit {
            job_id: "1".into(),
            nonce: "00000000".into(),
            result: "00".repeat(32),
        };
        assert_eq!(
            pool.submit_for_blob(share.clone(), "ff").await,
            SubmitOutcome::rejected("stale_job", None)
        );
        assert_eq!(
            pool.submit_for_blob(share, &blob_one().to_uppercase())
                .await,
            SubmitOutcome::Accepted
        );
        shutdown.cancel();
    }

    /// Every pool down: the status says so after one round.
    #[tokio::test]
    async fn no_reachable_pool_is_reported() {
        let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = gone.local_addr().unwrap().to_string();
        drop(gone);
        let shutdown = CancellationToken::new();
        let pool = StratumPool::spawn_with_failover(
            StratumConfig {
                pools: vec![addr.clone()],
                login: "x".into(),
                pass: "x".into(),
                agent: "test".into(),
            },
            LoginExtras::default(),
            Failover {
                switch_after: 1,
                return_to_first: Some(Duration::from_secs(1)),
                max_backoff: Duration::from_secs(8),
            },
            shutdown.clone(),
        );
        wait_status(&pool, |s| s.unreachable && !s.connected).await;
        assert_eq!(pool.status().pool.as_deref(), Some(addr.as_str()));
        assert!(!pool.is_ready());
        shutdown.cancel();
    }
}
