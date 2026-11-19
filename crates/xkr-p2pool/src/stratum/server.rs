//! The listening server: accepting miners, bans, new jobs for everyone on a new template and
//! the status (`StratumServer` constructor, `on_block`, `on_blobs_ready`, `print_status`,
//! `api_update_local_stats`, `TCPServer::ban` / `is_banned`).

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand_core::RngCore as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use super::job::{JobFields, MAX_TARGET, job_line, template_target};
use super::session::{self, ClientState, Out};
use super::stats::{HashrateTracker, StratumStats};
use super::{ShareSink, StratumServerConfig, TemplateSource};
use crate::p2p::{BanKey, BanList};

/// Locks a mutex, taking over a poisoned one.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Seconds since the epoch (`seconds_since_epoch`).
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A random 32-bit value from the OS generator (`get_random32`).
pub(crate) fn random32() -> u32 {
    xkr_crypto::os_rng().next_u32()
}

/// A connected miner as the server sees it.
#[derive(Debug)]
pub(crate) struct Client {
    /// Its address.
    pub(crate) addr: SocketAddr,
    /// Its writer.
    pub(crate) out: mpsc::UnboundedSender<Out>,
    /// Its state.
    pub(crate) state: Mutex<ClientState>,
}

/// What the server and its connection tasks share.
pub(crate) struct Inner {
    pub(crate) config: StratumServerConfig,
    pub(crate) source: Arc<dyn TemplateSource>,
    pub(crate) sink: Arc<dyn ShareSink>,
    /// `m_extraNonce`.
    extra_nonce: AtomicU32,
    bans: Mutex<BanList>,
    clients: Mutex<BTreeMap<u64, Arc<Client>>>,
    next_client: AtomicU64,
    pub(crate) stats: Mutex<HashrateTracker>,
    /// One proof-of-work check at a time (`m_shareCheckInFlight`).
    pub(crate) pow: Semaphore,
    pub(crate) stop: watch::Sender<bool>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("config", &self.config)
            .field("clients", &lock(&self.clients).len())
            .finish_non_exhaustive()
    }
}

impl Inner {
    /// The next extra nonce for a login or `getjob` (`m_extraNonce.fetch_add(1)`).
    pub(crate) fn next_extra_nonce(&self) -> u32 {
        self.extra_nonce.fetch_add(1, Ordering::SeqCst)
    }

    /// Registers a connection; returns its key.
    pub(crate) fn add_client(&self, client: Arc<Client>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        lock(&self.clients).insert(id, client);
        id
    }

    /// Forgets a connection.
    pub(crate) fn remove_client(&self, id: u64) {
        lock(&self.clients).remove(&id);
    }

    /// Whether `ip` is banned now (never localhost).
    pub(crate) fn is_banned(&self, ip: IpAddr) -> bool {
        BanKey::ip(ip).is_some_and(|key| lock(&self.bans).is_banned(&key, Instant::now()))
    }

    /// Bans `ip` (IPv6 by its /64) for the ban time; localhost is never banned.
    pub(crate) fn ban(&self, ip: IpAddr) {
        if let Some(key) = BanKey::ip(ip) {
            lock(&self.bans).ban_for(key, Instant::now(), self.config.ban_time);
        }
    }

    /// `on_block` and `on_blobs_ready`: a new template. Every logged-in miner gets a job with
    /// its own extra nonce from a fresh random start; logins meanwhile take the values after
    /// those reserved for the open connections.
    fn on_block(&self) {
        let now = now_secs();
        let clients: Vec<Arc<Client>> = lock(&self.clients).values().cloned().collect();
        let n = u32::try_from(clients.len()).unwrap_or(u32::MAX);
        if n > 0 {
            let start = random32();
            self.extra_nonce
                .store(start.wrapping_add(n), Ordering::SeqCst);
            let blobs = self.source.hashing_blobs(start, n);
            if let Some(first) = blobs.first()
                && blobs.len() == clients.len()
            {
                let base = template_target(first.mainchain_difficulty, first.sidechain_difficulty)
                    .min(MAX_TARGET);
                let mut sent = 0usize;
                for client in &clients {
                    let Some(hb) = blobs.get(sent) else {
                        break;
                    };
                    let line = {
                        let mut st = lock(&client.state);
                        if !st.logged_in() {
                            continue;
                        }
                        st.decay_auto_diff(now, self.config.auto_diff);
                        let target = st.job_target(base, now, self.config.auto_diff);
                        let extra_nonce = start.wrapping_add(sent as u32);
                        let job_id = st.save_job(extra_nonce, hb.template_id, target);
                        job_line(&JobFields {
                            blob: &hb.blob,
                            job_id,
                            target,
                            height: hb.height,
                            seed_hash: &hb.seed_hash,
                        })
                    };
                    if client.out.send(Out::Line(line)).is_ok() {
                        sent += 1;
                    }
                }
            }
        }
        lock(&self.stats).update(0, now);
    }

    /// The status with the worker list.
    fn stats(&self) -> StratumStats {
        let now = now_secs();
        let side = self.source.sidechain_difficulty();
        let mut s = {
            let mut st = lock(&self.stats);
            st.update(0, now);
            st.snapshot(side)
        };
        let clients: Vec<Arc<Client>> = lock(&self.clients).values().cloned().collect();
        s.connections = u32::try_from(clients.len()).unwrap_or(u32::MAX);
        s.workers = clients
            .iter()
            .map(|c| lock(&c.state).worker(c.addr, now, side))
            .collect();
        s
    }
}

/// The stratum server: a listening socket and one task per miner. Dropping it stops
/// everything.
pub struct StratumServer {
    inner: Arc<Inner>,
    local_addr: SocketAddr,
    accept: JoinHandle<()>,
}

impl fmt::Debug for StratumServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StratumServer")
            .field("local_addr", &self.local_addr)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl StratumServer {
    /// Listens on `config.listen` and starts accepting miners (`StratumServer::StratumServer`,
    /// `start_listening`). The extra nonces start at a random value.
    pub async fn bind(
        config: StratumServerConfig,
        source: Arc<dyn TemplateSource>,
        sink: Arc<dyn ShareSink>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        let (stop, stop_rx) = watch::channel(false);
        let inner = Arc::new(Inner {
            config,
            source,
            sink,
            extra_nonce: AtomicU32::new(random32()),
            bans: Mutex::new(BanList::new()),
            clients: Mutex::new(BTreeMap::new()),
            next_client: AtomicU64::new(0),
            stats: Mutex::new(HashrateTracker::new(now_secs())),
            pow: Semaphore::new(1),
            stop,
        });
        let accept = tokio::spawn(accept_loop(inner.clone(), listener, stop_rx));
        Ok(Self {
            inner,
            local_addr,
            accept,
        })
    }

    /// The address it listens on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// A new block template: pushes a job to every logged-in miner (`on_block`). Call it after
    /// each template change.
    pub fn on_block(&self) {
        self.inner.on_block();
    }

    /// The status for the console, `/status` and logs.
    #[must_use]
    pub fn stats(&self) -> StratumStats {
        self.inner.stats()
    }

    /// Clears the share counters (`reset_share_counters`), e.g. when the payout wallet changes.
    pub fn reset_share_counters(&self) {
        for c in lock(&self.inner.clients).values() {
            lock(&c.state).reset_share_counters();
        }
        lock(&self.inner.stats).reset_share_counters();
    }

    /// Bans `ip` for the ban time (localhost is never banned).
    pub fn ban(&self, ip: IpAddr) {
        self.inner.ban(ip);
    }

    /// Whether `ip` is banned now.
    #[must_use]
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        self.inner.is_banned(ip)
    }

    /// Open connections.
    #[must_use]
    pub fn connections(&self) -> usize {
        lock(&self.inner.clients).len()
    }

    /// A handle that serves miners on other streams than the listening socket
    /// ([`StratumHandle::serve_stream`]).
    #[must_use]
    pub fn handle(&self) -> StratumHandle {
        StratumHandle {
            inner: self.inner.clone(),
        }
    }

    /// Stops accepting and closes every connection.
    pub fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        self.accept.abort();
    }
}

impl Drop for StratumServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// How a miner connection served by [`StratumHandle::serve_stream`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamEnd {
    /// The miner or the server closed it.
    Closed,
    /// The miner broke the rules (a bad line, no login in time, too many bad shares): the
    /// caller refuses it for the ban time, as the server bans a TCP miner's address.
    Banned,
}

/// Serves miners of a [`StratumServer`] on any byte stream, with the same line protocol, checks
/// and limits as on its TCP socket. Cheap to clone; sessions end when the server stops.
#[derive(Clone)]
pub struct StratumHandle {
    inner: Arc<Inner>,
}

impl fmt::Debug for StratumHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StratumHandle")
            .field("inner", &self.inner)
            .finish()
    }
}

impl StratumHandle {
    /// One miner on `stream` until it ends. `addr` is the miner's address when known: it is
    /// checked against the bans and banned when the miner breaks the rules; without one the
    /// worker list shows `0.0.0.0:0` and the caller bans ([`StreamEnd::Banned`]).
    pub async fn serve_stream<S>(&self, stream: S, addr: Option<SocketAddr>) -> StreamEnd
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        if *self.inner.stop.borrow() {
            return StreamEnd::Closed;
        }
        if let Some(a) = addr
            && self.inner.is_banned(a.ip())
        {
            return StreamEnd::Banned;
        }
        let shown = addr.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
        match session::run(self.inner.clone(), stream, shown, addr.is_some()).await {
            session::End::Ban => StreamEnd::Banned,
            session::End::Close => StreamEnd::Closed,
        }
    }

    /// Whether the server has stopped.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.inner.stop.borrow()
    }
}

/// Accepts miners until stopped; banned addresses are closed at once.
async fn accept_loop(inner: Arc<Inner>, listener: TcpListener, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            r = listener.accept() => match r {
                Ok((stream, addr)) => {
                    if inner.is_banned(addr.ip()) {
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    tokio::spawn(session::run(inner.clone(), stream, addr, true));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            },
        }
    }
}
