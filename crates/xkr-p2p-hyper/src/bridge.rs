//! [`Bridge`]: the Rust end of the hyper-bridge frame protocol.
//!
//! A bridge has one link per Bare worker, each behind an in-memory channel
//! ([`Bridge::connect_streams`], [`crate::transport`]); [`Bridge::start_in_process`] starts the
//! workers and connects to them. Every link carries the same frames. Per link, a reader task
//! dispatches frames by `request_id` / `conn_id`, a writer task batches outgoing frames, and a
//! keepalive task pings after 10 s of silence and declares the link dead after 10 s more (this
//! is how a stuck worker is noticed).
//!
//! Topics are routed to workers (`docs/hyper-transport.md`): a topic announced with Hugin `mid`
//! material goes to the last worker, every other topic to worker 0, unless
//! [`Bridge::assign_topic`] says otherwise. xkrd runs one worker, so every topic shares its DHT
//! node and UDP port. Connections carry their worker in `conn_id`, so each [`HyperConn`] talks
//! to its own link. Each worker reports its DHT node in `DhtStatus` frames
//! ([`Bridge::dht_status`]).
//!
//! Failure is terminal for the whole bridge: when any link dies, every link is failed,
//! [`Incoming`] yields one `Err` and ends, every pending request fails, every [`HyperConn`]
//! returns an error and [`Bridge::wait_closed`] resolves. Reconnection is the caller's job.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::conn::{CloseInfo, CloseReason, ConnInfo, ConnShared, DataOutcome, HyperConn};
use crate::frame::{
    self, Ack, Announce, Body, Close, ConnId, DhtStatus, ErrorFrame, Frame, FrameCodec, Hello,
    Lookup, Open, Opened, Tlv, Unannounce, error_code, find_tlv_array, flags, tlv_tag,
};

/// Most control frames (everything but `Data`) queued for the writer at once. `Data` is bounded by
/// the credit windows; control frames (Pongs to a worker's Pings, Close and Error replies) are
/// not, so a worker that sends many Pings while not reading could grow the queue without bound.
/// Past the cap control frames are dropped and the bridge fails (on the next keepalive tick), and
/// the supervisor restarts the workers.
pub const MAX_QUEUED_CONTROL_FRAMES: usize = 16_384;
/// Most inbound connections (`Opened` with `request_id 0`) waiting to be taken from
/// [`Incoming`]. Further ones are closed at once.
pub const MAX_PENDING_INCOMING: usize = 256;
/// Most peers kept from one [`Bridge::lookup`] when the caller passes `max = 0` (no limit);
/// with a limit, at most `max` peers are kept whatever the worker sends.
pub const MAX_LOOKUP_PEERS: usize = 4096;

/// Errors of the bridge and of requests made through it. `Clone` so one failure can be
/// delivered to every waiter.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    #[error("i/o error: {0}")]
    Io(String),
    #[error("frame error: {0}")]
    Frame(String),
    #[error("cannot start the bridge: {0}")]
    Spawn(String),
    #[error("incompatible bridge protocol: ours {ours}, theirs {theirs}")]
    Version { ours: u16, theirs: u16 },
    #[error("bridge protocol violation: {0}")]
    Protocol(String),
    #[error("bridge keepalive timed out")]
    KeepaliveTimeout,
    #[error("request timed out")]
    Timeout,
    #[error("bridge error {code}: {message}")]
    Remote { code: u16, message: String },
    #[error("connection closed: {0}")]
    ConnClosed(CloseInfo),
    #[error("bridge closed: {0}")]
    Closed(String),
}

impl From<std::io::Error> for BridgeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<frame::FrameError> for BridgeError {
    fn from(e: frame::FrameError) -> Self {
        Self::Frame(e.to_string())
    }
}

/// Hugin `mid` material for [`Bridge::announce`] / [`Bridge::open_hugin`] (TLVs `0x10..0x12`).
///
/// Normally only `topic_key` is set (see [`crate::topic::HuginTopic::mid`]): the worker then
/// derives the topic base key pair, gives the topic its own DHT identity, signs that identity's
/// public key as the `mid`, and verifies the `mid` of every peer on that identity.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct HuginMid {
    /// Seed of the topic base key pair (`Buffer.alloc(32).fill(key)` in Hugin).
    pub topic_key: Option<[u8; 32]>,
    /// A precomputed `mid` (only meaningful together with `identity_seed`); remote `mid`s are
    /// not verified when `topic_key` is absent.
    pub mid: Option<[u8; 64]>,
    /// Seed of the DHT key pair to use for this topic (default: derived by the worker from its
    /// node key and the topic, stable across restarts with a key pair file).
    pub identity_seed: Option<[u8; 32]>,
}

impl fmt::Debug for HuginMid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HuginMid")
            .field("topic_key", &self.topic_key.is_some())
            .field("mid", &self.mid.is_some())
            .field("identity_seed", &self.identity_seed.is_some())
            .finish()
    }
}

impl HuginMid {
    #[must_use]
    pub fn from_topic_key(topic_key: [u8; 32]) -> Self {
        Self {
            topic_key: Some(topic_key),
            mid: None,
            identity_seed: None,
        }
    }

    fn tlvs(&self) -> Vec<Tlv> {
        let mut out = Vec::new();
        if let Some(m) = self.mid {
            out.push(Tlv::new(tlv_tag::HUGIN_MID, m.to_vec()));
        }
        if let Some(k) = self.topic_key {
            out.push(Tlv::new(tlv_tag::HUGIN_TOPIC_KEY, k.to_vec()));
        }
        if let Some(s) = self.identity_seed {
            out.push(Tlv::new(tlv_tag::IDENTITY_SEED, s.to_vec()));
        }
        out
    }
}

/// Announce mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceMode {
    /// Announce; accept inbound connections.
    Server = 1,
    /// Look up and auto-dial peers found on the topic (arrive as inbound `HyperConn`s with
    /// `initiator == true`).
    Client = 2,
    Both = 3,
}

/// `Open` flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenFlags(pub u32);

impl OpenFlags {
    pub const NONE: Self = Self(0);
    pub const ALLOW_RELAY: Self = Self(frame::open_flags::ALLOW_RELAY);
    pub const NO_HOLEPUNCH: Self = Self(frame::open_flags::NO_HOLEPUNCH);
}

impl std::ops::BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// One peer found by [`Bridge::lookup`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerAddr {
    pub public_key: [u8; 32],
    /// A DHT node the peer announced through (a handshake relay hint), not its own address.
    pub relay: Option<SocketAddr>,
}

impl fmt::Debug for PeerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerAddr")
            .field("public_key", &hex::encode(self.public_key))
            .field("relay", &self.relay)
            .finish()
    }
}

/// Parameters negotiated in the `Hello` exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionParams {
    pub minor: u16,
    pub max_frame: u32,
    pub initial_credit: u32,
    pub features: u64,
    pub workers: u16,
    pub worker: u16,
    /// The worker's main DHT public key (Hello TLV `0x01`).
    pub node_public_key: Option<[u8; 32]>,
}

/// How to start and talk to the Bare workers (see also [`crate::BareConfig`]).
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Directory containing `src/worker.js`. Default: see [`BridgeConfig::resolve_bridge_dir`].
    pub bridge_dir: Option<PathBuf>,
    /// Persist the DHT key pair here (worker 0; the other workers use files next to it, see
    /// [`crate::bare::worker_keypair_file`]).
    pub keypair_file: Option<PathBuf>,
    /// DHT bootstrap nodes as `host:port`; empty = hyperdht's public nodes.
    pub bootstrap: Vec<String>,
    /// Fixed UDP port of the DHT node: worker `i` binds `udp_port + i` and no other port (one
    /// worker, as in xkrd: exactly this port for every topic). `None`: a free port.
    pub udp_port: Option<u16>,
    /// Connection cap of worker 0's Hyperswarm for topics without Hugin `mid` (Hyperswarm's
    /// `maxPeers`, default 64). Explicit [`Bridge::open`]s are not counted by Hyperswarm, so the
    /// caller still enforces its own limit.
    pub max_peers: Option<u32>,
    /// JSON log lines from the workers.
    pub log_json: bool,
    /// Proposed `max_frame`.
    pub max_frame: u32,
    /// Proposed `initial_credit`.
    pub initial_credit: u32,
    /// Time allowed for starting the workers and for the `Hello` exchange.
    pub startup_timeout: Duration,
    /// Ping after this much silence.
    pub keepalive_idle: Duration,
    /// Declare the bridge dead if a Ping is unanswered for this long.
    pub keepalive_timeout: Duration,
    /// Timeout for `announce`, `unannounce` and `lookup`.
    pub request_timeout: Duration,
    /// Timeout for `open` (the worker itself gives up after 30 s).
    pub open_timeout: Duration,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bridge_dir: None,
            keypair_file: None,
            bootstrap: Vec::new(),
            udp_port: None,
            max_peers: None,
            log_json: false,
            max_frame: frame::DEFAULT_MAX_FRAME,
            initial_credit: frame::DEFAULT_INITIAL_CREDIT,
            startup_timeout: Duration::from_secs(20),
            keepalive_idle: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            open_timeout: Duration::from_secs(40),
        }
    }
}

/// Whether `dir`, its parent, `dir/src`, `dir/src/worker.js` and (where present)
/// `dir/src/session.js` belong to the owner of `exe` (or root) and nobody else can change them:
/// not writable by others, writable by the group only when it is the executable's group. A parent with the sticky bit (such as `/tmp`) may be
/// writable by others, since only the owner can rename or remove `dir` in it. Always true off
/// Unix.
#[must_use]
pub fn owned_like(dir: &Path, exe: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let Ok(owner) = std::fs::metadata(exe) else {
            return false;
        };
        let fine = |p: &Path, sticky_ok: bool| {
            std::fs::metadata(p).is_ok_and(|m| {
                let mode = m.mode();
                let by_owner = m.uid() == owner.uid() || m.uid() == 0;
                let others = mode & 0o002 == 0 || (sticky_ok && mode & 0o1000 != 0);
                let group = mode & 0o020 == 0 || m.gid() == owner.gid();
                by_owner && others && group
            })
        };
        let src = dir.join("src");
        let session = src.join("session.js");
        dir.parent().is_none_or(|p| fine(p, true))
            && fine(dir, false)
            && fine(&src, false)
            && fine(&src.join("worker.js"), false)
            && (!session.exists() || fine(&session, false))
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, exe);
        true
    }
}

impl BridgeConfig {
    /// Finds the bridge directory (one that contains `src/worker.js`), in this order:
    /// `bridge_dir`; `$XKR_BRIDGE_DIR`; `bridge/` next to the executable; `bridge/` in the
    /// executable directory's ancestors (`../../bridge` for `target/debug/xkrd`, deeper for
    /// examples and tests); finally this crate's source checkout (`../../bridge`).
    ///
    /// The two named directories are used as given. A directory found next to the executable,
    /// in its ancestors or in the source checkout is used only when [`owned_like`] holds for
    /// it and the executable.
    pub fn resolve_bridge_dir(&self) -> Result<PathBuf, BridgeError> {
        fn ok(p: &Path) -> bool {
            p.join("src").join("worker.js").is_file()
        }
        if let Some(d) = &self.bridge_dir {
            return if ok(d) {
                Ok(d.clone())
            } else {
                Err(BridgeError::Spawn(format!(
                    "no src/worker.js in {}",
                    d.display()
                )))
            };
        }
        if let Some(d) = std::env::var_os("XKR_BRIDGE_DIR")
            && ok(Path::new(&d))
        {
            return Ok(PathBuf::from(d));
        }
        let exe = std::env::current_exe().ok();
        let mut found = Vec::new();
        if let Some(dir) = exe.as_deref().and_then(Path::parent) {
            for anc in dir.ancestors().take(5) {
                found.push(anc.join("bridge"));
            }
        }
        found.push(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("bridge"),
        );
        let mut skipped = None;
        for c in found.into_iter().filter(|c| ok(c)) {
            if exe.as_deref().is_some_and(|e| owned_like(&c, e)) {
                return Ok(c);
            }
            tracing::warn!(
                dir = %c.display(),
                "bridge directory skipped: it or its parent is not owned like the executable, \
                 or writable by others (name it with --hyper-bridge-dir or XKR_BRIDGE_DIR to \
                 use it)"
            );
            skipped.get_or_insert(c);
        }
        Err(BridgeError::Spawn(match skipped {
            Some(c) => format!(
                "bridge directory {} not used (see the warning; set XKR_BRIDGE_DIR)",
                c.display()
            ),
            None => "bridge directory not found (set XKR_BRIDGE_DIR)".into(),
        }))
    }
}

// ---------------------------------------------------------------------------------------------
// shared state

enum Out {
    Frame(Frame),
    /// Flush what is queued, then close the socket.
    Shutdown,
}

enum Pending {
    Ack(oneshot::Sender<Result<Ack, BridgeError>>),
    Lookup {
        peers: Vec<PeerAddr>,
        /// Most peers kept ([`MAX_LOOKUP_PEERS`] for "no limit").
        max: usize,
        tx: oneshot::Sender<Result<Vec<PeerAddr>, BridgeError>>,
    },
}

impl Pending {
    fn fail(self, err: BridgeError) {
        match self {
            Pending::Ack(tx) => {
                let _ = tx.send(Err(err));
            }
            Pending::Lookup { tx, .. } => {
                let _ = tx.send(Err(err));
            }
        }
    }
}

type IncomingItem = Result<HyperConn, BridgeError>;

struct State {
    conns: HashMap<u64, Arc<ConnShared>>,
    pending: HashMap<u32, Pending>,
    pending_opens: HashMap<u64, oneshot::Sender<Result<HyperConn, BridgeError>>>,
    /// Ids we sent `Close` for without ever holding a `HyperConn` (timed-out opens).
    closing: HashSet<u64>,
    unknown_reported: HashSet<u64>,
    incoming_tx: Option<mpsc::UnboundedSender<IncomingItem>>,
    last_rx: Instant,
    ping: Option<(u64, Instant)>,
    error: Option<BridgeError>,
}

/// One link: the state of one worker.
pub(crate) struct Inner {
    out: mpsc::UnboundedSender<Out>,
    /// Worker index of this link: `header.shard` of every frame it sends.
    shard: u16,
    /// Every link of the bridge (this one included); a failure of one fails them all.
    siblings: OnceLock<Vec<Weak<Inner>>>,
    /// Control frames queued in `out` (see [`MAX_QUEUED_CONTROL_FRAMES`]).
    out_control: AtomicUsize,
    /// The control queue overflowed; the keepalive task fails the bridge.
    out_overflow: AtomicBool,
    /// Inbound connections queued for [`Incoming`] (see [`MAX_PENDING_INCOMING`]); shared by
    /// every link of the bridge.
    incoming_len: Arc<AtomicUsize>,
    state: Mutex<State>,
    pub(crate) params: SessionParams,
    config: BridgeConfig,
    next_request: AtomicU32,
    next_conn: AtomicU64,
    closed_tx: watch::Sender<Option<BridgeError>>,
    /// The worker's latest `DhtStatus` (`None` until the first one).
    dht_status: watch::Sender<Option<DhtStatus>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("shard", &self.shard)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Inner {
    pub(crate) fn send(&self, frame: Frame) -> Result<(), BridgeError> {
        if let Some(e) = &lock(&self.state).error {
            return Err(e.clone());
        }
        if !self.admit_out(&frame) {
            return Err(BridgeError::Closed("writer queue overflow".into()));
        }
        self.out
            .send(Out::Frame(self.stamp(frame)))
            .map_err(|_| BridgeError::Closed("writer stopped".into()))
    }

    /// Queues a frame without checking the bridge state. May be called with the state lock
    /// held, so it never calls [`Self::fail`] itself.
    fn send_raw(&self, frame: Frame) {
        if self.admit_out(&frame) {
            let _ = self.out.send(Out::Frame(self.stamp(frame)));
        }
    }

    /// Control frames name this link's worker in `header.shard`; connection frames already
    /// carry the worker of their `conn_id` ([`Frame::conn`]).
    fn stamp(&self, mut frame: Frame) -> Frame {
        if frame.conn_id == 0 {
            frame.shard = self.shard;
        }
        frame
    }

    /// Counts a control frame against [`MAX_QUEUED_CONTROL_FRAMES`]; false (frame dropped,
    /// overflow flagged) when the queue is full. `Data` is bounded by credits and always passes.
    fn admit_out(&self, frame: &Frame) -> bool {
        if matches!(frame.body, Body::Data(_)) {
            return true;
        }
        if self.out_control.fetch_add(1, Ordering::Relaxed) >= MAX_QUEUED_CONTROL_FRAMES {
            self.out_control.fetch_sub(1, Ordering::Relaxed);
            self.out_overflow.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn next_request_id(&self) -> u32 {
        loop {
            let id = self.next_request.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    /// Fails everything once; later calls are no-ops.
    // `err` is stored and cloned to every waiter, so taking it by value reads best at the callers.
    #[allow(clippy::needless_pass_by_value)]
    fn fail(&self, err: BridgeError) {
        let (pending, opens, conns, incoming) = {
            let mut st = lock(&self.state);
            if st.error.is_some() {
                return;
            }
            st.error = Some(err.clone());
            (
                std::mem::take(&mut st.pending),
                std::mem::take(&mut st.pending_opens),
                std::mem::take(&mut st.conns),
                st.incoming_tx.take(),
            )
        };
        tracing::warn!(error = %err, worker = self.shard, "hyper bridge failed");
        for p in pending.into_values() {
            p.fail(err.clone());
        }
        for tx in opens.into_values() {
            let _ = tx.send(Err(err.clone()));
        }
        for c in conns.into_values() {
            c.on_bridge_dead(&err);
        }
        if let Some(tx) = incoming {
            let _ = tx.send(Err(err.clone()));
        }
        let _ = self.out.send(Out::Shutdown);
        self.closed_tx.send_replace(Some(err.clone()));
        // The other workers of this bridge go down with it (each `fail` runs once).
        for sibling in self.siblings.get().into_iter().flatten() {
            if let Some(s) = sibling.upgrade() {
                s.fail(err.clone());
            }
        }
    }

    fn unknown_conn(&self, st: &mut State, conn_id: u64) {
        if st.unknown_reported.insert(conn_id) {
            if st.unknown_reported.len() > 4096 {
                st.unknown_reported.clear();
                st.unknown_reported.insert(conn_id);
            }
            self.send_raw(Frame {
                conn_id,
                ..Frame::control(Body::Error(ErrorFrame {
                    request_id: 0,
                    code: error_code::UNKNOWN_CONN,
                    message: "unknown conn_id".into(),
                }))
            });
        }
    }

    fn handle(self: &Arc<Self>, frame: Frame) {
        let conn_id = frame.conn_id;
        let ty = frame.body.frame_type();
        match frame.body {
            Body::Ping(n) => self.send_raw(Frame::control(Body::Pong(n))),
            Body::Pong(n) => {
                let mut st = lock(&self.state);
                if st.ping.is_some_and(|(nonce, _)| nonce == n) {
                    st.ping = None;
                }
            }
            Body::Opened(o) => self.on_opened(conn_id, &o),
            Body::Close(c) => self.on_close(conn_id, &c),
            Body::Data(d) => self.on_data(conn_id, d),
            Body::Credit(n) => {
                let shared = lock(&self.state).conns.get(&conn_id).cloned();
                if let Some(s) = shared {
                    s.on_credit(n);
                }
            }
            Body::LookupResult(r) => {
                let mut st = lock(&self.state);
                let done = frame.flags & flags::END != 0;
                if let Some(Pending::Lookup { peers, max, .. }) = st.pending.get_mut(&r.request_id)
                {
                    let room = max.saturating_sub(peers.len());
                    peers.extend(r.peers.iter().take(room).map(|p| PeerAddr {
                        public_key: p.public_key,
                        relay: p.addr.to_socket_addr(),
                    }));
                    if done
                        && let Some(Pending::Lookup { peers, tx, .. }) =
                            st.pending.remove(&r.request_id)
                    {
                        let _ = tx.send(Ok(peers));
                    }
                }
            }
            Body::Ack(a) => {
                let p = lock(&self.state).pending.remove(&a.request_id);
                match p {
                    Some(Pending::Ack(tx)) => {
                        let _ = tx.send(Ok(a));
                    }
                    Some(other) => other.fail(BridgeError::Protocol("Ack for a lookup".into())),
                    None => {}
                }
            }
            Body::PeerInfo(p) => {
                let shared = lock(&self.state).conns.get(&conn_id).cloned();
                if let Some(s) = shared {
                    s.on_peer_info(p);
                }
            }
            Body::DhtStatus(d) => {
                tracing::debug!(
                    worker = self.shard,
                    state = d.state,
                    firewalled = d.firewalled,
                    nat = d.nat,
                    udp_port = d.udp_port,
                    "DHT status"
                );
                self.dht_status.send_replace(Some(d));
            }
            Body::Error(e) => self.on_error(conn_id, e),
            Body::Unknown { frame_type, .. } => {
                if frame.flags & flags::IGNORABLE == 0 {
                    self.send_raw(Frame::control(Body::Error(ErrorFrame {
                        request_id: 0,
                        code: error_code::UNKNOWN_TYPE,
                        message: format!("unknown frame type {frame_type:#04x}"),
                    })));
                    self.fail(BridgeError::Protocol(format!(
                        "unknown frame type {frame_type:#04x}"
                    )));
                }
            }
            Body::Hello(_)
            | Body::Open(_)
            | Body::Announce(_)
            | Body::Unannounce(_)
            | Body::Lookup(_)
            | Body::PeerInfoRequest => {
                tracing::warn!(frame_type = ty, "unexpected frame from a Bare worker");
                self.send_raw(Frame {
                    conn_id,
                    ..Frame::control(Body::Error(ErrorFrame {
                        request_id: 0,
                        code: error_code::BAD_FRAME,
                        message: format!("unexpected frame type {ty:#04x}"),
                    }))
                });
            }
        }
    }

    fn on_opened(self: &Arc<Self>, conn_id: u64, o: &Opened) {
        let info = ConnInfo {
            conn_id,
            remote_public_key: o.remote_public_key,
            handshake_hash: o.handshake_hash,
            topic: (o.topic != [0u8; 32]).then_some(o.topic),
            initiator: o.initiator,
            addr: o.addr.to_socket_addr(),
            local_public_key: find_tlv_array(&o.tlvs, tlv_tag::PUBLIC_KEY),
        };
        let shared = Arc::new(ConnShared::new(info, self.params.initial_credit));
        let mut st = lock(&self.state);
        if ConnId(conn_id).from_bare() {
            if st.conns.contains_key(&conn_id) {
                drop(st);
                tracing::warn!(conn_id, "duplicate Opened");
                return;
            }
            st.conns.insert(conn_id, shared.clone());
            let conn = HyperConn::new(shared, self.clone());
            let delivered = match &st.incoming_tx {
                // A consumer that does not keep up gets no more than
                // `MAX_PENDING_INCOMING` connections queued; the rest are closed at once.
                Some(tx)
                    if self.incoming_len.fetch_add(1, Ordering::Relaxed) < MAX_PENDING_INCOMING =>
                {
                    tx.send(Ok(conn)).map_err(|e| {
                        self.incoming_len.fetch_sub(1, Ordering::Relaxed);
                        e.0
                    })
                }
                Some(_) => {
                    self.incoming_len.fetch_sub(1, Ordering::Relaxed);
                    tracing::debug!(conn_id, "too many inbound connections queued; closing");
                    Err(Ok(conn))
                }
                None => Err(Ok(conn)),
            };
            drop(st);
            // Dropping an undeliverable HyperConn sends Close.
            drop(delivered);
        } else if let Some(tx) = st.pending_opens.remove(&conn_id) {
            st.conns.insert(conn_id, shared.clone());
            drop(st);
            // If the opener is gone the HyperConn comes back and is dropped, which closes it.
            let _ = tx.send(Ok(HyperConn::new(shared, self.clone())));
        } else if st.closing.contains(&conn_id) {
            // Late Opened for an open we already cancelled; our Close is on its way.
        } else {
            drop(st);
            tracing::warn!(conn_id, "Opened for an id we never opened");
            self.send_raw(Frame::conn(
                conn_id,
                Body::Close(Close {
                    reason: frame::close_reason::PROTOCOL,
                    code: 0,
                    message: "unexpected Opened".into(),
                }),
            ));
        }
    }

    fn on_close(&self, conn_id: u64, c: &Close) {
        let info = CloseInfo::from_frame(c);
        let mut st = lock(&self.state);
        if let Some(shared) = st.conns.remove(&conn_id) {
            drop(st);
            let had_sent = shared.on_remote_close(info);
            if !had_sent {
                self.send_raw(Frame::conn(conn_id, Body::Close(normal_close())));
            }
        } else if let Some(tx) = st.pending_opens.remove(&conn_id) {
            drop(st);
            let _ = tx.send(Err(BridgeError::ConnClosed(info)));
            self.send_raw(Frame::conn(conn_id, Body::Close(normal_close())));
        } else if st.closing.remove(&conn_id) {
            // reply to our Close
        } else {
            self.unknown_conn(&mut st, conn_id);
        }
    }

    fn on_data(&self, conn_id: u64, payload: bytes::Bytes) {
        let mut st = lock(&self.state);
        let Some(shared) = st.conns.get(&conn_id).cloned() else {
            if !st.closing.contains(&conn_id) && !st.pending_opens.contains_key(&conn_id) {
                self.unknown_conn(&mut st, conn_id);
            }
            return;
        };
        drop(st);
        match shared.on_data(payload) {
            DataOutcome::Ok | DataOutcome::Ignored => {}
            DataOutcome::CreditExceeded => {
                let info = CloseInfo {
                    reason: CloseReason::Protocol,
                    code: frame::close_code::CREDIT_EXCEEDED,
                    message: "credit window exceeded".into(),
                };
                if shared.fail_local(info) {
                    self.send_raw(Frame {
                        conn_id,
                        ..Frame::control(Body::Error(ErrorFrame {
                            request_id: 0,
                            code: error_code::CREDIT_EXCEEDED,
                            message: "credit window exceeded".into(),
                        }))
                    });
                    self.send_raw(Frame::conn(
                        conn_id,
                        Body::Close(Close {
                            reason: frame::close_reason::PROTOCOL,
                            code: frame::close_code::CREDIT_EXCEEDED,
                            message: "credit window exceeded".into(),
                        }),
                    ));
                }
            }
        }
    }

    fn on_error(&self, conn_id: u64, mut e: ErrorFrame) {
        e.message = printable(&e.message);
        if e.request_id != 0 {
            let p = lock(&self.state).pending.remove(&e.request_id);
            if let Some(p) = p {
                p.fail(BridgeError::Remote {
                    code: e.code,
                    message: e.message,
                });
                return;
            }
        }
        match e.code {
            error_code::VERSION | error_code::UNKNOWN_TYPE => {
                self.fail(BridgeError::Remote {
                    code: e.code,
                    message: e.message,
                });
            }
            error_code::UNKNOWN_CONN => tracing::debug!(conn_id, "Bare worker: unknown conn_id"),
            _ => tracing::warn!(conn_id, code = e.code, message = %e.message, "Bare worker error"),
        }
    }
}

/// `text` with tabs and line breaks as spaces and every other control character left out, for
/// logs and error texts.
#[must_use]
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' | '\n' | '\r' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

fn normal_close() -> Close {
    Close {
        reason: frame::close_reason::NORMAL,
        code: 0,
        message: String::new(),
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// tasks

async fn writer_task<W: AsyncWrite + Unpin>(
    mut writer: FramedWrite<W, FrameCodec>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    inner: Weak<Inner>,
) {
    let mut taken_control = 0usize;
    let mut shutdown = false;
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < 256 {
            match rx.try_recv() {
                Ok(o) => batch.push(o),
                Err(_) => break,
            }
        }
        for o in batch {
            match o {
                Out::Frame(f) => {
                    if !matches!(f.body, Body::Data(_)) {
                        taken_control += 1;
                    }
                    if let Err(e) = writer.feed(f).await {
                        if let Some(i) = inner.upgrade() {
                            i.fail(e.into());
                        }
                        return;
                    }
                }
                Out::Shutdown => shutdown = true,
            }
        }
        if taken_control > 0 {
            if let Some(i) = inner.upgrade() {
                i.out_control.fetch_sub(taken_control, Ordering::Relaxed);
            }
            taken_control = 0;
        }
        if let Err(e) = writer.flush().await {
            if let Some(i) = inner.upgrade() {
                i.fail(e.into());
            }
            return;
        }
        if shutdown {
            let _ = writer.get_mut().shutdown().await;
            return;
        }
    }
}

async fn reader_task<R: AsyncRead + Unpin>(
    mut reader: FramedRead<R, FrameCodec>,
    inner: Weak<Inner>,
) {
    loop {
        let next = reader.next().await;
        let Some(inner) = inner.upgrade() else { return };
        match next {
            Some(Ok(frame)) => {
                lock(&inner.state).last_rx = Instant::now();
                inner.handle(frame);
            }
            Some(Err(e)) => {
                let err = BridgeError::from(e);
                inner.send_raw(Frame::control(Body::Error(ErrorFrame {
                    request_id: 0,
                    code: error_code::BAD_FRAME,
                    message: err.to_string(),
                })));
                inner.fail(err);
                return;
            }
            None => {
                inner.fail(BridgeError::Closed(format!(
                    "worker {} closed the connection",
                    inner.shard
                )));
                return;
            }
        }
    }
}

async fn keepalive_task(inner: Weak<Inner>, idle: Duration, timeout: Duration) {
    let mut tick = tokio::time::interval(Duration::from_secs(1).min(idle));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut counter = 0u64;
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        if inner.out_overflow.load(Ordering::Relaxed) {
            inner.fail(BridgeError::Protocol(
                "more than MAX_QUEUED_CONTROL_FRAMES control frames queued (worker not reading)"
                    .into(),
            ));
            return;
        }
        let action = {
            let mut st = lock(&inner.state);
            if st.error.is_some() {
                return;
            }
            let ping = st.ping;
            let silent_for = st.last_rx.elapsed();
            match ping {
                Some((_, at)) if at.elapsed() >= timeout => Some(None),
                None if silent_for >= idle => {
                    counter += 1;
                    let nanos = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_nanos() as u64);
                    let nonce = nanos ^ counter.rotate_left(32);
                    st.ping = Some((nonce, Instant::now()));
                    Some(Some(nonce))
                }
                // A ping is in flight but not timed out yet, or the link is not idle.
                Some(_) | None => None,
            }
        };
        match action {
            Some(Some(nonce)) => inner.send_raw(Frame::control(Body::Ping(nonce))),
            Some(None) => {
                inner.fail(BridgeError::KeepaliveTimeout);
                return;
            }
            None => {}
        }
    }
}

/// Waits for the peer's Hello, answering Pings meanwhile.
async fn read_hello<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut FramedRead<R, FrameCodec>,
    writer: &mut FramedWrite<W, FrameCodec>,
    shard: u16,
) -> Result<Hello, BridgeError> {
    loop {
        let Some(next) = reader.next().await else {
            return Err(BridgeError::Closed(format!(
                "worker {shard} closed during Hello"
            )));
        };
        let f = next?;
        match f.body {
            Body::Hello(h) => return Ok(h),
            Body::Ping(n) => {
                writer
                    .send(Frame {
                        shard,
                        ..Frame::control(Body::Pong(n))
                    })
                    .await?;
            }
            Body::Unknown { .. } if f.flags & flags::IGNORABLE != 0 => {}
            other => {
                return Err(BridgeError::Protocol(format!(
                    "expected Hello, got frame type {:#04x}",
                    other.frame_type()
                )));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// public API

/// Inbound connections (server side and Hyperswarm auto-dials) of every worker. Yields one
/// `Err` and ends when the bridge dies.
#[derive(Debug)]
pub struct Incoming {
    rx: mpsc::UnboundedReceiver<IncomingItem>,
    queued: Arc<AtomicUsize>,
    /// The `Err` was yielded: the stream has ended.
    done: bool,
}

impl Stream for Incoming {
    type Item = Result<HyperConn, BridgeError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let item = this.rx.poll_recv(cx);
        match &item {
            Poll::Ready(Some(Ok(_))) => {
                // Saturating: the final `Err` is not counted.
                let _ = this
                    .queued
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
            }
            // Every worker reports the failure; the first report ends the stream.
            Poll::Ready(Some(Err(_)) | None) => this.done = true,
            Poll::Pending => {}
        }
        item
    }
}

/// Most workers of one bridge (the worker index has 15 bits in `conn_id`).
pub const MAX_WORKERS: u16 = 0x7fff;

/// Where a link puts its inbound connections (one queue for every link of a bridge).
struct IncomingSink {
    tx: mpsc::UnboundedSender<IncomingItem>,
    len: Arc<AtomicUsize>,
}

/// One worker of a bridge.
#[derive(Debug, Clone)]
struct Link {
    inner: Arc<Inner>,
}

impl Link {
    /// `Hello` exchange with worker `worker` of `workers` over `stream`, then starts the reader,
    /// writer and keepalive tasks.
    async fn connect<S>(
        stream: S,
        config: BridgeConfig,
        worker: u16,
        workers: u16,
        sink: IncomingSink,
    ) -> Result<Link, BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = FramedRead::new(rd, FrameCodec::new(config.max_frame));
        let mut writer = FramedWrite::new(wr, FrameCodec::new(config.max_frame));

        writer
            .send(Frame {
                shard: worker,
                ..Frame::control(Body::Hello(Hello {
                    major: frame::VERSION_MAJOR,
                    minor: frame::VERSION_MINOR,
                    max_frame: config.max_frame,
                    initial_credit: config.initial_credit,
                    workers,
                    worker,
                    features: frame::features::HUGIN_MID,
                    tlvs: Vec::new(),
                }))
            })
            .await?;

        let hello = tokio::time::timeout(
            config.startup_timeout,
            read_hello(&mut reader, &mut writer, worker),
        )
        .await
        .map_err(|_| BridgeError::Timeout)??;

        if hello.major != frame::VERSION_MAJOR {
            let _ = writer
                .send(Frame {
                    shard: worker,
                    ..Frame::control(Body::Error(ErrorFrame {
                        request_id: 0,
                        code: error_code::VERSION,
                        message: format!("major {} != {}", hello.major, frame::VERSION_MAJOR),
                    }))
                })
                .await;
            return Err(BridgeError::Version {
                ours: frame::VERSION_MAJOR,
                theirs: hello.major,
            });
        }
        if hello.worker != worker || hello.workers != workers {
            return Err(BridgeError::Protocol(format!(
                "expected worker {worker} of {workers}, the bridge answered as worker {} of {}",
                hello.worker, hello.workers
            )));
        }
        // The session runs at min(ours, theirs) minor (docs/hyper-bridge-protocol.md section 5).
        #[expect(
            clippy::unnecessary_min_or_max,
            reason = "always VERSION_MINOR while that is 0 (v1.0); keep the negotiation for later minors"
        )]
        let minor = hello.minor.min(frame::VERSION_MINOR);
        let params = SessionParams {
            minor,
            max_frame: hello.max_frame.min(config.max_frame),
            initial_credit: hello.initial_credit.min(config.initial_credit),
            features: hello.features & frame::features::HUGIN_MID,
            workers: hello.workers,
            worker: hello.worker,
            node_public_key: find_tlv_array(&hello.tlvs, tlv_tag::PUBLIC_KEY),
        };
        if params.max_frame < frame::MIN_MAX_FRAME || params.initial_credit == 0 {
            return Err(BridgeError::Protocol(
                "max_frame or initial_credit too small".into(),
            ));
        }
        reader.decoder_mut().max_frame = params.max_frame;
        writer.encoder_mut().max_frame = params.max_frame;

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (closed_tx, _) = watch::channel(None);
        let (dht_status, _) = watch::channel(None);
        let idle = config.keepalive_idle;
        let ka_timeout = config.keepalive_timeout;
        let inner = Arc::new(Inner {
            out: out_tx,
            shard: worker,
            siblings: OnceLock::new(),
            out_control: AtomicUsize::new(0),
            out_overflow: AtomicBool::new(false),
            incoming_len: sink.len,
            state: Mutex::new(State {
                conns: HashMap::new(),
                pending: HashMap::new(),
                pending_opens: HashMap::new(),
                closing: HashSet::new(),
                unknown_reported: HashSet::new(),
                incoming_tx: Some(sink.tx),
                last_rx: Instant::now(),
                ping: None,
                error: None,
            }),
            params,
            config,
            next_request: AtomicU32::new(1),
            next_conn: AtomicU64::new(1),
            closed_tx,
            dht_status,
            tasks: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&inner);
        let tasks = vec![
            tokio::spawn(writer_task(writer, out_rx, weak.clone())),
            tokio::spawn(reader_task(reader, weak.clone())),
            tokio::spawn(keepalive_task(weak, idle, ka_timeout)),
        ];
        *lock(&inner.tasks) = tasks;
        tracing::info!(
            worker,
            workers,
            max_frame = params.max_frame,
            initial_credit = params.initial_credit,
            node = %params.node_public_key.map_or_default(hex::encode),
            "hyper bridge connected"
        );
        Ok(Link { inner })
    }

    fn error(&self) -> Option<BridgeError> {
        lock(&self.inner.state).error.clone()
    }

    async fn request_ack(&self, build: impl FnOnce(u32) -> Frame) -> Result<Ack, BridgeError> {
        let id = self.inner.next_request_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&self.inner.state);
            if let Some(e) = &st.error {
                return Err(e.clone());
            }
            st.pending.insert(id, Pending::Ack(tx));
        }
        self.inner.send(build(id))?;
        self.await_pending(id, rx).await
    }

    async fn await_pending<T>(
        &self,
        id: u32,
        rx: oneshot::Receiver<Result<T, BridgeError>>,
    ) -> Result<T, BridgeError> {
        match tokio::time::timeout(self.inner.config.request_timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(self
                .error()
                .unwrap_or(BridgeError::Closed("request dropped".into()))),
            Err(_) => {
                lock(&self.inner.state).pending.remove(&id);
                Err(BridgeError::Timeout)
            }
        }
    }

    async fn announce(
        &self,
        topic: [u8; 32],
        mode: AnnounceMode,
        hugin: Option<HuginMid>,
    ) -> Result<Ack, BridgeError> {
        let tlvs = hugin.as_ref().map_or_default(HuginMid::tlvs);
        self.request_ack(|request_id| {
            Frame::control(Body::Announce(Box::new(Announce {
                request_id,
                topic,
                mode: mode as u8,
                tlvs,
            })))
        })
        .await
    }

    async fn unannounce(&self, topic: [u8; 32]) -> Result<Ack, BridgeError> {
        self.request_ack(|request_id| {
            Frame::control(Body::Unannounce(Unannounce {
                request_id,
                topic,
                tlvs: Vec::new(),
            }))
        })
        .await
    }

    async fn lookup(&self, topic: [u8; 32], max: u16) -> Result<Vec<PeerAddr>, BridgeError> {
        let id = self.inner.next_request_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&self.inner.state);
            if let Some(e) = &st.error {
                return Err(e.clone());
            }
            st.pending.insert(
                id,
                Pending::Lookup {
                    peers: Vec::new(),
                    max: if max == 0 {
                        MAX_LOOKUP_PEERS
                    } else {
                        usize::from(max)
                    },
                    tx,
                },
            );
        }
        self.inner.send(Frame::control(Body::Lookup(Lookup {
            request_id: id,
            topic,
            max_peers: max,
            tlvs: Vec::new(),
        })))?;
        self.await_pending(id, rx).await
    }

    async fn open_inner(
        &self,
        remote_public_key: [u8; 32],
        topic: Option<[u8; 32]>,
        flags: OpenFlags,
        tlvs: Vec<Tlv>,
    ) -> Result<HyperConn, BridgeError> {
        let counter = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        let conn_id = ConnId::new(false, self.inner.shard, counter).0;
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&self.inner.state);
            if let Some(e) = &st.error {
                return Err(e.clone());
            }
            st.pending_opens.insert(conn_id, tx);
        }
        self.inner.send(Frame::conn(
            conn_id,
            Body::Open(Box::new(Open {
                request_id: counter as u32,
                remote_public_key,
                topic: topic.unwrap_or([0u8; 32]),
                open_flags: flags.0,
                tlvs,
            })),
        ))?;
        match tokio::time::timeout(self.inner.config.open_timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(self
                .error()
                .unwrap_or(BridgeError::Closed("open dropped".into()))),
            Err(_) => {
                let cancelled = {
                    let mut st = lock(&self.inner.state);
                    let c = st.pending_opens.remove(&conn_id).is_some();
                    if c {
                        st.closing.insert(conn_id);
                    }
                    c
                };
                if cancelled {
                    self.inner
                        .send_raw(Frame::conn(conn_id, Body::Close(normal_close())));
                }
                Err(BridgeError::Timeout)
            }
        }
    }
}

#[derive(Debug)]
struct Links {
    /// Link `i` talks to worker `i`.
    workers: Vec<Link>,
    /// Topics served by another worker than worker 0 (Hugin topics, [`Bridge::assign_topic`]).
    routes: Mutex<HashMap<[u8; 32], u16>>,
    incoming_rx: Mutex<Option<mpsc::UnboundedReceiver<IncomingItem>>>,
    incoming_len: Arc<AtomicUsize>,
}

/// Client of a bridge: N in-process Bare workers. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Bridge {
    links: Arc<Links>,
}

impl Bridge {
    /// Talks to one worker (worker 0 of 1) over any byte stream, for example a
    /// [`crate::transport::MemStream`].
    pub async fn connect_stream<S>(stream: S, config: BridgeConfig) -> Result<Bridge, BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        Bridge::connect_streams(vec![stream], config).await
    }

    /// Talks to `streams.len()` workers, `streams[i]` being worker `i` (1 to [`MAX_WORKERS`]).
    /// The `Hello` exchanges run at once; each worker must answer with its own index and the
    /// same worker count.
    pub async fn connect_streams<S>(
        streams: Vec<S>,
        config: BridgeConfig,
    ) -> Result<Bridge, BridgeError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let workers = u16::try_from(streams.len())
            .ok()
            .filter(|n| (1..=MAX_WORKERS).contains(n))
            .ok_or_else(|| {
                BridgeError::Protocol(format!(
                    "{} worker streams; 1 to {MAX_WORKERS} are possible",
                    streams.len()
                ))
            })?;
        let (tx, incoming_rx) = mpsc::unbounded_channel();
        let incoming_len = Arc::new(AtomicUsize::new(0));
        let configs = std::iter::repeat_n(config, usize::from(workers));
        let results =
            futures::future::join_all(streams.into_iter().zip(configs).zip(0..workers).map(
                |((stream, config), worker)| {
                    Link::connect(
                        stream,
                        config,
                        worker,
                        workers,
                        IncomingSink {
                            tx: tx.clone(),
                            len: incoming_len.clone(),
                        },
                    )
                },
            ))
            .await;
        drop(tx);
        // On an error the links that did connect are dropped here, which stops their tasks.
        let links = results.into_iter().collect::<Result<Vec<_>, _>>()?;
        let all: Vec<Weak<Inner>> = links.iter().map(|l| Arc::downgrade(&l.inner)).collect();
        for l in &links {
            let _ = l.inner.siblings.set(all.clone());
        }
        // A worker that failed between its Hello and now takes the others with it.
        if let Some(e) = links.iter().find_map(Link::error) {
            for l in &links {
                l.inner.fail(e.clone());
            }
        }
        Ok(Bridge {
            links: Arc::new(Links {
                workers: links,
                routes: Mutex::new(HashMap::new()),
                incoming_rx: Mutex::new(Some(incoming_rx)),
                incoming_len,
            }),
        })
    }

    fn first(&self) -> &Link {
        &self.links.workers[0]
    }

    /// The link of the worker that serves `topic`. Hugin topics without a route go to
    /// [`Bridge::hugin_worker`] and are remembered, so later opens and lookups on them use the
    /// worker that holds their identity.
    fn route(&self, topic: &[u8; 32], hugin: bool) -> &Link {
        let mut routes = lock(&self.links.routes);
        let worker = match routes.get(topic) {
            Some(&w) => w,
            None if hugin => {
                let w = self.hugin_worker();
                if w != 0 {
                    routes.insert(*topic, w);
                }
                w
            }
            None => 0,
        };
        drop(routes);
        &self.links.workers[usize::from(worker)]
    }

    /// Negotiated session parameters (of worker 0).
    #[must_use]
    pub fn params(&self) -> SessionParams {
        self.first().inner.params
    }

    /// Worker 0's main DHT public key: the node key, under which the XKR topics are announced.
    #[must_use]
    pub fn node_public_key(&self) -> Option<[u8; 32]> {
        self.first().inner.params.node_public_key
    }

    /// The main DHT public key of every worker, by worker index.
    #[must_use]
    pub fn worker_public_keys(&self) -> Vec<Option<[u8; 32]>> {
        self.links
            .workers
            .iter()
            .map(|l| l.inner.params.node_public_key)
            .collect()
    }

    /// The state of worker `worker`'s DHT node (`DhtStatus` frames): `None` inside the watch
    /// until the worker sent its first one. Worker 0 is the node's DHT (xkrd runs one worker).
    /// `None` if there is no such worker.
    #[must_use]
    pub fn dht_status(&self, worker: u16) -> Option<watch::Receiver<Option<DhtStatus>>> {
        self.links
            .workers
            .get(usize::from(worker))
            .map(|l| l.inner.dht_status.subscribe())
    }

    /// Number of workers.
    #[must_use]
    pub fn workers(&self) -> u16 {
        self.links.workers.len() as u16
    }

    /// The worker that serves Hugin topics: the last one (worker 0 when there is only one).
    #[must_use]
    pub fn hugin_worker(&self) -> u16 {
        self.workers() - 1
    }

    /// Routes `topic` to `worker` for every later announce, unannounce, lookup and open (a
    /// topic group of its own). Call it before the first announce of the topic. False if there
    /// is no such worker.
    #[must_use = "false means there is no such worker"]
    pub fn assign_topic(&self, topic: [u8; 32], worker: u16) -> bool {
        if worker >= self.workers() {
            return false;
        }
        lock(&self.links.routes).insert(topic, worker);
        true
    }

    /// The worker that serves `topic` (0 unless routed elsewhere).
    #[must_use]
    pub fn worker_for(&self, topic: &[u8; 32]) -> u16 {
        lock(&self.links.routes).get(topic).copied().unwrap_or(0)
    }

    /// The inbound connection stream of every worker. Only the first call returns `Some`;
    /// connections that arrive before it is taken are buffered.
    #[must_use]
    pub fn incoming(&self) -> Option<Incoming> {
        lock(&self.links.incoming_rx).take().map(|rx| Incoming {
            rx,
            queued: self.links.incoming_len.clone(),
            done: false,
        })
    }

    /// The error that ended the bridge, if it has ended.
    #[must_use]
    pub fn error(&self) -> Option<BridgeError> {
        self.links.workers.iter().find_map(Link::error)
    }

    /// Resolves when the bridge dies (keepalive timeout, socket or channel closed, protocol
    /// error, [`Bridge::close`]).
    pub async fn wait_closed(&self) -> BridgeError {
        // Every link fails with the first one, so watching worker 0 is enough.
        let mut rx = self.first().inner.closed_tx.subscribe();
        loop {
            if let Some(e) = rx.borrow_and_update().clone() {
                return e;
            }
            if rx.changed().await.is_err() {
                return BridgeError::Closed("bridge dropped".into());
            }
        }
    }

    /// Closes the bridge; every worker drops every connection and topic.
    pub fn close(&self) {
        self.first()
            .inner
            .fail(BridgeError::Closed("closed by caller".into()));
    }

    /// Joins `topic` (announce and/or look up). With `hugin`, the topic gets a Hugin identity
    /// whose handshakes carry and verify `mid`. Resolves when the topic is fully joined; the Ack
    /// carries the local public key used for it ([`Ack::local_public_key`]).
    pub async fn announce(
        &self,
        topic: [u8; 32],
        mode: AnnounceMode,
        hugin: Option<HuginMid>,
    ) -> Result<Ack, BridgeError> {
        self.route(&topic, hugin.is_some())
            .announce(topic, mode, hugin)
            .await
    }

    /// Leaves `topic`. Existing connections stay open.
    pub async fn unannounce(&self, topic: [u8; 32]) -> Result<Ack, BridgeError> {
        self.route(&topic, false).unannounce(topic).await
    }

    /// One DHT lookup of `topic`, up to `max` unique peers (0 = no limit).
    pub async fn lookup(&self, topic: [u8; 32], max: u16) -> Result<Vec<PeerAddr>, BridgeError> {
        self.route(&topic, false).lookup(topic, max).await
    }

    /// Dials `remote_public_key`. With a `topic` that has a Hugin identity (see
    /// [`Bridge::announce`]) the identity's key pair and `mid` are used.
    pub async fn open(
        &self,
        remote_public_key: [u8; 32],
        topic: Option<[u8; 32]>,
        flags: OpenFlags,
    ) -> Result<HyperConn, BridgeError> {
        let link = match &topic {
            Some(t) => self.route(t, false),
            None => self.first(),
        };
        link.open_inner(remote_public_key, topic, flags, Vec::new())
            .await
    }

    /// Dials a peer on a Hugin topic without announcing it first.
    pub async fn open_hugin(
        &self,
        remote_public_key: [u8; 32],
        topic: [u8; 32],
        flags: OpenFlags,
        hugin: &HuginMid,
    ) -> Result<HyperConn, BridgeError> {
        self.route(&topic, true)
            .open_inner(remote_public_key, Some(topic), flags, hugin.tlvs())
            .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn found_bridge_directories_must_be_owned_like_the_executable() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("xkrd");
        std::fs::write(&exe, b"").unwrap();
        let dir = tmp.path().join("bridge");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("worker.js"), b"").unwrap();
        let chmod = |p: &Path, mode: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        chmod(tmp.path(), 0o755);
        chmod(&dir, 0o755);
        assert!(owned_like(&dir, &exe));
        // Group-writable is fine for the executable's own group.
        chmod(&dir, 0o775);
        assert!(owned_like(&dir, &exe));
        chmod(&dir, 0o757);
        assert!(!owned_like(&dir, &exe));
        chmod(&dir, 0o755);
        chmod(&dir.join("src").join("worker.js"), 0o646);
        assert!(!owned_like(&dir, &exe));
        chmod(&dir.join("src").join("worker.js"), 0o644);
        // The shared session module counts once it exists.
        let session = dir.join("src").join("session.js");
        std::fs::write(&session, b"").unwrap();
        chmod(&session, 0o646);
        assert!(!owned_like(&dir, &exe));
        chmod(&session, 0o644);
        assert!(owned_like(&dir, &exe));
        // A parent everyone may write to needs the sticky bit.
        chmod(tmp.path(), 0o777);
        assert!(!owned_like(&dir, &exe));
        chmod(tmp.path(), 0o1777);
        assert!(owned_like(&dir, &exe));
        chmod(tmp.path(), 0o755);
        assert!(!owned_like(&dir, &tmp.path().join("missing")));
    }
}
