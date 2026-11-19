//! Jobs and share submits through p2pool (`docs/hugin-relay.md` §11).
//!
//! The relay in xkrd is a stratum client of a p2pool node, like a miner: it logs in with the
//! node operator's fee address and a fixed share difficulty (`<address>+<difficulty>`,
//! [`login_difficulty`]: p2pool's least, [`P2POOL_MIN_DIFF`], or the app difficulty when that is
//! higher), hands p2pool's jobs to the Hugin apps with the easier app target, and submits the
//! shares of accepted messages that also meet p2pool's target (see [`crate::relay`]). The local
//! p2pool ([`LOCAL_P2POOL`]) comes first (on Linux only when its port is not held by another
//! user, [`default_nodes_checked`]); while it cannot be reached the public nodes
//! ([`PUBLIC_P2POOL_NODES`]) are tried in order, and the client moves back to the local one as
//! soon as it accepts a login again ([`RETURN_TO_LOCAL`]). With no fee address the relay still
//! takes jobs, but submits nothing (it checks every share itself either way).
//!
//! Jobs reach the apps only while the node is synchronized (a `bool` watch channel from xkrd,
//! like the built-in miner pauses): until then `job_request` is answered `job_pending`. A second
//! watch channel carries the node's main-chain tip ([`ChainTip`]), with which the relay dates
//! every job it hands out.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::pool::{
    ChainTip, Failover, JobSource, LoginExtras, PoolConnector, PoolJob, PoolStatus, ShareSubmit,
    StratumConfig, StratumPool, SubmitOutcome, VALID_JOBS,
};

/// The local p2pool's stratum port (p2pool's default `--stratum`).
pub const LOCAL_P2POOL: &str = "127.0.0.1:3333";
/// The port of [`LOCAL_P2POOL`].
pub const LOCAL_P2POOL_PORT: u16 = 3333;

/// Public p2pool stratum nodes, tried in this order when the local p2pool cannot be reached
/// (`host:port`, or `stratum+ssl://host:port` for TLS). Empty for now: the hosts are filled in
/// here once they are chosen. `--hugin-p2pool` replaces the whole list, the local entry
/// included.
pub const PUBLIC_P2POOL_NODES: &[&str] = &[];

/// p2pool's least fixed share difficulty (its `MIN_DIFF`): the `+N` of the login unless the
/// app difficulty is higher.
pub const P2POOL_MIN_DIFF: u64 = 1000;

/// While a public node is in use, how often the local p2pool is tried again.
pub const RETURN_TO_LOCAL: Duration = Duration::from_secs(30);

/// Longest wait between two rounds over all p2pool nodes without a login.
pub const MAX_BACKOFF: Duration = Duration::from_mins(1);

/// The login user without a fee address.
pub const NO_FEE_USER: &str = "x";

/// The default node list: [`LOCAL_P2POOL`], then [`PUBLIC_P2POOL_NODES`].
#[must_use]
pub fn default_nodes() -> Vec<String> {
    std::iter::once(LOCAL_P2POOL)
        .chain(PUBLIC_P2POOL_NODES.iter().copied())
        .map(str::to_owned)
        .collect()
}

/// Who holds the local p2pool's port ([`check_local_p2pool`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPortOwner {
    /// Only listening sockets of our own user.
    Ours,
    /// Nothing listens on it now.
    Free,
    /// A listening socket of another user (its uid).
    Other(u32),
    /// Not known (not Linux, or `/proc` not readable).
    Unknown,
}

/// The uids of the sockets in state LISTEN on local port `port` in a `/proc/net/tcp` or
/// `/proc/net/tcp6` table (a header line, then `sl local_address rem_address st ... uid ...`,
/// addresses as `<hex address>:<hex port>`, LISTEN is state `0A`). Lines of another shape are
/// skipped.
#[must_use]
pub fn listening_uids(table: &str, port: u16) -> Vec<u32> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (_, local_port) = fields.get(1)?.rsplit_once(':')?;
            let listening = u16::from_str_radix(local_port, 16).ok()? == port
                && fields.get(3)?.eq_ignore_ascii_case("0A");
            if !listening {
                return None;
            }
            fields.get(7)?.parse().ok()
        })
        .collect()
}

/// The effective uid in a `/proc/<pid>/status` text (the second number of its `Uid:` line).
#[must_use]
pub fn status_uid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Who holds `port`, from the `/proc/net/tcp` and `/proc/net/tcp6` tables and our own uid:
/// any listening socket of another user makes it [`LocalPortOwner::Other`].
#[must_use]
pub fn port_owner(tcp: &str, tcp_v6: &str, own_uid: u32, port: u16) -> LocalPortOwner {
    let uids: Vec<u32> = listening_uids(tcp, port)
        .into_iter()
        .chain(listening_uids(tcp_v6, port))
        .collect();
    match uids.iter().find(|&&u| u != own_uid) {
        Some(&other) => LocalPortOwner::Other(other),
        None if uids.is_empty() => LocalPortOwner::Free,
        None => LocalPortOwner::Ours,
    }
}

/// Who holds [`LOCAL_P2POOL_PORT`] on this machine now ([`port_owner`] on `/proc`; Linux only).
#[must_use]
pub fn check_local_p2pool() -> LocalPortOwner {
    if !cfg!(target_os = "linux") {
        return LocalPortOwner::Unknown;
    }
    let read = |p: &str| std::fs::read_to_string(p).ok();
    let Some(uid) = read("/proc/self/status").as_deref().and_then(status_uid) else {
        return LocalPortOwner::Unknown;
    };
    let (tcp, tcp_v6) = (read("/proc/net/tcp"), read("/proc/net/tcp6"));
    if tcp.is_none() && tcp_v6.is_none() {
        return LocalPortOwner::Unknown;
    }
    port_owner(
        tcp.as_deref().unwrap_or_default(),
        tcp_v6.as_deref().unwrap_or_default(),
        uid,
        LOCAL_P2POOL_PORT,
    )
}

/// [`default_nodes`] without [`LOCAL_P2POOL`] when another user holds its port
/// ([`check_local_p2pool`]), and the owner found.
#[must_use]
pub fn default_nodes_checked() -> (Vec<String>, LocalPortOwner) {
    let owner = check_local_p2pool();
    let mut nodes = default_nodes();
    if matches!(owner, LocalPortOwner::Other(_)) {
        nodes.retain(|n| n != LOCAL_P2POOL);
    }
    (nodes, owner)
}

/// The fixed share difficulty asked from p2pool for an app difficulty: [`P2POOL_MIN_DIFF`], or
/// the app difficulty when it is higher (then every app share is a p2pool share).
#[must_use]
pub fn login_difficulty(app_difficulty: u64) -> u64 {
    app_difficulty.max(P2POOL_MIN_DIFF)
}

/// The stratum login: `<fee address>+<difficulty>` (p2pool reads the part before `+` as the
/// user name and the number after it as a fixed share difficulty), or `x+<difficulty>` without
/// a fee address.
#[must_use]
pub fn p2pool_login(fee_address: Option<&str>, difficulty: u64) -> String {
    format!(
        "{}+{}",
        fee_address.unwrap_or(NO_FEE_USER),
        difficulty.max(1)
    )
}

/// Settings of [`P2poolSource`].
#[derive(Debug, Clone)]
pub struct P2poolConfig {
    /// Stratum addresses in order of preference (the first is returned to).
    pub nodes: Vec<String>,
    /// The fee address (login); `None`: no submits.
    pub fee_address: Option<String>,
    /// The relay's app difficulty (`RelayConfig::app_difficulty`); the login asks for
    /// [`login_difficulty`] of it.
    pub app_difficulty: u64,
    /// Stratum `agent`.
    pub agent: String,
    /// Opens the entries of `nodes` that are not reached over TCP (xkrd without the node: the
    /// stratum service of remote nodes over Hyperswarm).
    pub connector: Option<Arc<dyn PoolConnector>>,
}

/// The relay's job source on p2pool.
#[derive(Debug)]
pub struct P2poolSource {
    pool: Arc<StratumPool>,
    /// p2pool's jobs while the gate is open, else `None`.
    jobs: watch::Receiver<Option<Arc<PoolJob>>>,
    gate: watch::Receiver<bool>,
    /// The node's main-chain tip (`None`: unknown).
    tip: watch::Receiver<Option<ChainTip>>,
    submits: AtomicBool,
    /// The `+N` of the login ([`login_difficulty`]).
    difficulty: u64,
    /// `(job_id, lowercase blob)` of the last [`VALID_JOBS`] jobs handed out.
    recent: Arc<Mutex<VecDeque<(String, String)>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl P2poolSource {
    /// Starts the stratum client and the gate (needs a tokio runtime); both stop with
    /// `shutdown`. `synced` opens the gate; `tip` is the node's main-chain tip
    /// ([`JobSource::tip`]).
    #[must_use]
    pub fn spawn(
        cfg: P2poolConfig,
        synced: watch::Receiver<bool>,
        tip: watch::Receiver<Option<ChainTip>>,
        shutdown: &CancellationToken,
    ) -> Arc<Self> {
        let difficulty = login_difficulty(cfg.app_difficulty);
        let pool = StratumPool::spawn_with_connector(
            StratumConfig {
                pools: cfg.nodes,
                login: p2pool_login(cfg.fee_address.as_deref(), difficulty),
                pass: "x".into(),
                agent: cfg.agent,
            },
            LoginExtras::default(),
            Failover {
                switch_after: 1,
                return_to_first: Some(RETURN_TO_LOCAL),
                max_backoff: MAX_BACKOFF,
            },
            cfg.connector,
            shutdown.clone(),
        );
        let (tx, jobs) = watch::channel(None);
        let recent = Arc::new(Mutex::new(VecDeque::new()));
        tokio::spawn(gate_jobs(
            pool.jobs(),
            synced.clone(),
            tx,
            recent.clone(),
            shutdown.clone(),
        ));
        Arc::new(Self {
            pool,
            jobs,
            gate: synced,
            tip,
            submits: AtomicBool::new(cfg.fee_address.is_some()),
            difficulty,
            recent,
        })
    }

    /// A new fee address (`None`: stop submitting): the client logs in again with it. Jobs of
    /// the previous connection are refused from then on (`stale_job`, the app gets the new
    /// job).
    pub fn set_fee_address(&self, fee_address: Option<&str>) {
        self.submits.store(fee_address.is_some(), Ordering::Relaxed);
        self.pool
            .set_login(p2pool_login(fee_address, self.difficulty));
    }

    /// The current stratum login.
    #[must_use]
    pub fn login(&self) -> String {
        self.pool.login()
    }

    /// Which p2pool node is in use and whether it is logged in.
    #[must_use]
    pub fn status(&self) -> PoolStatus {
        self.pool.status()
    }

    /// [`Self::status`] as a watch channel.
    #[must_use]
    pub fn status_changes(&self) -> watch::Receiver<PoolStatus> {
        self.pool.status_changes()
    }
}

/// Forwards p2pool's jobs while `gate` is `true` (else `None`) and remembers the jobs handed out.
async fn gate_jobs(
    mut pool_jobs: watch::Receiver<Option<Arc<PoolJob>>>,
    mut gate: watch::Receiver<bool>,
    tx: watch::Sender<Option<Arc<PoolJob>>>,
    recent: Arc<Mutex<VecDeque<(String, String)>>>,
    shutdown: CancellationToken,
) {
    let mut gate_alive = true;
    loop {
        let open = *gate.borrow_and_update();
        let job = pool_jobs.borrow_and_update().clone().filter(|_| open);
        if let Some(j) = &job {
            let entry = (j.job_id.clone(), j.blob.to_ascii_lowercase());
            let mut r = lock(&recent);
            if !r.contains(&entry) {
                r.push_back(entry);
                while r.len() > VALID_JOBS {
                    r.pop_front();
                }
            }
        }
        tx.send_if_modified(|current| {
            let same = match (current.as_ref(), job.as_ref()) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                current.clone_from(&job);
            }
            !same
        });
        tokio::select! {
            () = shutdown.cancelled() => return,
            r = pool_jobs.changed() => if r.is_err() { return },
            r = gate.changed(), if gate_alive => if r.is_err() { gate_alive = false },
        }
    }
}

impl JobSource for P2poolSource {
    fn jobs(&self) -> watch::Receiver<Option<Arc<PoolJob>>> {
        self.jobs.clone()
    }

    fn submit(&self, share: ShareSubmit) -> BoxFuture<'static, SubmitOutcome> {
        self.pool.submit(share)
    }

    fn submit_for(&self, share: ShareSubmit, blob: &[u8]) -> BoxFuture<'static, SubmitOutcome> {
        self.pool.submit_for_blob(share, &hex::encode(blob))
    }

    fn is_ready(&self) -> bool {
        self.pool.is_ready() && *self.gate.borrow()
    }

    fn submits_shares(&self) -> bool {
        self.submits.load(Ordering::Relaxed)
    }

    fn job_known(&self, job_id: &str, blob: &[u8]) -> bool {
        let blob = hex::encode(blob);
        lock(&self.recent)
            .iter()
            .any(|(id, b)| id == job_id && *b == blob)
    }

    fn tip(&self) -> Option<ChainTip> {
        *self.tip.borrow()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn logins_and_node_list() {
        assert_eq!(
            login_difficulty(crate::relay::DEFAULT_APP_DIFFICULTY),
            P2POOL_MIN_DIFF
        );
        assert_eq!(login_difficulty(1), 1000);
        assert_eq!(login_difficulty(1000), 1000);
        assert_eq!(login_difficulty(5000), 5000);
        assert_eq!(p2pool_login(Some("SEKRfee"), 1000), "SEKRfee+1000");
        assert_eq!(p2pool_login(None, 1000), "x+1000");
        assert_eq!(p2pool_login(None, 0), "x+1");
        let nodes = default_nodes();
        assert_eq!(nodes[0], LOCAL_P2POOL);
        assert_eq!(nodes.len(), 1 + PUBLIC_P2POOL_NODES.len());
    }

    /// The listening owner of the local p2pool's port from `/proc/net/tcp` samples.
    #[test]
    fn local_pool_owner_check() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0D05 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 31337 1 0000000000000000 100 0 0 10 0
   1: 0100007F:0D05 0100007F:B2C4 01 00000000:00000000 00:00000000 00000000  1001        0 0 4 0000000000000000 20 4 30 10 -1
   2: 00000000:2E7A 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0
   3: garbage
";
        let tcp_v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0D05 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1002        0 5151 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(listening_uids(tcp, 3333), [1000]);
        assert_eq!(listening_uids(tcp, 11898), [0]);
        assert_eq!(listening_uids(tcp_v6, LOCAL_P2POOL_PORT), [1002]);
        assert_eq!(port_owner(tcp, "", 1000, 3333), LocalPortOwner::Ours);
        assert_eq!(port_owner(tcp, "", 1001, 3333), LocalPortOwner::Other(1000));
        // an established connection of another user is no listener; a listener on :: counts
        assert_eq!(
            port_owner(tcp, tcp_v6, 1000, 3333),
            LocalPortOwner::Other(1002)
        );
        assert_eq!(port_owner(tcp, tcp_v6, 1000, 4444), LocalPortOwner::Free);
        assert_eq!(port_owner("", "", 1000, 3333), LocalPortOwner::Free);
        let status =
            "Name:\txkrd\nUmask:\t0022\nUid:\t1000\t1001\t1001\t1001\nGid:\t100\t100\t100\t100\n";
        assert_eq!(status_uid(status), Some(1001));
        assert_eq!(status_uid("Name: x"), None);
    }

    fn blob() -> String {
        format!("0500{}{}", "01", "55".repeat(32))
    }

    /// Answers every login with job `1` and reports the login.
    async fn server(listener: TcpListener, logins: mpsc::UnboundedSender<String>) {
        while let Ok((sock, _)) = listener.accept().await {
            let logins = logins.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    if req["method"] == "login" {
                        let _ = logins.send(req["params"]["login"].as_str().unwrap().to_owned());
                    }
                    let job = json!({ "job_id": "1", "blob": blob(), "target": "ffffffff" });
                    let resp = json!({ "id": req["id"], "error": null, "result": { "id": "s", "job": job, "status": "OK" } });
                    if w.write_all(format!("{resp}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    /// No job reaches the apps before the node is synchronized; then p2pool's job does, and is
    /// known; a new fee address logs in again with it, none stops the submits.
    #[tokio::test]
    async fn jobs_wait_for_sync_and_fee_address_changes_the_login() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, mut logins) = mpsc::unbounded_channel();
        tokio::spawn(server(listener, tx));
        let (synced_tx, synced) = watch::channel(false);
        let (tip_tx, tip) = watch::channel(None);
        let shutdown = CancellationToken::new();
        let src = P2poolSource::spawn(
            P2poolConfig {
                nodes: vec![addr.clone()],
                fee_address: Some("SEKRfee".into()),
                app_difficulty: crate::relay::DEFAULT_APP_DIFFICULTY,
                agent: "test".into(),
                connector: None,
            },
            synced,
            tip,
            &shutdown,
        );
        assert_eq!(logins.recv().await.unwrap(), "SEKRfee+1000");
        let mut status = src.status_changes();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(src.status().pool.as_deref(), Some(addr.as_str()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            src.jobs().borrow().is_none(),
            "no job before the node is synced"
        );
        assert!(!src.is_ready());
        assert!(src.submits_shares());
        assert_eq!(src.tip(), None);
        let tip = ChainTip {
            height: 7,
            hash: [7; 32],
        };
        tip_tx.send_replace(Some(tip));
        assert_eq!(src.tip(), Some(tip));

        synced_tx.send_replace(true);
        let mut jobs = src.jobs();
        tokio::time::timeout(Duration::from_secs(10), jobs.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        let raw = hex::decode(blob()).unwrap();
        assert!(src.job_known("1", &raw));
        assert!(!src.job_known("2", &raw));
        assert!(!src.job_known("1", &raw[1..]));
        assert!(src.is_ready());

        // behind again: the job goes away
        synced_tx.send_replace(false);
        tokio::time::timeout(Duration::from_secs(10), jobs.wait_for(Option::is_none))
            .await
            .unwrap()
            .unwrap();
        synced_tx.send_replace(true);

        src.set_fee_address(Some("SEKRnew"));
        assert_eq!(src.login(), "SEKRnew+1000");
        assert_eq!(logins.recv().await.unwrap(), "SEKRnew+1000");
        src.set_fee_address(None);
        assert!(!src.submits_shares());
        assert_eq!(logins.recv().await.unwrap(), "x+1000");
        shutdown.cancel();
    }
}
