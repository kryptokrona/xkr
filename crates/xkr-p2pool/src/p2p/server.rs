//! The sidechain P2P server (`P2PServer`, `src/p2p_server.cpp`): connections, the peer list,
//! bans, syncing missing shares and broadcasting.
//!
//! One task owns all state (the C++ event loop). Each connection runs [`run_connection`] in
//! its own task and reports [`ConnEvent`]s; shares are parsed, checked and added to the
//! sidechain on a blocking thread (CN-Turtle and the PPLNS checks are CPU work) and the result
//! comes back to the owner task, which bans, asks for missing shares and broadcasts, like
//! `handle_incoming_block_async` / `post_handle_incoming_block`.
//!
//! Transports: TCP (outgoing by default, a listener only when [`P2pConfig::listen`] is set)
//! and already open Hyperswarm streams handed in with [`P2pServer::attach`]. Hyperswarm peers
//! use the fourth peer id, are banned by their public key, never appear in peer lists, and
//! get the same broadcasts as TCP peers, so shares cross between the two transports.
//!
//! The server drains the sidechain's events ([`SideChain::take_events`]) and forwards each
//! one to the node as [`P2pEvent::Sidechain`]. After the node changes the sidechain itself
//! (its own share, the block cache) it calls [`P2pServer::sidechain_changed`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};
use xkr_types::Hash;

use super::conn::{Command, ConnEnd, ConnEvent, TICK, run_connection};
use super::handshake::PeerIds;
use super::message::{HASH_SIZE, PeerEntry};
use super::peers::{
    BanKey, BanList, BroadcastThrottle, PeerBook, ReceivedPeers, VersionRecord, is_ipv4_prefix,
};
use super::session::{
    Direction, Domain, Event, MAX_PENDING_BLOCK_REQUESTS, PeerSession, SessionConfig,
};
use super::{P2pConfig, dialer, peerfile};
use crate::block::PoolBlock;
use crate::config::SidechainConfig;
use crate::mainchain::{ChainMain, MainChainView};
use crate::parse::{parse_compact_pool_block, parse_pool_block};
use crate::sidechain::{BlockRejected, ExternalOutcome, SideChain, SidechainEvent};

/// The sidechain as the server and the node share it.
pub type SharedSideChain = Arc<Mutex<SideChain>>;
/// The main chain as the server reads it (the node's header cache behind a lock).
pub type SharedMainChain = Arc<dyn MainChainView + Send + Sync>;
/// Seconds since the epoch (`seconds_since_epoch`); replaceable for tests.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A stream a peer connection can run on: a TCP socket, a Hyperswarm connection
/// (`xkr_p2p_hyper::HyperConn`) or an in-memory pipe.
pub trait PeerStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> PeerStream for T {}

/// Where a peer is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// A TCP peer at this address.
    Tcp(SocketAddr),
    /// A Hyperswarm peer with this DHT public key.
    Hyper([u8; 32]),
}

impl Transport {
    /// The peer id domain of the transport.
    #[must_use]
    pub const fn domain(&self) -> Domain {
        match self {
            Self::Tcp(_) => Domain::Tcp,
            Self::Hyper(_) => Domain::Hyper,
        }
    }

    /// What a ban of this peer applies to: the IP address (IPv6 by /64) or the public key.
    /// `None` for localhost, which is never banned.
    #[must_use]
    pub fn ban_key(&self) -> Option<BanKey> {
        match self {
            Self::Tcp(addr) => BanKey::ip(addr.ip()),
            Self::Hyper(key) => Some(BanKey::Key(*key)),
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "{addr}"),
            Self::Hyper(key) => write!(f, "hyper:{}", hex::encode(&key[..8])),
        }
    }
}

/// How often the peer file is written (`save_peer_list_async`).
pub const PEER_FILE_SAVE_INTERVAL: u64 = 300;
/// A peer whose connection failed this many times is dropped from the peer list
/// (`on_connect_failed`).
pub const MAX_FAILED_CONNECTIONS: u32 = 10;
/// Largest clock difference accepted on a broadcast share (`handle_incoming_block_async(…,
/// 1800)`).
pub const BROADCAST_MAX_TIME_DELTA: u64 = 1800;
/// A broadcast share this many main-chain blocks behind ours bans the peer.
pub const STALE_BROADCAST_BAN: u64 = 5;
/// A peer that has not broadcast for this long while our chain moved is banned
/// (`update_peer_connections`).
pub const SILENT_PEER_LIMIT: u64 = 900;
/// Commands waiting for one connection; a peer that lets more pile up is dropped.
const COMMAND_QUEUE: usize = 1024;
/// Events waiting for the server.
const EVENT_QUEUE: usize = 1024;
/// Our requests by id per connection; one slot of the session's limit stays free for the
/// chain tip request.
const REQUEST_LIMIT: usize = MAX_PENDING_BLOCK_REQUESTS - 1;

/// Locks a mutex, taking over a poisoned one.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Seconds since the epoch from the system clock.
#[must_use]
pub fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Settings of a [`P2pServer`].
#[derive(Clone)]
pub struct P2pServerConfig {
    /// Listen address and connection limits.
    pub p2p: P2pConfig,
    /// `p2pool_peers.txt`, read at start and written every 5 minutes and at stop; `None`
    /// keeps the peer list in memory only.
    pub peer_file: Option<PathBuf>,
    /// Seed entries (`host:port`), resolved at start and again while no peer is connected.
    pub seeds: Vec<String>,
    /// Peers to connect to at start (`--addpeers`).
    pub add_peers: Vec<String>,
    /// Port for entries without one.
    pub default_port: u16,
    /// Wall clock in seconds.
    pub clock: Clock,
}

impl fmt::Debug for P2pServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pServerConfig")
            .field("p2p", &self.p2p)
            .field("peer_file", &self.peer_file)
            .field("seeds", &self.seeds)
            .field("add_peers", &self.add_peers)
            .field("default_port", &self.default_port)
            .finish_non_exhaustive()
    }
}

impl P2pServerConfig {
    /// Settings without seeds, peer file or extra peers, on the system clock.
    #[must_use]
    pub fn new(p2p: P2pConfig) -> Self {
        Self {
            p2p,
            peer_file: None,
            seeds: Vec::new(),
            add_peers: Vec::new(),
            default_port: super::DEFAULT_P2P_PORT,
            clock: Arc::new(system_clock),
        }
    }

    /// The settings for a sidechain: its seeds ([`crate::seeds::seeds_for`]), its default port
    /// and `p2pool_peers.txt` in `data_dir` (the node passes `<data-dir>/p2pool`).
    #[must_use]
    pub fn for_sidechain(
        p2p: P2pConfig,
        sidechain: &SidechainConfig,
        data_dir: Option<&Path>,
    ) -> Self {
        Self {
            seeds: crate::seeds::seeds_for(sidechain)
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            default_port: sidechain.default_p2p_port(),
            peer_file: data_dir.map(|d| d.join(peerfile::PEER_FILE_NAME)),
            ..Self::new(p2p)
        }
    }
}

/// What the node learns from the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum P2pEvent {
    /// An event of the sidechain (new tip, store in cache, possible uncle, …). `Broadcast`
    /// events are already sent to the peers.
    Sidechain(SidechainEvent),
    /// A share from a peer was stored.
    ShareAdded {
        /// Its id.
        id: Hash,
        /// Its sidechain height.
        height: u64,
        /// The peer it came from, if still connected.
        from: Option<Transport>,
    },
    /// A share from a peer is a candidate for the next main-chain block and meets the
    /// main-chain difficulty: submit it as a block.
    SubmitToDaemon(Box<PoolBlock>),
    /// The main-chain block being watched for arrived as this share.
    BlockFound {
        /// The share id.
        id: Hash,
        /// The main-chain block.
        main: ChainMain,
    },
    /// A peer was disconnected and banned.
    PeerRejected {
        /// The peer.
        peer: Transport,
        /// Why.
        reason: String,
    },
    /// The sidechain has a tip and no shares are missing (sent once).
    Synchronized,
}

/// One connection as the stats show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerStatus {
    /// Where the peer is.
    pub transport: Transport,
    /// Who connected.
    pub direction: Direction,
    /// Handshake done and listen port known.
    pub good: bool,
    /// The peer's version record.
    pub software: Option<VersionRecord>,
    /// Highest share height the peer broadcast or announced.
    pub height: u64,
}

/// The server's numbers (console, `/status`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct P2pStats {
    /// Outgoing TCP connections.
    pub peers_out: usize,
    /// Incoming TCP connections.
    pub peers_in: usize,
    /// Hyperswarm connections.
    pub peers_hyper: usize,
    /// Connections with the handshake done and the listen port known.
    pub peers_good: usize,
    /// Outgoing TCP connections being opened.
    pub connecting: usize,
    /// Addresses in the peer list.
    pub known: usize,
    /// Bans held.
    pub banned: usize,
    /// The sidechain has a tip and no shares are missing.
    pub synced: bool,
    /// Every connection.
    pub peers: Vec<PeerStatus>,
}

/// What the owner task is told by the handle and its helper tasks.
enum Control {
    Attach {
        stream: Box<dyn PeerStream>,
        direction: Direction,
        key: [u8; 32],
    },
    Connect(SocketAddr),
    Dialed {
        addr: SocketAddr,
        result: io::Result<TcpStream>,
    },
    Accepted {
        stream: TcpStream,
        addr: SocketAddr,
    },
    Resolved {
        addrs: Vec<SocketAddr>,
        dial: bool,
    },
    Ended {
        conn: u64,
        end: ConnEnd,
    },
    SidechainChanged,
    Broadcast(Hash),
    Stop,
}

/// The sidechain P2P server. Dropping it stops the server (without the final peer file
/// write; use [`P2pServer::stop`] for that).
pub struct P2pServer {
    control: mpsc::UnboundedSender<Control>,
    stats: watch::Receiver<P2pStats>,
    local_addr: Option<SocketAddr>,
    task: Option<JoinHandle<()>>,
}

impl fmt::Debug for P2pServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("P2pServer")
            .field("local_addr", &self.local_addr)
            .field("stats", &*self.stats.borrow())
            .finish_non_exhaustive()
    }
}

impl P2pServer {
    /// Starts the server: binds the listener (if configured), loads the peer file, resolves
    /// the seeds and the extra peers, and returns the handle with the node's event channel.
    pub async fn start(
        cfg: P2pServerConfig,
        sidechain: SharedSideChain,
        main: SharedMainChain,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<P2pEvent>)> {
        let listener = match cfg.p2p.listen {
            Some(addr) => Some(TcpListener::bind(addr).await?),
            None => None,
        };
        let local_addr = listener.as_ref().map(TcpListener::local_addr).transpose()?;
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (conn_tx, conn_rx) = mpsc::channel(EVENT_QUEUE);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (stats_tx, stats_rx) = watch::channel(P2pStats::default());
        let (consensus_id, unlock_window, block_time) = {
            let sc = lock(&sidechain);
            (
                sc.consensus_id().0,
                sc.options().unlock_window,
                sc.config().block_time,
            )
        };
        let accept = listener.map(|l| tokio::spawn(accept_loop(l, control_tx.clone())));
        let now = (cfg.clock)();
        let mut server = Server {
            peer_ids: PeerIds::random(),
            consensus_id,
            unlock_window,
            sidechain,
            main,
            conns: HashMap::new(),
            next_conn: 1,
            book: PeerBook::new(),
            bans: BanList::new(),
            throttle: BroadcastThrottle::new(block_time),
            failures: HashMap::new(),
            dialing: HashSet::new(),
            missing_requests: HashSet::new(),
            notify_requests: HashSet::new(),
            look_for_missing: true,
            synced: false,
            timer: 0,
            started: Instant::now(),
            last_saved: now,
            control: control_tx.clone(),
            conn_events: conn_tx,
            events: events_tx,
            stats: stats_tx,
            accept,
            cfg,
        };
        server.load_peer_file();
        server.resolve(server.cfg.seeds.clone(), false);
        server.resolve(server.cfg.add_peers.clone(), true);
        server.publish_stats();
        let task = tokio::spawn(server.run(control_rx, conn_rx));
        Ok((
            Self {
                control: control_tx,
                stats: stats_rx,
                local_addr,
                task: Some(task),
            },
            events_rx,
        ))
    }

    /// The address the listener is bound to, when listening.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Runs the protocol on an already open Hyperswarm stream of the sidechain topic.
    /// `direction` is [`Direction::Outgoing`] when we opened it (the Hyperswarm initiator,
    /// which puts work into its handshake answer); `remote_key` is the peer's DHT public key,
    /// used for bans and for one connection per key.
    pub fn attach<S: PeerStream>(&self, stream: S, direction: Direction, remote_key: [u8; 32]) {
        let _ = self.control.send(Control::Attach {
            stream: Box::new(stream),
            direction,
            key: remote_key,
        });
    }

    /// Opens a TCP connection to `addr` (unless banned or already connected).
    pub fn connect(&self, addr: SocketAddr) {
        let _ = self.control.send(Control::Connect(addr));
    }

    /// Sends the stored share `id` to every peer (the node's own new share; shares from
    /// peers are relayed through the sidechain's broadcast events).
    pub fn broadcast(&self, id: Hash) {
        let _ = self.control.send(Control::Broadcast(id));
    }

    /// The node changed the sidechain: drain its events, broadcast what is due and forward
    /// them.
    pub fn sidechain_changed(&self) {
        let _ = self.control.send(Control::SidechainChanged);
    }

    /// The current numbers.
    #[must_use]
    pub fn stats(&self) -> P2pStats {
        self.stats.borrow().clone()
    }

    /// The numbers as a watch, updated on every change.
    #[must_use]
    pub fn subscribe_stats(&self) -> watch::Receiver<P2pStats> {
        self.stats.clone()
    }

    /// Closes every connection, writes the peer file and waits for the server task.
    pub async fn stop(mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for P2pServer {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
    }
}

/// Accepts TCP peers and hands them to the owner task.
async fn accept_loop(listener: TcpListener, control: mpsc::UnboundedSender<Control>) {
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let _ = stream.set_nodelay(true);
                if control.send(Control::Accepted { stream, addr }).is_err() {
                    return;
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// The address bytes used for "one connection per IP": IPv4 (also IPv4-mapped IPv6) as is,
/// IPv6 by its /64 prefix.
fn ip_key(ip: IpAddr) -> [u8; 16] {
    let mut k = PeerEntry::from_socket_addr(SocketAddr::new(ip, 0)).ip;
    if !is_ipv4_prefix(&k) {
        k[8..].fill(0);
    }
    k
}

/// One open connection.
#[derive(Debug)]
struct Conn {
    commands: mpsc::Sender<Command>,
    abort: AbortHandle,
    transport: Transport,
    direction: Direction,
    peer_id: Option<u64>,
    handshake: bool,
    listen_port: Option<u16>,
    software: Option<VersionRecord>,
    /// Our requests by id still open.
    pending: usize,
    broadcast_max_height: u64,
    last_broadcast: u64,
    last_block_request: u64,
}

impl Conn {
    fn good(&self) -> bool {
        self.handshake && self.listen_port.is_some()
    }

    fn is_tcp(&self, direction: Direction) -> bool {
        matches!(self.transport, Transport::Tcp(_)) && self.direction == direction
    }
}

/// What a received share answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Our request for the peer's chain tip.
    Tip,
    /// Our request for this id.
    Requested(Hash),
    /// A push from the peer.
    Broadcast { compact: bool },
}

/// The result of [`process_share`].
#[derive(Debug)]
enum Processed {
    /// A rule was broken.
    Ban(String),
    /// The share was read.
    Read {
        id: Hash,
        height: u64,
        /// `None`: not offered to the sidechain (seen before, stale, clock difference).
        result: Option<Result<ExternalOutcome, BlockRejected>>,
        /// The share, when it went to the sidechain (for a daemon submit).
        block: Option<Box<PoolBlock>>,
        events: Vec<SidechainEvent>,
    },
}

/// Parses and checks a share from a peer and offers it to the sidechain
/// (`on_block_response`, `on_block_broadcast`, `handle_incoming_block_async`,
/// `handle_incoming_block`). Runs on a blocking thread.
fn process_share(
    sidechain: &Mutex<SideChain>,
    main: &dyn MainChainView,
    blob: &[u8],
    kind: Kind,
    now: u64,
) -> Processed {
    let (parsed, has_tip, unlock_window, window_time) = {
        let sc = lock(sidechain);
        let parsed = match kind {
            Kind::Broadcast { compact: true } => parse_compact_pool_block(blob, &*sc),
            _ => parse_pool_block(blob, &*sc),
        };
        let cfg = sc.config();
        (
            parsed,
            sc.chain_tip().is_some(),
            sc.options().unlock_window,
            cfg.pplns_window * 4 * cfg.block_time,
        )
    };
    let mut block = match parsed {
        Ok(b) => b,
        Err(e) => return Processed::Ban(format!("invalid share: {e}")),
    };
    let id = block.sidechain_id;
    let height = block.sidechain_height;
    let skipped = || Processed::Read {
        id,
        height,
        result: None,
        block: None,
        events: Vec::new(),
    };
    let tip = main.tip();
    let max_time_delta = match kind {
        Kind::Tip => {
            if block.txin_gen_height + 2 < tip.height {
                return Processed::Ban("chain tip on a stale main-chain block".into());
            }
            if has_tip { window_time } else { 0 }
        }
        Kind::Requested(want) => {
            if id != want {
                return Processed::Ban("answer with another share than requested".into());
            }
            if has_tip { window_time } else { 0 }
        }
        Kind::Broadcast { .. } => {
            if block.prev_id != tip.prev_id && block.txin_gen_height < tip.height {
                let behind = tip.height - block.txin_gen_height;
                if behind >= STALE_BROADCAST_BAN {
                    return Processed::Ban("broadcast of a stale share".into());
                }
                if behind > 1 {
                    return skipped();
                }
            }
            block.offchain.want_broadcast = true;
            BROADCAST_MAX_TIME_DELTA
        }
    };

    {
        let mut sc = lock(sidechain);
        let stored = sc.find_block(&id).is_some();
        if max_time_delta > 0
            && !stored
            && (block.timestamp.saturating_add(max_time_delta) < now
                || block.timestamp > now.saturating_add(max_time_delta))
        {
            return skipped();
        }
        if sc.incoming_block_seen(&block, now) {
            return skipped();
        }
        if stored {
            return Processed::Read {
                id,
                height,
                result: Some(Ok(ExternalOutcome::AlreadyKnown)),
                block: None,
                events: Vec::new(),
            };
        }
    }

    // The proof of work without the lock; the sidechain then only compares it.
    block.offchain.pow_hash = Some(block.pow_hash(unlock_window));
    let copy = Box::new(block.clone());
    let mut sc = lock(sidechain);
    let result = sc.add_external_block(block, main, now);
    let events = sc.take_events();
    Processed::Read {
        id,
        height,
        result: Some(result),
        block: Some(copy),
        events,
    }
}

/// The owner task's state (the C++ `P2PServer` object).
struct Server {
    cfg: P2pServerConfig,
    peer_ids: PeerIds,
    consensus_id: [u8; 32],
    unlock_window: u64,
    sidechain: SharedSideChain,
    main: SharedMainChain,
    conns: HashMap<u64, Conn>,
    next_conn: u64,
    book: PeerBook,
    bans: BanList,
    throttle: BroadcastThrottle,
    /// Failed outgoing connections per address.
    failures: HashMap<SocketAddr, u32>,
    /// Outgoing connections being opened.
    dialing: HashSet<SocketAddr>,
    /// `m_missingBlockRequests`: (connection, id) pairs asked.
    missing_requests: HashSet<(u64, Hash)>,
    /// `m_blockNotifyRequests`.
    notify_requests: HashSet<Hash>,
    /// `m_lookForMissingBlocks`.
    look_for_missing: bool,
    synced: bool,
    /// `m_timerCounter`.
    timer: u64,
    started: Instant,
    last_saved: u64,
    control: mpsc::UnboundedSender<Control>,
    conn_events: mpsc::Sender<ConnEvent>,
    events: mpsc::UnboundedSender<P2pEvent>,
    stats: watch::Sender<P2pStats>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    async fn run(
        mut self,
        mut control: mpsc::UnboundedReceiver<Control>,
        mut conn_events: mpsc::Receiver<ConnEvent>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                c = control.recv() => match c {
                    None | Some(Control::Stop) => break,
                    Some(c) => self.on_control(c),
                },
                Some(e) = conn_events.recv() => self.on_event(e).await,
                _ = tick.tick() => self.on_timer(),
            }
            self.publish_stats();
        }
        self.shutdown();
    }

    fn now(&self) -> u64 {
        (self.cfg.clock)()
    }

    /// Microseconds on a monotonic clock, for the broadcast throttle.
    fn micros(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    fn emit(&self, e: P2pEvent) {
        let _ = self.events.send(e);
    }

    fn shutdown(&mut self) {
        if let Some(a) = self.accept.take() {
            a.abort();
        }
        for c in std::mem::take(&mut self.conns).into_values() {
            c.abort.abort();
        }
        self.save_peer_file();
        self.publish_stats();
    }

    // ---- peer list

    fn load_peer_file(&mut self) {
        let Some(path) = self.cfg.peer_file.clone() else {
            return;
        };
        if let Ok(list) = peerfile::load(&path) {
            self.add_to_book(&list);
        }
    }

    fn save_peer_file(&mut self) {
        let Some(path) = self.cfg.peer_file.clone() else {
            return;
        };
        let _ = peerfile::save(&path, self.book.entries());
        self.last_saved = self.now();
    }

    /// Adds peers that are not banned (`update_peer_in_list`, the end of
    /// `on_peer_list_response`).
    fn add_to_book(&mut self, entries: &[PeerEntry]) {
        let now = Instant::now();
        let mut fresh = Vec::with_capacity(entries.len());
        for e in entries {
            let banned =
                BanKey::ip(e.socket_addr().ip()).is_some_and(|k| self.bans.is_banned(&k, now));
            if !banned {
                fresh.push(*e);
            }
        }
        self.book.add(&fresh, now, |_| false);
    }

    /// Resolves entries in a helper task; `dial` connects to each right away (`--addpeers`),
    /// otherwise they only go into the peer list (seeds).
    fn resolve(&self, entries: Vec<String>, dial: bool) {
        if entries.is_empty() {
            return;
        }
        let control = self.control.clone();
        let port = self.cfg.default_port;
        tokio::spawn(async move {
            let addrs = dialer::resolve(&entries, port).await;
            let _ = control.send(Control::Resolved { addrs, dial });
        });
    }

    // ---- connections

    fn on_control(&mut self, c: Control) {
        match c {
            Control::Attach {
                stream,
                direction,
                key,
            } => self.attach(stream, direction, key),
            Control::Connect(addr) => self.start_dial(addr),
            Control::Dialed { addr, result } => {
                self.dialing.remove(&addr);
                match result {
                    Ok(stream) => {
                        self.failures.remove(&addr);
                        self.add_tcp(stream, addr, Direction::Outgoing);
                    }
                    Err(_) => self.connect_failed(addr),
                }
            }
            Control::Accepted { stream, addr } => self.add_tcp(stream, addr, Direction::Incoming),
            Control::Resolved { addrs, dial } => {
                let entries: Vec<PeerEntry> = addrs
                    .iter()
                    .map(|a| PeerEntry::from_socket_addr(*a).normalized())
                    .collect();
                self.add_to_book(&entries);
                if dial {
                    for a in addrs {
                        self.start_dial(a);
                    }
                }
            }
            Control::Ended { conn, end } => self.ended(conn, &end),
            Control::SidechainChanged => {
                let events = lock(&self.sidechain).take_events();
                self.on_sidechain_events(events);
            }
            Control::Broadcast(id) => self.broadcast_share(id),
            Control::Stop => {}
        }
    }

    /// `on_connect_failed`: the 10th failure drops the address from the peer list.
    fn connect_failed(&mut self, addr: SocketAddr) {
        let n = self.failures.entry(addr).or_insert(0);
        *n += 1;
        if *n >= MAX_FAILED_CONNECTIONS {
            self.failures.remove(&addr);
            let e = PeerEntry::from_socket_addr(addr).normalized();
            self.book.remove(&e.ip);
        }
    }

    fn connected_to(&self, ip: IpAddr) -> bool {
        let key = ip_key(ip);
        self.conns
            .values()
            .any(|c| matches!(c.transport, Transport::Tcp(a) if ip_key(a.ip()) == key))
    }

    fn start_dial(&mut self, addr: SocketAddr) {
        let now = Instant::now();
        if self.dialing.contains(&addr)
            || BanKey::ip(addr.ip()).is_some_and(|k| self.bans.is_banned(&k, now))
            || self
                .conns
                .values()
                .any(|c| c.transport == Transport::Tcp(addr))
        {
            return;
        }
        if BanKey::ip(addr.ip()).is_some()
            && !self.cfg.p2p.allow_same_ip
            && self.connected_to(addr.ip())
        {
            return;
        }
        self.dialing.insert(addr);
        let control = self.control.clone();
        tokio::spawn(async move {
            let result = dialer::dial(addr).await;
            let _ = control.send(Control::Dialed { addr, result });
        });
    }

    /// A new TCP connection (`P2PClient::on_connect`): bans, the incoming limits and one
    /// connection per IP (IPv6 per /64; localhost exempt) unless same-IP connections are
    /// allowed.
    fn add_tcp(&mut self, stream: TcpStream, addr: SocketAddr, direction: Direction) {
        let ip = addr.ip();
        let key = BanKey::ip(ip);
        if key.is_some_and(|k| self.bans.is_banned(&k, Instant::now())) {
            return;
        }
        let localhost = key.is_none();
        if direction == Direction::Incoming {
            let incoming = self
                .conns
                .values()
                .filter(|c| c.is_tcp(Direction::Incoming))
                .count();
            if incoming >= self.cfg.p2p.in_peers {
                return;
            }
            if localhost {
                let local = self
                    .conns
                    .values()
                    .filter(|c| {
                        c.is_tcp(Direction::Incoming)
                            && matches!(c.transport, Transport::Tcp(a) if BanKey::ip(a.ip()).is_none())
                    })
                    .count();
                if local >= self.cfg.p2p.in_peers_localhost {
                    return;
                }
            }
        }
        if !self.cfg.p2p.allow_same_ip && !localhost && self.connected_to(ip) {
            return;
        }
        self.spawn_conn(Box::new(stream), Transport::Tcp(addr), direction);
    }

    /// A Hyperswarm stream: not banned, one connection per public key.
    fn attach(&mut self, stream: Box<dyn PeerStream>, direction: Direction, key: [u8; 32]) {
        if self.bans.is_banned(&BanKey::Key(key), Instant::now())
            || self
                .conns
                .values()
                .any(|c| c.transport == Transport::Hyper(key))
        {
            return;
        }
        self.spawn_conn(stream, Transport::Hyper(key), direction);
    }

    fn spawn_conn(
        &mut self,
        stream: Box<dyn PeerStream>,
        transport: Transport,
        direction: Direction,
    ) {
        let id = self.next_conn;
        self.next_conn += 1;
        let session = PeerSession::new(
            SessionConfig {
                consensus_id: self.consensus_id,
                peer_ids: self.peer_ids,
                domain: transport.domain(),
                direction,
                listen_port: self.cfg.p2p.external_port,
                version: VersionRecord::ours(),
            },
            Instant::now(),
        );
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
        let events = self.conn_events.clone();
        let control = self.control.clone();
        let task = tokio::spawn(async move {
            let end = run_connection(stream, session, id, rx, events).await;
            let _ = control.send(Control::Ended { conn: id, end });
        });
        self.conns.insert(
            id,
            Conn {
                commands: tx,
                abort: task.abort_handle(),
                transport,
                direction,
                peer_id: None,
                handshake: false,
                listen_port: None,
                software: None,
                pending: 0,
                broadcast_max_height: 0,
                last_broadcast: 0,
                last_block_request: 0,
            },
        );
    }

    /// Sends a command; a peer whose queue is full is dropped.
    fn send(&mut self, conn: u64, cmd: Command) -> bool {
        let Some(c) = self.conns.get(&conn) else {
            return false;
        };
        match c.commands.try_send(cmd) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.drop_conn(conn);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Closes a connection without a ban.
    fn drop_conn(&mut self, conn: u64) {
        if let Some(c) = self.conns.remove(&conn) {
            c.abort.abort();
        }
        self.missing_requests.retain(|(c, _)| *c != conn);
    }

    /// Closes a connection and bans the peer.
    fn ban_conn(&mut self, conn: u64, reason: String) {
        if let Some(c) = self.conns.remove(&conn) {
            c.abort.abort();
            self.missing_requests.retain(|(k, _)| *k != conn);
            self.punish(c.transport, reason);
        }
    }

    /// `ban` plus `remove_peer_from_list`.
    fn punish(&mut self, peer: Transport, reason: String) {
        if let Some(key) = peer.ban_key() {
            self.bans.ban(key, Instant::now());
        }
        if let Transport::Tcp(addr) = peer {
            let e = PeerEntry::from_socket_addr(addr).normalized();
            self.book.remove(&e.ip);
        }
        self.emit(P2pEvent::PeerRejected { peer, reason });
    }

    /// A connection task finished (`on_disconnected`): a broken rule bans the peer; a peer
    /// that left before the handshake is dropped from the peer list without a ban.
    fn ended(&mut self, conn: u64, end: &ConnEnd) {
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        self.missing_requests.retain(|(k, _)| *k != conn);
        if let ConnEnd::Ban(e) = end {
            self.punish(c.transport, e.to_string());
        } else if !c.handshake
            && let Transport::Tcp(addr) = c.transport
        {
            let e = PeerEntry::from_socket_addr(addr).normalized();
            self.book.remove(&e.ip);
        }
    }

    // ---- events from connections

    async fn on_event(&mut self, ev: ConnEvent) {
        let ConnEvent { conn, event } = ev;
        if !self.conns.contains_key(&conn) {
            return;
        }
        let now = self.now();
        match event {
            Event::PeerIdentified { peer_id } => self.identified(conn, peer_id),
            Event::HandshakeComplete => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.handshake = true;
                }
            }
            Event::ListenPort(port) => self.listen_port(conn, port),
            Event::BlockRequest(id) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.last_block_request = now;
                }
                self.answer_block_request(conn, id);
            }
            Event::BlockResponse { requested, blob } => {
                if requested.is_some()
                    && let Some(c) = self.conns.get_mut(&conn)
                {
                    c.pending = c.pending.saturating_sub(1);
                }
                if !blob.is_empty() {
                    let kind = requested.map_or(Kind::Tip, |h| Kind::Requested(Hash(h)));
                    self.incoming_share(conn, blob, kind).await;
                }
            }
            Event::BlockBroadcast { blob, compact } => {
                let key = self.throttle_key(conn);
                let t = self.micros();
                if self.throttle.over_limit(key, t) {
                    self.ban_conn(conn, "too many broadcasts".into());
                    return;
                }
                self.incoming_share(conn, blob, Kind::Broadcast { compact })
                    .await;
            }
            Event::BlockNotify(id) => self.block_notify(conn, Hash(id), now),
            Event::PeerListRequest => self.answer_peer_list(conn),
            Event::PeerList(received) => self.peer_list(conn, &received),
            Event::Dropped(_) => {}
        }
    }

    /// A second connection with the same peer id in the same domain is closed without a ban
    /// (`on_handshake_challenge`).
    fn identified(&mut self, conn: u64, peer_id: u64) {
        let Some(domain) = self.conns.get(&conn).map(|c| c.transport.domain()) else {
            return;
        };
        let duplicate = self.conns.iter().any(|(k, c)| {
            *k != conn && c.peer_id == Some(peer_id) && c.transport.domain() == domain
        });
        if duplicate {
            self.drop_conn(conn);
        } else if let Some(c) = self.conns.get_mut(&conn) {
            c.peer_id = Some(peer_id);
        }
    }

    /// `on_listen_port`: the peer is good; a TCP peer's listening address goes into the peer
    /// list.
    fn listen_port(&mut self, conn: u64, port: u16) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        c.listen_port = Some(port);
        if let Transport::Tcp(addr) = c.transport
            && BanKey::ip(addr.ip()).is_some()
        {
            let e = PeerEntry::from_socket_addr(SocketAddr::new(addr.ip(), port)).normalized();
            self.book.remove(&e.ip);
            self.book.add(&[e], Instant::now(), |_| false);
        }
    }

    /// The throttle key (`throttle_broadcast`): the IP (IPv6 /64), the peer id for
    /// localhost, the public key for Hyperswarm peers.
    fn throttle_key(&self, conn: u64) -> BanKey {
        let Some(c) = self.conns.get(&conn) else {
            return BanKey::Key([0; 32]);
        };
        c.transport.ban_key().unwrap_or_else(|| {
            let mut k = [0u8; 32];
            k[0] = 1;
            k[1..9].copy_from_slice(&c.peer_id.unwrap_or(0).to_le_bytes());
            BanKey::Key(k)
        })
    }

    /// `on_block_request` with `get_block_blob`: the share (zero id: our tip unless it is on
    /// a stale main-chain block), announced first by its parent, uncles and grandparent that
    /// we have.
    fn answer_block_request(&mut self, conn: u64, id: Option<[u8; HASH_SIZE]>) {
        let main_height = self.main.tip().height;
        let (blob, notify) = {
            let sc = lock(&self.sidechain);
            let block = match id {
                None => sc
                    .chain_tip()
                    .filter(|b| b.txin_gen_height + 2 >= main_height),
                Some(h) => sc.find_block(&Hash(h)),
            };
            match block {
                None => (Arc::<[u8]>::from(Vec::new()), Vec::new()),
                Some(b) => {
                    let mut notify = Vec::new();
                    let parent = sc.find_block(&b.parent);
                    if parent.is_some() {
                        notify.push(b.parent.0);
                    }
                    for u in &b.uncles {
                        if sc.find_block(u).is_some() {
                            notify.push(u.0);
                        }
                    }
                    if let Some(p) = parent
                        && sc.find_block(&p.parent).is_some()
                    {
                        notify.push(p.parent.0);
                    }
                    (Arc::from(b.serialize(self.unlock_window)), notify)
                }
            }
        };
        self.send(conn, Command::RespondBlock { blob, notify });
    }

    /// `on_peer_list_request`: good TCP peers other than the requester, at their listen
    /// ports.
    fn answer_peer_list(&mut self, conn: u64) {
        let Some(me) = self.conns.get(&conn) else {
            return;
        };
        let my_ip = match me.transport {
            Transport::Tcp(a) => Some(ip_key(a.ip())),
            Transport::Hyper(_) => None,
        };
        let candidates: Vec<PeerEntry> = self
            .conns
            .iter()
            .filter(|(k, c)| **k != conn && c.good())
            .filter_map(|(_, c)| match c.transport {
                Transport::Tcp(a) if Some(ip_key(a.ip())) != my_ip => Some(
                    PeerEntry::from_socket_addr(SocketAddr::new(a.ip(), c.listen_port?)),
                ),
                _ => None,
            })
            .collect();
        let num_connections = self.conns.len();
        self.send(
            conn,
            Command::RespondPeerList {
                candidates,
                num_connections,
            },
        );
    }

    /// `on_peer_list_response`: the version record, then new addresses into the peer list.
    fn peer_list(&mut self, conn: u64, received: &ReceivedPeers) {
        if let Some(v) = received.version
            && let Some(c) = self.conns.get_mut(&conn)
        {
            c.software = Some(v);
        }
        self.add_to_book(&received.peers);
    }

    /// `on_block_notify`: an unknown share is asked for once.
    fn block_notify(&mut self, conn: u64, id: Hash, now: u64) {
        let known = lock(&self.sidechain)
            .find_block(&id)
            .map(|b| b.sidechain_height);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        c.last_broadcast = now;
        if let Some(h) = known {
            c.broadcast_max_height = c.broadcast_max_height.max(h);
            return;
        }
        if c.pending >= REQUEST_LIMIT
            || self.notify_requests.contains(&id)
            || self.missing_requests.contains(&(conn, id))
        {
            return;
        }
        if self.send(conn, Command::RequestBlock(id.0)) {
            self.notify_requests.insert(id);
            self.missing_requests.insert((conn, id));
            if let Some(c) = self.conns.get_mut(&conn) {
                c.pending += 1;
            }
        }
    }

    /// Asks `conn` for shares (`post_handle_incoming_block`), each once per connection.
    fn request_missing(&mut self, conn: u64, ids: &[Hash]) {
        for id in ids {
            let Some(c) = self.conns.get(&conn) else {
                return;
            };
            if c.pending >= REQUEST_LIMIT {
                return;
            }
            if !self.missing_requests.insert((conn, *id)) {
                continue;
            }
            if !self.send(conn, Command::RequestBlock(id.0)) {
                self.missing_requests.remove(&(conn, *id));
                return;
            }
            if let Some(c) = self.conns.get_mut(&conn) {
                c.pending += 1;
            }
        }
    }

    async fn incoming_share(&mut self, conn: u64, blob: Vec<u8>, kind: Kind) {
        let sidechain = Arc::clone(&self.sidechain);
        let main = Arc::clone(&self.main);
        let now = self.now();
        let processed = tokio::task::spawn_blocking(move || {
            process_share(&sidechain, &*main, &blob, kind, now)
        })
        .await;
        if let Ok(p) = processed {
            self.after_share(conn, kind, p, now);
        }
    }

    /// `post_handle_incoming_block`: bans, rejections, missing parents and uncles, events.
    fn after_share(&mut self, conn: u64, kind: Kind, processed: Processed, now: u64) {
        let (id, height, result, block, events) = match processed {
            Processed::Ban(reason) => {
                self.ban_conn(conn, reason);
                return;
            }
            Processed::Read {
                id,
                height,
                result,
                block,
                events,
            } => (id, height, result, block, events),
        };
        if kind != Kind::Requested(id) {
            self.send(conn, Command::NoteBroadcasted(id.0));
            if let Some(c) = self.conns.get_mut(&conn) {
                c.broadcast_max_height = c.broadcast_max_height.max(height);
                if matches!(kind, Kind::Broadcast { .. }) {
                    c.last_broadcast = now;
                }
            }
        }
        match result {
            Some(Ok(ExternalOutcome::Added {
                missing,
                submit_to_daemon,
                found,
            })) => {
                let from = self.conns.get(&conn).map(|c| c.transport);
                self.emit(P2pEvent::ShareAdded { id, height, from });
                if submit_to_daemon && let Some(b) = block {
                    self.emit(P2pEvent::SubmitToDaemon(b));
                }
                if let Some(main) = found {
                    self.emit(P2pEvent::BlockFound { id, main });
                }
                self.look_for_missing = true;
                self.request_missing(conn, &missing);
            }
            Some(Err(rejected)) => {
                if rejected
                    == (BlockRejected::NotEnoughPow {
                        submit_to_daemon: true,
                    })
                    && let Some(b) = block
                {
                    self.emit(P2pEvent::SubmitToDaemon(b));
                }
                self.send(conn, Command::RejectBlock);
            }
            Some(Ok(ExternalOutcome::AlreadyKnown | ExternalOutcome::Ignored(_))) | None => {}
        }
        self.on_sidechain_events(events);
    }

    /// Broadcasts what the sidechain asks for and forwards every event to the node.
    fn on_sidechain_events(&mut self, events: Vec<SidechainEvent>) {
        for e in events {
            if let SidechainEvent::Broadcast(id) = &e {
                self.broadcast_share(*id);
            }
            if matches!(e, SidechainEvent::NewTip { .. }) {
                self.look_for_missing = true;
            }
            self.emit(P2pEvent::Sidechain(e));
        }
    }

    /// `P2PServer::broadcast` and `on_broadcast`: the full share to every good peer, unless
    /// it is more than 2 main-chain blocks away from ours. Each connection sends a
    /// `BLOCK_NOTIFY` instead to a peer that sent us this share.
    fn broadcast_share(&mut self, id: Hash) {
        let main_height = self.main.tip().height;
        let blob = {
            let sc = lock(&self.sidechain);
            let Some(b) = sc.find_block(&id) else {
                return;
            };
            if b.txin_gen_height + 2 < main_height || b.txin_gen_height > main_height + 2 {
                return;
            }
            Arc::<[u8]>::from(b.serialize(self.unlock_window))
        };
        let targets: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.good())
            .map(|(k, _)| *k)
            .collect();
        for t in targets {
            self.send(
                t,
                Command::Broadcast {
                    id: id.0,
                    blob: Arc::clone(&blob),
                },
            );
        }
    }

    // ---- timer

    /// `on_timer`, every 2 s.
    fn on_timer(&mut self) {
        self.timer += 1;
        let t = self.timer;
        let now = self.now();
        self.download_missing_blocks();
        if self.cfg.peer_file.is_some() && now >= self.last_saved + PEER_FILE_SAVE_INTERVAL {
            self.save_peer_file();
        }
        self.update_peer_connections(t, now);
        if t % 30 == 3 {
            lock(&self.sidechain).cleanup_incoming_blocks(now);
        }
        if t % 30 == 7 {
            let us = self.micros();
            self.throttle.clean(us);
        }
        self.bans.prune(Instant::now());
    }

    /// `download_missing_blocks`: each missing parent or uncle from a random good peer, once
    /// per peer until nothing is missing.
    fn download_missing_blocks(&mut self) {
        let has_tip = lock(&self.sidechain).chain_tip().is_some();
        if self.look_for_missing {
            let missing = lock(&self.sidechain).get_missing_blocks();
            if missing.is_empty() {
                self.look_for_missing = false;
                self.missing_requests.clear();
                self.notify_requests.clear();
            } else {
                let good: Vec<u64> = self
                    .conns
                    .iter()
                    .filter(|(_, c)| c.good())
                    .map(|(k, _)| *k)
                    .collect();
                if !good.is_empty() {
                    for id in missing {
                        let conn = good[(super::random_u64() % good.len() as u64) as usize];
                        if self
                            .conns
                            .get(&conn)
                            .is_none_or(|c| c.pending >= REQUEST_LIMIT)
                        {
                            continue;
                        }
                        if !self.missing_requests.insert((conn, id)) {
                            continue;
                        }
                        if self.send(conn, Command::RequestBlock(id.0)) {
                            if let Some(c) = self.conns.get_mut(&conn) {
                                c.pending += 1;
                            }
                        } else {
                            self.missing_requests.remove(&(conn, id));
                        }
                    }
                }
            }
        }
        let synced = has_tip && !self.look_for_missing;
        if synced && !self.synced {
            self.emit(P2pEvent::Synchronized);
        }
        self.synced = synced;
    }

    /// `update_peer_connections`: bans peers that stopped broadcasting while our chain moved,
    /// refreshes and expires the peer list, keeps the outgoing TCP target and reloads the
    /// seeds while no peer is good.
    fn update_peer_connections(&mut self, t: u64, now: u64) {
        let last_updated = lock(&self.sidechain).last_updated();
        if last_updated > 0 {
            let silent: Vec<u64> = self
                .conns
                .iter()
                .filter(|(_, c)| {
                    c.handshake
                        && c.last_broadcast > 0
                        && now >= last_updated.max(c.last_block_request) + 10
                        && last_updated >= c.last_broadcast + SILENT_PEER_LIMIT
                })
                .map(|(k, _)| *k)
                .collect();
            for conn in silent {
                self.ban_conn(conn, "not broadcasting shares".into());
            }
        }

        let instant = Instant::now();
        if t % 30 == 1 {
            let connected: Vec<PeerEntry> = self
                .conns
                .values()
                .filter_map(|c| match c.transport {
                    Transport::Tcp(a) if BanKey::ip(a.ip()).is_some() => Some(
                        PeerEntry::from_socket_addr(SocketAddr::new(a.ip(), c.listen_port?))
                            .normalized(),
                    ),
                    _ => None,
                })
                .collect();
            self.book.add(&connected, instant, |_| false);
            self.book.expire(instant);
        }

        let mut busy: HashSet<[u8; 16]> = self
            .conns
            .values()
            .filter_map(|c| match c.transport {
                Transport::Tcp(a) => Some(ip_key(a.ip())),
                Transport::Hyper(_) => None,
            })
            .collect();
        busy.extend(self.dialing.iter().map(|a| ip_key(a.ip())));
        let mut out = self
            .conns
            .values()
            .filter(|c| c.is_tcp(Direction::Outgoing))
            .count()
            + self.dialing.len();
        if out < self.cfg.p2p.out_peers {
            let mut candidates: Vec<SocketAddr> = Vec::new();
            for e in self.book.entries() {
                let addr = e.socket_addr();
                let banned =
                    BanKey::ip(addr.ip()).is_some_and(|k| self.bans.is_banned(&k, instant));
                if !banned && !busy.contains(&ip_key(addr.ip())) {
                    candidates.push(addr);
                }
            }
            while out < self.cfg.p2p.out_peers && !candidates.is_empty() {
                let k = (super::random_u64() % candidates.len() as u64) as usize;
                let addr = candidates.swap_remove(k);
                if busy.insert(ip_key(addr.ip())) {
                    self.start_dial(addr);
                    out += 1;
                }
            }
        }

        let has_good = self.conns.values().any(Conn::good);
        if !has_good && t.is_multiple_of(10) {
            self.load_peer_file();
            self.resolve(self.cfg.seeds.clone(), false);
        }
    }

    fn publish_stats(&self) {
        let peers: Vec<PeerStatus> = self
            .conns
            .values()
            .map(|c| PeerStatus {
                transport: c.transport,
                direction: c.direction,
                good: c.good(),
                software: c.software,
                height: c.broadcast_max_height,
            })
            .collect();
        let stats = P2pStats {
            peers_out: self
                .conns
                .values()
                .filter(|c| c.is_tcp(Direction::Outgoing))
                .count(),
            peers_in: self
                .conns
                .values()
                .filter(|c| c.is_tcp(Direction::Incoming))
                .count(),
            peers_hyper: self
                .conns
                .values()
                .filter(|c| matches!(c.transport, Transport::Hyper(_)))
                .count(),
            peers_good: peers.iter().filter(|p| p.good).count(),
            connecting: self.dialing.len(),
            known: self.book.len(),
            banned: self.bans.len(),
            synced: self.synced,
            peers,
        };
        self.stats.send_if_modified(|s| {
            if *s == stats {
                false
            } else {
                *s = stats;
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_keys() {
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        let mapped: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        assert_eq!(ip_key(v4), ip_key(mapped));
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::7".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(ip_key(a), ip_key(b));
        assert_ne!(ip_key(a), ip_key(c));
    }

    #[test]
    fn transport_keys() {
        let local = Transport::Tcp("127.0.0.1:1".parse().unwrap());
        assert_eq!(local.ban_key(), None);
        let remote = Transport::Tcp("1.2.3.4:1".parse().unwrap());
        assert!(matches!(remote.ban_key(), Some(BanKey::Ip(_))));
        assert_eq!(
            Transport::Hyper([7; 32]).ban_key(),
            Some(BanKey::Key([7; 32]))
        );
        assert_eq!(Transport::Hyper([7; 32]).domain(), Domain::Hyper);
        assert_eq!(
            Transport::Hyper([0xab; 32]).to_string(),
            "hyper:abababababababab"
        );
    }
}
