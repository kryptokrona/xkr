//! Legacy (Levin) P2P manager: one tokio task that owns the peer table, the peer list, the
//! ban list, the per-peer sync state machines (`xkr_p2p_legacy::sync`) and the [`Downloader`].
//!
//! Connections run in their own tasks (`LegacyPeer::spawn`) and report [`PeerEvent`]s over
//! one channel; outbound dials, inbound handshakes, back-pings and streams handed over by other
//! transports ([`P2pHandle::add_stream`]) run in short-lived tasks and report over the internal
//! channel; the chain thread reports [`ChainEvent`]s. Everything that touches the database for
//! longer than a lookup (serving `NOTIFY_REQUEST_GET_OBJECTS`, pool admission with ring
//! signatures) runs on `spawn_blocking`.
//!
//! As a full C++-compatible peer it
//!
//! * accepts inbound connections (limits: `--in-peers`, `--max-conns-per-ip`, bans), answers
//!   their handshake and back-pings the announced port afterwards (C++ `try_ping`): success
//!   whitelists `ip:my_port`, failure only graylists it;
//! * answers timed syncs (connection task), `NOTIFY_REQUEST_CHAIN`, `NOTIFY_REQUEST_GET_OBJECTS`,
//!   `NOTIFY_REQUEST_TX_POOL` and `NOTIFY_MISSING_TXS` (sync machine), and relays new blocks
//!   (lite to P2P version >= 4) and transactions to every other synchronized peer;
//! * keeps `--out-peers` outbound connections from the white list (70 %), gray list and seeds,
//!   with exponential back-off per address; `--add-peer` / `--add-exclusive-node` are kept
//!   connected;
//! * bans (`--ban-time`, default 24 h) peers that send invalid blocks, malformed or oversized
//!   Levin messages or break the sync protocol;
//! * persists white/gray lists, bans and its peer id in `<data-dir>/p2pstate.json`;
//! * once the main chain has passed a block-version fork (fork 13, `docs/forks.md`), drops and
//!   briefly bans peers that stay at or below the fork height or send blocks with the old major
//!   version ("peer is on an older fork", `docs/node-architecture.md`, "Peers at a fork");
//! * warns when the number of blocks of the last hour deviates strongly from the target
//!   (block-rate monitor, `docs/node-architecture.md`);
//! * knows stripe-pruned peers (`docs/node-architecture.md`, "Pruned peers"): it announces its
//!   own pruning seed in `CORE_SYNC_DATA` (`0x0100`, "recent blocks only", for a database
//!   pruned completely), remembers every peer's (bounded, also in `p2pstate.json`), serves
//!   only blocks it holds in full, and while the downloader waits for a stripe no peer serves
//!   it prefers peers that may hold it, tells the Hyperswarm transport which height to look up
//!   stripe topics for, and rotates out an outbound peer ([`Downloader::rotation_victim`]:
//!   first one that missed the range it claims, then a pruned one without it, never an anchor
//!   or the last peer reaching it) within the downloader's per-span budget when the slots are
//!   full (TCP on its own, Hyperswarm when the transport asks).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use xkr_chain::{AddBlockOutcome, Chain, NodeView, unix_now};
use xkr_consensus::Network;
use xkr_consensus::reward::{max_block_cumulative_size, max_transaction_allowed_size};
use xkr_p2p_legacy::constants::{P2P_DEFAULT_PEERS_IN_HANDSHAKE, RELAY_CHUNK_BYTES};
use xkr_p2p_legacy::peer::ping;
use xkr_p2p_legacy::sync::{
    self, ChainView, PeerSync, SyncAction, SyncEvent, SyncInput, SyncStatus, build_sparse_chain,
    relay_block_messages, serve_get_objects_held, serve_missing_txs, wants_lite_blocks,
};
use xkr_p2p_legacy::wire::{
    CoreSyncData, MissingTxs, NewTransactions, PeerlistEntry, ProtocolMessage, RequestGetObjects,
};
use xkr_p2p_legacy::{
    Accepted, BoxedPeerStream, Handshaked, LegacyConfig, LegacyPeer, LocalState, PeerError,
    PeerEvent, PeerEventKind, PeerHandle, PeerInfo, PeerList,
};
use xkr_rpc::{HyperNetStatus, NetStatus, RpcCommand};
use xkr_storage::pruning::PruningSeed;
use xkr_txpool::TxSource;
use xkr_types::{Hash, RawBlock};

use crate::chain_worker::{ChainEvent, ChainJob};
use crate::downloader::{
    DlAction, Downloader, DownloaderConfig, PeerSlot, Unserved, network_height_estimate, peer_seed,
};
use crate::peerstore::{self, MAX_PEER_SEEDS, SavedState, SeedBook};
use crate::rng::{RandomSource as _, SystemRng, os_u64};

/// Peer events buffered between connection tasks and the manager.
const PEER_EVENT_QUEUE: usize = 4096;
/// Inbound handshakes in progress at once (slow-loris bound).
const MAX_PENDING_HANDSHAKES: usize = 64;
/// Inbound handshakes in progress at once from one IPv4 address or IPv6 /64 (loopback exempt),
/// so one address cannot hold all [`MAX_PENDING_HANDSHAKES`] slots. A C++ node needs at
/// most two at a time (its handshake and a back-ping).
const MAX_PENDING_HANDSHAKES_PER_IP: usize = 3;
/// Most banned addresses remembered; beyond it the ban that ends first is forgotten.
const MAX_BANS: usize = 10_000;
/// Relayed transaction bytes per peer waiting while an earlier batch of the same peer is in
/// pool admission; more is dropped (the pool sync brings them back).
const MAX_QUEUED_ADMISSION_BYTES: usize = 4 * 1024 * 1024;
/// Relayed blocks queued at the chain thread at once; more are dropped (the job queue to
/// the chain thread is otherwise unbounded). Peers relay each block once per block time.
const MAX_QUEUED_LIVE_BLOCKS: usize = 64;
/// How often the peer state file is written (seconds).
const SAVE_INTERVAL_TICKS: u64 = 300;
/// Longest back-off for an address that keeps failing.
const MAX_BACKOFF: Duration = Duration::from_hours(1);
/// Longest back-off for an explicit (`--add-peer`) address.
const MAX_EXPLICIT_BACKOFF: Duration = Duration::from_mins(5);
/// Once our chain is past a block-version fork at height `F`, a peer whose top index stays
/// `<= F` without growing for this long is on an older fork. Longer than two C++ timed-sync
/// intervals (60 s each), so a peer that is still syncing up to the fork is never mistaken for
/// one.
const OLDER_FORK_STALL: Duration = Duration::from_secs(150);
/// Ban for peers on an older fork (explicit peers included; loopback is never banned). Short:
/// the node may be upgraded and come back.
const OLDER_FORK_BAN: Duration = Duration::from_mins(10);
/// Stall observations older than this are forgotten.
const FORK_WATCH_EXPIRY: Duration = Duration::from_hours(1);
/// Block-rate check interval (ticks = seconds).
const BLOCK_RATE_CHECK_TICKS: u64 = 60;
/// Least time between two block-rate warnings.
const BLOCK_RATE_WARN_INTERVAL: Duration = Duration::from_mins(10);
/// Interval of the "upcoming fork" readiness log (ticks = seconds).
const FORK_LOG_TICKS: u64 = 600;
/// Blocks a run must have synced before its completion triggers database maintenance.
const AUTO_MAINTENANCE_MIN_BLOCKS: u64 = 50_000;
/// Least time between two back-pings of the same IP address.
const BACK_PING_INTERVAL: Duration = Duration::from_mins(10);
/// Back-pings running at once.
const MAX_BACK_PINGS: usize = 16;
/// Addresses whose last back-ping is remembered; while it is full, no new address is pinged.
const MAX_BACK_PING_MEMORY: usize = 4096;
/// White-list entries one public /24 network gets from back-pings (one per IP address).
const MAX_WHITE_PER_24: usize = 2;

/// P2P settings.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    /// Network.
    pub network: Network,
    /// Listen address; `None` = no inbound connections.
    pub listen: Option<SocketAddr>,
    /// Port announced in handshakes (0 = hidden).
    pub my_port: u32,
    /// Outgoing connections to maintain (`P2P_DEFAULT_CONNECTIONS_COUNT`).
    pub out_peers: usize,
    /// Maximum incoming connections.
    pub in_peers: usize,
    /// Maximum simultaneous inbound connections from one IPv4 address or IPv6 /64 (loopback
    /// exempt).
    pub max_conns_per_ip: usize,
    /// How long misbehaving peers are banned.
    pub ban_time: Duration,
    /// `--add-peer`: always kept connected.
    pub add_peers: Vec<String>,
    /// `--add-exclusive-node`: the only peers.
    pub exclusive: Vec<String>,
    /// Seed nodes (used while the peer list is small).
    pub seeds: Vec<String>,
    /// Accept private addresses from peer lists.
    pub allow_local_ip: bool,
    /// Directory of `p2pstate.json` (`None` = do not persist).
    pub state_dir: Option<PathBuf>,
    /// Ask the chain thread for database maintenance (repack + compact) once, when an initial
    /// sync of at least [`AUTO_MAINTENANCE_MIN_BLOCKS`] blocks completes.
    pub auto_maintenance: bool,
    /// `--verify-threads`: sizes the bounded blocking executors: serving
    /// `NOTIFY_REQUEST_GET_OBJECTS` / `NOTIFY_MISSING_TXS` (half, at least 2) and pool
    /// admission of relayed transactions (all, at least 1). 0 = the number of cores.
    pub verify_threads: usize,
}

/// Where a peer connection comes from. Stored per peer; `/status` counts peers per origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOrigin {
    /// A TCP connection of the legacy network (dialled or accepted by this manager).
    LegacyTcp {
        /// Remote address.
        addr: SocketAddr,
    },
    /// A Hyperswarm connection running the same Levin protocol (phase 1 of
    /// `docs/hyper-transport.md`), handed over with [`P2pHandle::add_stream`].
    Hyper {
        /// The remote's Noise public key.
        remote_public_key: [u8; 32],
        /// Discovery topic, if known.
        topic: Option<[u8; 32]>,
    },
}

impl PeerOrigin {
    /// Whether this is a Hyperswarm peer.
    #[must_use]
    pub const fn is_hyper(&self) -> bool {
        matches!(self, Self::Hyper { .. })
    }

    /// The address recorded for the peer ([`PeerInfo::addr`]): the TCP address, or for
    /// Hyperswarm peers a synthetic address derived from the public key (see
    /// [`hyper_synthetic_addr`]), so bans and logs are keyed by the key.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        match self {
            Self::LegacyTcp { addr } => *addr,
            Self::Hyper {
                remote_public_key, ..
            } => hyper_synthetic_addr(remote_public_key),
        }
    }
}

impl std::fmt::Display for PeerOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LegacyTcp { addr } => write!(f, "{addr}"),
            Self::Hyper {
                remote_public_key, ..
            } => {
                write!(f, "hyper:")?;
                for b in &remote_public_key[..8] {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// `fd` (unique local IPv6) + the first 15 key bytes, port from key bytes 15..17: a stable,
/// never-routable address per Hyperswarm public key. It never enters the peer lists.
#[must_use]
pub fn hyper_synthetic_addr(key: &[u8; 32]) -> SocketAddr {
    let mut octets = [0u8; 16];
    octets[0] = 0xfd;
    octets[1..].copy_from_slice(&key[..15]);
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::from(octets)),
        u16::from_le_bytes([key[15], key[16]]),
    )
}

/// Why [`P2pHandle::add_stream`] failed.
#[derive(Debug)]
pub enum AddStreamError {
    /// The Levin handshake failed (wrong network, old version, self-connection, timeout, I/O).
    Handshake(PeerError),
    /// The remote only sent a C++ back-ping (`COMMAND_PING`) and the stream is done.
    PingOnly,
    /// The manager refused the peer (banned, duplicate peer id, limits).
    Refused(String),
    /// The P2P manager is not running.
    Stopped,
}

impl std::fmt::Display for AddStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handshake(e) => write!(f, "handshake failed: {e}"),
            Self::PingOnly => f.write_str("the remote only pinged"),
            Self::Refused(r) => write!(f, "peer refused: {r}"),
            Self::Stopped => f.write_str("the P2P manager is not running"),
        }
    }
}

impl std::error::Error for AddStreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Handshake(e) => Some(e),
            Self::PingOnly | Self::Refused(_) | Self::Stopped => None,
        }
    }
}

/// Cheap, cloneable access to a running [`P2pManager`] for other transports.
#[derive(Debug, Clone)]
pub struct P2pHandle {
    legacy: LegacyConfig,
    local: LocalState,
    internal: mpsc::UnboundedSender<Internal>,
    /// Our raw pruning seed (fixed for the life of the database).
    pruning_seed: u32,
    /// First height of the range the downloader waits for (see [`P2pHandle::wanted_height`]).
    wanted_height: watch::Receiver<Option<u64>>,
}

impl P2pHandle {
    /// Our pruning seed (full node: `is_full()`), for the Hyperswarm stripe topics we announce.
    #[must_use]
    pub fn pruning_seed(&self) -> PruningSeed {
        peer_seed(self.pruning_seed)
    }

    /// The first height of the range the downloader needs and no connected peer holds in full
    /// (stripe pruning), `None` while nothing is missing. The Hyperswarm transport looks up the
    /// stripe topics of its holders when it changes (`downloader::stripe_holders_for`,
    /// `hyper::Session::find_stripe_peers`).
    #[must_use]
    pub fn wanted_height(&self) -> watch::Receiver<Option<u64>> {
        self.wanted_height.clone()
    }

    /// Asks the manager to drop (no ban) one outbound Hyperswarm peer without the range the
    /// downloader waits for ([`Downloader::rotation_victim`]), because a stripe lookup found
    /// holders but our established outbound Hyperswarm connections fill the outbound target.
    /// The manager ignores it while nothing is missing and within the rotation budget
    /// ([`Downloader::may_rotate`], shared with TCP).
    pub fn rotate_hyper_for_stripe(&self) {
        let _ = self.internal.send(Internal::HyperStripeRotation);
    }

    /// Publishes the state of the Hyperswarm network's DHT node (UDP port, firewalled, NAT) in
    /// the status `/status` and the console read ([`NetStatus::hyper_net`]).
    pub fn set_hyper_net(&self, status: HyperNetStatus) {
        let _ = self.internal.send(Internal::HyperNet(status));
    }

    /// Hands an already connected stream (for example a Hyperswarm connection on the sync
    /// topic) to the manager: runs the Levin handshake over it, as initiator if `outbound`
    /// (`LegacyPeer::connect_stream`) or as responder (`LegacyPeer::accept_stream`), then
    /// registers the peer like a TCP peer (same sync machine, downloader slots and relay).
    /// Returns the connection id. The peer is closed again when the manager shuts down or
    /// the stream ends.
    pub async fn add_stream(
        &self,
        stream: BoxedPeerStream,
        origin: PeerOrigin,
        outbound: bool,
    ) -> Result<u64, AddStreamError> {
        let addr = origin.peer_addr();
        let hs = if outbound {
            LegacyPeer::connect_stream(stream, addr, &self.legacy, &self.local)
                .await
                .map_err(AddStreamError::Handshake)?
        } else {
            match LegacyPeer::accept_stream(stream, addr, &self.legacy, &self.local)
                .await
                .map_err(AddStreamError::Handshake)?
            {
                Accepted::Peer(hs) => *hs,
                Accepted::Pinged => return Err(AddStreamError::PingOnly),
            }
        };
        let (reply, rx) = oneshot::channel();
        self.internal
            .send(Internal::Stream { hs, origin, reply })
            .map_err(|_| AddStreamError::Stopped)?;
        rx.await
            .map_err(|_| AddStreamError::Stopped)?
            .map_err(AddStreamError::Refused)
    }
}

/// Results of helper tasks.
#[derive(Debug)]
enum Internal {
    Outbound {
        target: String,
        explicit: bool,
        result: Result<Handshaked, PeerError>,
    },
    Inbound(Handshaked),
    Stream {
        hs: Handshaked,
        origin: PeerOrigin,
        reply: oneshot::Sender<Result<u64, String>>,
    },
    BackPing {
        ip: IpAddr,
        port: u32,
        peer_id: u64,
        ok: bool,
    },
    /// A pool-admission batch finished (`txs` = the accepted ones, possibly none).
    TxsAccepted {
        from: Option<u64>,
        txs: Vec<Vec<u8>>,
    },
    /// A `NOTIFY_REQUEST_GET_OBJECTS` / `NOTIFY_MISSING_TXS` answer finished; `failure` is a
    /// protocol violation found while serving (the peer is dropped and banned).
    ServeDone {
        conn: u64,
        failure: Option<&'static str>,
    },
    /// The Hyperswarm transport needs a slot for a stripe holder
    /// ([`P2pHandle::rotate_hyper_for_stripe`]).
    HyperStripeRotation,
    /// The Hyperswarm network's DHT node changed ([`P2pHandle::set_hyper_net`]).
    HyperNet(HyperNetStatus),
}

/// What the accept loop needs to refuse inbound connections before reading from them,
/// published by the manager.
#[derive(Debug, Default)]
struct InboundSnapshot {
    /// Established inbound TCP peers.
    inbound: usize,
    /// Established inbound TCP peers per [`host_key`].
    per_ip: HashMap<IpAddr, usize>,
    /// IPs we are connected or connecting to: their C++ back-pings must get through even when
    /// the inbound slots are full.
    outbound_ips: HashSet<IpAddr>,
}

#[derive(Debug)]
struct PeerEntry {
    handle: PeerHandle,
    info: PeerInfo,
    origin: PeerOrigin,
    sync: PeerSync,
    top_id: Hash,
    remote_height: u64,
    explicit: bool,
    target: Option<String>,
    /// The peer's raw pruning seed (`CORE_SYNC_DATA.pruning_seed`, updated by timed syncs).
    pruning_seed: u32,
    /// A `NOTIFY_REQUEST_GET_OBJECTS` / `NOTIFY_MISSING_TXS` of this peer is being answered
    /// (one at a time per peer).
    serving: bool,
    /// A batch of this peer's relayed transactions is in pool admission.
    admitting: bool,
    /// Relayed transactions waiting for that batch, at most [`MAX_QUEUED_ADMISSION_BYTES`].
    admit_queue: Vec<Vec<u8>>,
    admit_queue_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    blocks: u64,
    bytes: u64,
}

/// The manager.
#[derive(Debug)]
pub struct P2pManager {
    cfg: P2pConfig,
    legacy: LegacyConfig,
    local: LocalState,
    sync_tx: watch::Sender<CoreSyncData>,
    peerlist_tx: watch::Sender<Arc<[PeerlistEntry]>>,
    peerlist: PeerList,
    peers: HashMap<u64, PeerEntry>,
    connecting: HashSet<String>,
    backoff: HashMap<String, (u32, Instant)>,
    bans: HashMap<IpAddr, Instant>,
    bans_tx: watch::Sender<Arc<HashSet<IpAddr>>>,
    next_conn: u64,
    chain: Arc<Chain>,
    jobs: mpsc::UnboundedSender<ChainJob>,
    status: watch::Sender<Arc<NetStatus>>,
    events_tx: mpsc::Sender<PeerEvent>,
    events_rx: Option<mpsc::Receiver<PeerEvent>>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    downloader: Downloader,
    rng: SystemRng,
    shutdown: CancellationToken,
    sample: Sample,
    rates: (f64, f64),
    peer_lists: (Vec<String>, Vec<String>),
    ticks: u64,
    /// Peers seen at or below an active block-version fork: (IP, peer id) -> (their top index,
    /// since). The peer id survives reconnects of the same node; keying by it as well keeps
    /// several nodes behind one address (loopback, NAT) from resetting each other's watch.
    fork_watch: HashMap<(IpAddr, u64), (u64, Instant)>,
    /// Block-version fork whose activation was logged last.
    logged_fork: Option<u8>,
    /// Last block-rate warning.
    last_rate_warning: Option<Instant>,
    /// Tip height when the manager started (auto maintenance after a long sync).
    start_height: u64,
    /// Database maintenance was requested already.
    maintenance_sent: bool,
    /// Permits for serving `NOTIFY_REQUEST_GET_OBJECTS` / `NOTIFY_MISSING_TXS`.
    serve_permits: Arc<Semaphore>,
    /// Permits for pool admission of relayed transactions.
    pool_permits: Arc<Semaphore>,
    /// Relayed blocks queued at the chain thread ([`MAX_QUEUED_LIVE_BLOCKS`]).
    live_block_jobs: usize,
    /// Inbound limits for the accept loop.
    inbound_tx: watch::Sender<Arc<InboundSnapshot>>,
    /// Our pruning seed (read from the database at start; full = `is_full()`).
    pruning_seed: PruningSeed,
    /// Raw pruning seeds of peer-list addresses that announced a non-zero one (persisted,
    /// at most [`MAX_PEER_SEEDS`]).
    peer_seeds: SeedBook,
    /// First height of the range the downloader waits for, published for the Hyperswarm
    /// transport.
    wanted_height_tx: watch::Sender<Option<u64>>,
    /// Last back-ping per IP address ([`BACK_PING_INTERVAL`], [`MAX_BACK_PING_MEMORY`]).
    back_pinged: HashMap<IpAddr, Instant>,
    /// Permits for running back-pings ([`MAX_BACK_PINGS`]).
    back_ping_permits: Arc<Semaphore>,
    /// The Hyperswarm network's DHT node, as the transport last reported it.
    hyper_net: Option<HyperNetStatus>,
}

impl P2pManager {
    /// A manager for `chain`; `jobs` feeds the chain thread, `status` is read by the RPC.
    /// Loads the saved peer state from `cfg.state_dir`.
    #[must_use]
    pub fn new(
        cfg: P2pConfig,
        chain: Arc<Chain>,
        jobs: mpsc::UnboundedSender<ChainJob>,
        status: watch::Sender<Arc<NetStatus>>,
        shutdown: CancellationToken,
    ) -> Self {
        let now_unix = unix_now();
        let saved = cfg
            .state_dir
            .as_deref()
            .and_then(|d| peerstore::load(d, cfg.network, now_unix))
            .unwrap_or_default();
        let peer_id = saved
            .peer_id
            .filter(|id| *id != 0)
            .unwrap_or_else(|| os_u64().max(1));
        let mut legacy = LegacyConfig::new(cfg.network, peer_id, cfg.my_port);
        legacy.allow_local_ip = cfg.allow_local_ip;
        let mut peerlist = legacy.peer_list();
        for e in &saved.white {
            peerlist.append_white(*e);
        }
        for e in &saved.gray {
            peerlist.append_gray(*e);
        }
        let now = Instant::now();
        let bans: HashMap<IpAddr, Instant> = saved
            .bans
            .iter()
            .map(|(ip, until)| {
                (
                    host_key(*ip),
                    now + Duration::from_secs(until.saturating_sub(now_unix)),
                )
            })
            .collect();
        if !saved.white.is_empty() || !saved.gray.is_empty() {
            tracing::info!(
                white = peerlist.white.len(),
                gray = peerlist.gray.len(),
                bans = bans.len(),
                "peer state loaded"
            );
        }
        let tip = chain.tip();
        // A database pruned completely (`xkrd db prune --all`) reports `RECENT_ONLY` (0x0100):
        // never the full seed, so syncing xkrd nodes only ask it for its newest blocks.
        let pruning_seed = chain
            .reader()
            .map_or(PruningSeed::FULL, |r| r.pruning_seed());
        if pruning_seed.is_stripe() {
            tracing::info!(
                stripes = pruning_seed.stripes(),
                stripe = pruning_seed.stripe(),
                seed = pruning_seed.raw(),
                "stripe-pruned node: announcing the pruning seed, serving only blocks held in full"
            );
        } else if pruning_seed.is_recent_only() {
            tracing::info!(
                seed = pruning_seed.raw(),
                "completely pruned node: announcing the recent-blocks-only seed, serving only \
                 blocks held in full"
            );
        }
        let (sync_tx, sync_rx) = watch::channel(CoreSyncData {
            current_height: u32::try_from(tip.height + 1).unwrap_or(u32::MAX),
            top_id: tip.hash,
            pruning_seed: pruning_seed.raw(),
        });
        let (wanted_height_tx, _) = watch::channel(None);
        let head: Arc<[PeerlistEntry]> = Arc::from(peerlist.head(P2P_DEFAULT_PEERS_IN_HANDSHAKE));
        let (peerlist_tx, peerlist_rx) = watch::channel(head);
        let (bans_tx, _) = watch::channel(Arc::new(bans.keys().copied().collect::<HashSet<_>>()));
        let (events_tx, events_rx) = mpsc::channel(PEER_EVENT_QUEUE);
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let threads = if cfg.verify_threads == 0 {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        } else {
            cfg.verify_threads
        };
        let (inbound_tx, _) = watch::channel(Arc::new(InboundSnapshot::default()));
        Self {
            local: LocalState {
                core_sync: sync_rx,
                peerlist_head: peerlist_rx,
            },
            legacy,
            sync_tx,
            peerlist_tx,
            peerlist,
            peers: HashMap::new(),
            connecting: HashSet::new(),
            backoff: HashMap::new(),
            bans,
            bans_tx,
            next_conn: 1,
            chain,
            jobs,
            status,
            events_tx,
            events_rx: Some(events_rx),
            internal_tx,
            internal_rx: Some(internal_rx),
            downloader: Downloader::new(DownloaderConfig::default()),
            rng: SystemRng,
            shutdown,
            sample: Sample {
                at: Instant::now(),
                blocks: 0,
                bytes: 0,
            },
            rates: (0.0, 0.0),
            peer_lists: (Vec::new(), Vec::new()),
            ticks: 0,
            fork_watch: HashMap::new(),
            logged_fork: None,
            last_rate_warning: None,
            start_height: tip.height,
            maintenance_sent: false,
            serve_permits: Arc::new(Semaphore::new((threads / 2).max(2))),
            pool_permits: Arc::new(Semaphore::new(threads.max(1))),
            live_block_jobs: 0,
            inbound_tx,
            pruning_seed,
            peer_seeds: SeedBook::from_saved(saved.pruning_seeds, MAX_PEER_SEEDS),
            wanted_height_tx,
            back_pinged: HashMap::new(),
            back_ping_permits: Arc::new(Semaphore::new(MAX_BACK_PINGS)),
            hyper_net: None,
            cfg,
        }
    }

    /// A handle for other transports (Hyperswarm) to add connected streams.
    #[must_use]
    pub fn handle(&self) -> P2pHandle {
        P2pHandle {
            legacy: self.legacy.clone(),
            local: self.local.clone(),
            internal: self.internal_tx.clone(),
            pruning_seed: self.pruning_seed.raw(),
            wanted_height: self.wanted_height_tx.subscribe(),
        }
    }

    /// Runs until the shutdown token is cancelled.
    pub async fn run(
        mut self,
        mut chain_events: mpsc::UnboundedReceiver<ChainEvent>,
        mut commands: mpsc::UnboundedReceiver<RpcCommand>,
    ) {
        let (Some(mut events_rx), Some(mut internal_rx)) =
            (self.events_rx.take(), self.internal_rx.take())
        else {
            return;
        };
        if let Some(addr) = self.cfg.listen {
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    tracing::info!(%addr, "P2P listening");
                    tokio::spawn(accept_loop(
                        listener,
                        AcceptCtx {
                            cfg: self.legacy.clone(),
                            local: self.local.clone(),
                            tx: self.internal_tx.clone(),
                            bans: self.bans_tx.subscribe(),
                            inbound: self.inbound_tx.subscribe(),
                            in_peers: self.cfg.in_peers,
                            max_conns_per_ip: self.cfg.max_conns_per_ip,
                        },
                        self.shutdown.clone(),
                    ));
                }
                Err(e) => tracing::error!(%addr, error = %e, "cannot listen for P2P connections"),
            }
        } else {
            tracing::info!("P2P listening disabled");
        }
        tracing::info!(
            peer_id = self.legacy.peer_id,
            my_port = self.legacy.my_port,
            "P2P started"
        );
        self.maintain_connections();

        let shutdown = self.shutdown.clone();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                Some(ev) = events_rx.recv() => self.on_peer_event(ev),
                Some(msg) = internal_rx.recv() => self.on_internal(msg),
                Some(ev) = chain_events.recv() => self.on_chain_event(ev),
                Some(cmd) = commands.recv() => self.on_command(cmd),
                _ = tick.tick() => self.on_tick(),
            }
        }
        let step = crate::stopping::step("legacy P2P");
        tracing::info!(peers = self.peers.len(), "P2P shutting down");
        for p in self.peers.values() {
            p.handle.close();
        }
        step.done_with(match self.peers.len() {
            1 => "1 peer disconnected".to_owned(),
            n => format!("{n} peers disconnected"),
        });
        if let Some(dir) = self.cfg.state_dir.clone() {
            let step = crate::stopping::step("peer list");
            let text = self.encoded_state();
            match peerstore::store(&dir, &text) {
                Ok(()) => {
                    tracing::info!(
                        white = self.peerlist.white.len(),
                        gray = self.peerlist.gray.len(),
                        "peer state saved"
                    );
                    step.done_with(format!(
                        "{} white, {} gray saved to p2pstate.json",
                        self.peerlist.white.len(),
                        self.peerlist.gray.len()
                    ));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cannot save the peer state");
                    step.fail(format!("cannot save p2pstate.json: {e}"));
                }
            }
        }
    }

    // ------------------------------------------------------------------ events

    fn on_peer_event(&mut self, ev: PeerEvent) {
        let PeerEvent { conn_id, kind } = ev;
        match kind {
            PeerEventKind::Closed(err) => {
                if let Some(p) = self.peers.remove(&conn_id) {
                    tracing::info!(conn = conn_id, peer = %p.origin, reason = err.as_deref().unwrap_or("closed"), "peer disconnected");
                    self.downloader.on_peer_closed(conn_id);
                }
            }
            PeerEventKind::Misbehaved(reason) => {
                tracing::warn!(conn = conn_id, %reason, "protocol violation");
                self.drop_peer(conn_id, "protocol violation", true);
            }
            PeerEventKind::CoreSync(cs) => {
                if let Some(p) = self.peers.get_mut(&conn_id) {
                    p.top_id = cs.top_id;
                    if p.pruning_seed != cs.pruning_seed {
                        tracing::debug!(
                            conn = conn_id,
                            seed = cs.pruning_seed,
                            "peer changed its pruning seed"
                        );
                        p.pruning_seed = cs.pruning_seed;
                    }
                }
                self.remember_seed(conn_id);
                self.step(conn_id, SyncInput::TimedSync(cs));
                self.check_fork_peer(conn_id);
            }
            PeerEventKind::RemotePeerList {
                entries,
                remote_time,
            } => {
                let peer = self.peers.get(&conn_id);
                let hyper = peer.is_some_and(|p| p.origin.is_hyper());
                let source = peer.and_then(|p| peerlist_source(&p.info.addr));
                // Discovery of Hyperswarm peers is the DHT's job; their lists are not merged.
                if !hyper
                    && self
                        .peerlist
                        .merge_remote_from(&entries, remote_time, unix_now(), source)
                        .is_err()
                {
                    self.drop_peer(conn_id, "peer list entry from the future", false);
                }
            }
            PeerEventKind::Protocol(ProtocolMessage::RequestGetObjects(req)) => {
                self.serve_objects(conn_id, req);
            }
            PeerEventKind::Protocol(ProtocolMessage::MissingTxs(req)) => {
                self.serve_missing(conn_id, req);
            }
            PeerEventKind::Protocol(msg) => self.step(conn_id, SyncInput::Message(msg)),
        }
        self.drive();
    }

    fn on_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Outbound {
                target,
                explicit,
                result,
            } => {
                self.connecting.remove(&target);
                match result {
                    Ok(hs) => {
                        let addr = hs.peer.info.addr;
                        match self.register(
                            hs,
                            Some(target.clone()),
                            explicit,
                            PeerOrigin::LegacyTcp { addr },
                        ) {
                            Ok(_) => {
                                self.backoff.remove(&target);
                            }
                            Err(reason) => {
                                tracing::debug!(%addr, %reason, "outbound peer not registered");
                                // Not taken: the address waits like one that failed.
                                self.bump_dial_backoff(&target, explicit);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(%target, error = %e, "outbound connection failed");
                        self.bump_dial_backoff(&target, explicit);
                        if !explicit && let Ok(SocketAddr::V4(a)) = target.parse::<SocketAddr>() {
                            let (ip, port) = PeerlistEntry::encode_addr(a);
                            // A white entry that fails gets another chance from the gray list.
                            if let Some(e) = self
                                .peerlist
                                .white
                                .iter()
                                .find(|e| e.ip == ip && e.port == port)
                                .copied()
                            {
                                self.peerlist.remove(ip, port);
                                self.peerlist.append_gray(e);
                            } else {
                                self.peerlist
                                    .gray
                                    .retain(|e| !(e.ip == ip && e.port == port));
                            }
                        }
                    }
                }
            }
            Internal::Inbound(hs) => {
                let addr = hs.peer.info.addr;
                if let Err(reason) = self.register(hs, None, false, PeerOrigin::LegacyTcp { addr })
                {
                    tracing::debug!(%addr, %reason, "inbound peer refused");
                }
            }
            Internal::Stream { hs, origin, reply } => {
                let result = self.register(hs, None, false, origin);
                let _ = reply.send(result);
            }
            Internal::BackPing {
                ip,
                port,
                peer_id,
                ok,
            } => {
                if let Some(a) = peerstore::as_v4(&SocketAddr::new(ip, port as u16)) {
                    let (ip32, port32) = PeerlistEntry::encode_addr(a);
                    let entry = PeerlistEntry {
                        ip: ip32,
                        port: port32,
                        id: peer_id,
                        last_seen: unix_now(),
                    };
                    if ok {
                        tracing::debug!(addr = %a, "back ping succeeded, whitelisted");
                        append_white_bounded(&mut self.peerlist, entry);
                    } else {
                        tracing::debug!(addr = %a, "back ping failed, graylisted");
                        self.peerlist.append_gray(entry);
                    }
                }
            }
            Internal::TxsAccepted { from, txs } => {
                self.relay_txs(from, txs);
                if let Some(conn) = from {
                    let next = self.peers.get_mut(&conn).and_then(|p| {
                        p.admitting = false;
                        if p.admit_queue.is_empty() {
                            return None;
                        }
                        p.admitting = true;
                        p.admit_queue_bytes = 0;
                        Some(std::mem::take(&mut p.admit_queue))
                    });
                    if let Some(queued) = next {
                        self.spawn_admission(Some(conn), queued);
                    }
                }
            }
            Internal::ServeDone { conn, failure } => {
                if let Some(p) = self.peers.get_mut(&conn) {
                    p.serving = false;
                }
                if let Some(reason) = failure {
                    self.drop_peer(conn, reason, true);
                }
            }
            Internal::HyperStripeRotation => {
                let now = Instant::now();
                if let Some(range) = self.downloader.rotation_range()
                    && self.downloader.may_rotate(range, now)
                {
                    let rotated = self.drop_for_stripe(range, now, true);
                    if !rotated {
                        tracing::debug!(
                            start = range.start,
                            "no outbound Hyperswarm peer to rotate out for the missing stripe"
                        );
                    }
                }
            }
            Internal::HyperNet(status) => {
                if self.hyper_net != Some(status) {
                    self.hyper_net = Some(status);
                    self.publish_status();
                }
            }
        }
        self.drive();
    }

    fn on_chain_event(&mut self, ev: ChainEvent) {
        match ev {
            ChainEvent::BatchDone {
                generation,
                start,
                count,
                sources,
                report,
                elapsed,
            } => {
                match report {
                    Ok(r) => {
                        if let Some(err) = &r.error
                            && let Some((expected, got)) = err.reason.older_block_version()
                        {
                            // Same culprit rule as `Downloader::on_batch_done`, which then finds
                            // the peer gone (no 24 h ban for an outdated node).
                            let height = start + err.index as u64;
                            if let Some(&(_, culprit)) =
                                sources.iter().rev().find(|(h, _)| *h <= height)
                            {
                                let detail = format!(
                                    "sent block {height} with major version {got}, {expected} required"
                                );
                                self.drop_older_fork(culprit, &detail);
                            }
                        }
                        let t = &r.times;
                        let blocks = r.added_main + r.added_alt;
                        tracing::debug!(
                            start,
                            count,
                            added = r.added_main,
                            alt = r.added_alt,
                            ms = elapsed.as_millis() as u64,
                            prepare_ms = t.prepare.as_millis() as u64,
                            context_ms = t.context.as_millis() as u64,
                            sigs_ms = t.signatures.as_millis() as u64,
                            write_ms = t.write.as_millis() as u64,
                            commit_ms = t.commit.as_millis() as u64,
                            blocks,
                            blocks_per_sec = %format!("{:.1}", per_sec(blocks, elapsed)),
                            "batch verified"
                        );
                        if r.switched {
                            tracing::info!(
                                height = r.tip.height,
                                "main chain switched during sync"
                            );
                        }
                        let mut out = Vec::new();
                        self.downloader
                            .on_batch_done(generation, start, &sources, &r, &mut out);
                        self.run_dl_actions(out);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, start, "batch could not be stored");
                        self.downloader.on_batch_failed(generation);
                    }
                }
                self.publish_sync_data();
            }
            ChainEvent::BlockDone {
                conn,
                block,
                hop,
                local,
                result,
            } => {
                if conn.is_some() {
                    self.live_block_jobs = self.live_block_jobs.saturating_sub(1);
                }
                match result {
                    Ok(r) => {
                        match &r.outcome {
                            AddBlockOutcome::AddedToMain | AddBlockOutcome::AddedAndSwitched => {
                                tracing::info!(
                                    height = ?r.height,
                                    hash = %r.hash,
                                    switched = matches!(r.outcome, AddBlockOutcome::AddedAndSwitched),
                                    mined = local,
                                    "new block"
                                );
                                self.publish_sync_data();
                                // The new tip reaches the status at once (the Hugin relay
                                // dates its jobs with it), not on the next tick.
                                self.publish_status();
                                // C++ relays submitted blocks with hop 0 and received ones with hop + 1.
                                self.relay_block(conn, &block, if local { 0 } else { hop + 1 });
                            }
                            AddBlockOutcome::AddedToAlternative => {
                                tracing::info!(height = ?r.height, hash = %r.hash, mined = local, "alternative block");
                            }
                            AddBlockOutcome::Invalid(reason) => {
                                tracing::warn!(hash = %r.hash, %reason, peer = ?conn, mined = local, "block rejected");
                                if let (Some(c), Some((expected, got))) =
                                    (conn, reason.older_block_version())
                                {
                                    let detail = format!(
                                        "sent a block with major version {got}, {expected} required"
                                    );
                                    self.drop_older_fork(c, &detail);
                                }
                            }
                            AddBlockOutcome::AlreadyExists | AddBlockOutcome::Orphan => {}
                        }
                        if let Some(c) = conn {
                            self.step(c, SyncInput::BlockProcessed(r.outcome.sync_outcome()));
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "block could not be stored"),
                }
            }
        }
        self.drive();
    }

    fn on_command(&mut self, cmd: RpcCommand) {
        match cmd {
            RpcCommand::RelayTransactions(txs) => self.relay_txs(None, txs),
            RpcCommand::SubmitBlock { block, reply } => {
                let job = ChainJob::Block {
                    conn: None,
                    block,
                    hop: 0,
                    local: true,
                    reply: Some(reply),
                };
                if let Err(e) = self.jobs.send(job)
                    && let ChainJob::Block {
                        reply: Some(reply), ..
                    } = e.0
                {
                    let _ = reply.send(Err("the chain thread is not running".to_owned()));
                }
            }
        }
    }

    fn on_tick(&mut self) {
        self.ticks += 1;
        let now = Instant::now();
        for (conn, reason) in self.downloader.check_timeouts(now) {
            let explicit = self.peers.get(&conn).is_some_and(|p| p.explicit);
            if explicit {
                tracing::debug!(conn, reason, "explicit peer not dropped");
            } else {
                self.drop_peer(conn, reason, false);
            }
        }
        if self.ticks.is_multiple_of(2) {
            self.rotate_for_stripe(now);
            self.maintain_connections();
        }
        if self.ticks % 10 == 1 {
            self.peerlist_tx.send_replace(Arc::from(
                self.peerlist.head(P2P_DEFAULT_PEERS_IN_HANDSHAKE),
            ));
            let before = self.bans.len();
            self.bans.retain(|_, until| *until > now);
            if self.bans.len() != before {
                self.publish_bans();
            }
            self.backoff
                .retain(|_, (_, until)| *until + MAX_BACKOFF > now);
            self.back_pinged
                .retain(|_, at| now.saturating_duration_since(*at) < BACK_PING_INTERVAL);
            self.peer_lists = (
                self.peerlist
                    .white
                    .iter()
                    .map(|e| e.addr().to_string())
                    .collect(),
                self.peerlist
                    .gray
                    .iter()
                    .map(|e| e.addr().to_string())
                    .collect(),
            );
        }
        if self.ticks.is_multiple_of(60) {
            let chain = self.chain.clone();
            tokio::task::spawn_blocking(move || {
                let removed = chain.pool().clean(
                    unix_now(),
                    u32::try_from(chain.tip().height).unwrap_or(u32::MAX),
                );
                if !removed.is_empty() {
                    tracing::debug!(count = removed.len(), "expired pool transactions removed");
                }
            });
        }
        if self.ticks.is_multiple_of(SAVE_INTERVAL_TICKS)
            && let Some(dir) = self.cfg.state_dir.clone()
        {
            let text = self.encoded_state();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = peerstore::store(&dir, &text) {
                    tracing::warn!(error = %e, "cannot save the peer state");
                }
            });
        }
        if self.ticks % 10 == 5 {
            let conns: Vec<u64> = self.peers.keys().copied().collect();
            for conn in conns {
                self.check_fork_peer(conn);
            }
            self.fork_watch
                .retain(|_, (_, since)| now.duration_since(*since) < FORK_WATCH_EXPIRY);
            self.log_fork_activation();
        }
        if self.ticks.is_multiple_of(BLOCK_RATE_CHECK_TICKS) {
            self.check_block_rate(now);
        }
        if self.ticks % FORK_LOG_TICKS == 30 {
            self.log_upcoming_fork();
        }
        self.progress(now);
        self.publish_status();
        self.drive();
    }

    // ------------------------------------------------------------------ forks

    /// The block-version fork our main chain has passed (tip index `> F`): `(index, F)`.
    fn passed_version_fork(&self) -> Option<(u8, u64)> {
        let tip = self.chain.tip().height;
        let fork = self.chain.params().forks().last_version_fork_at(tip)?;
        Some((fork.index, fork.height?))
    }

    /// Peer policy at a block-version fork (`docs/node-architecture.md`, "Peers at a fork").
    /// Before the fork nothing happens. Once our tip is past `F`, a peer whose top index
    /// (`CORE_SYNC_DATA.current_height - 1`, from the handshake and every timed sync) stays
    /// `<= F` without growing for [`OLDER_FORK_STALL`] is dropped and briefly banned. The
    /// observation is kept per IP and peer id, so reconnects (old C++ nodes close the connection
    /// themselves when they fail to parse a v6 block, and keep their peer id) do not reset it,
    /// and an up-to-date node behind the same IP (loopback, NAT) does not reset it either.
    fn check_fork_peer(&mut self, conn: u64) {
        let Some((index, fork_height)) = self.passed_version_fork() else {
            return;
        };
        let Some(p) = self.peers.get(&conn) else {
            return;
        };
        let key = (p.info.addr.ip(), p.info.peer_id);
        let peer_top = p.remote_height.saturating_sub(1);
        if peer_top > fork_height {
            self.fork_watch.remove(&key);
            return;
        }
        let now = Instant::now();
        match self.fork_watch.get(&key).copied() {
            Some((seen, since)) if peer_top <= seen => {
                let stalled = now.duration_since(since);
                if stalled >= OLDER_FORK_STALL {
                    let detail = format!(
                        "top block {peer_top} has not passed fork {index} at height {fork_height} for {} s",
                        stalled.as_secs()
                    );
                    self.drop_older_fork(conn, &detail);
                }
            }
            _ => {
                self.fork_watch.insert(key, (peer_top, now));
            }
        }
    }

    /// Drops a peer that cannot follow the active fork and bans its IP for
    /// [`OLDER_FORK_BAN`] (capped by `--ban-time`), explicit peers included so they are not
    /// redialled in a loop. Loopback addresses are only dropped.
    fn drop_older_fork(&mut self, conn: u64, detail: &str) {
        let Some(p) = self.peers.remove(&conn) else {
            return;
        };
        p.handle.close();
        let ip = p.info.addr.ip();
        let ban = if ip.is_loopback() {
            Duration::ZERO
        } else {
            OLDER_FORK_BAN.min(self.cfg.ban_time)
        };
        if !ban.is_zero() {
            self.ban(ip, Instant::now() + ban);
            if let Some(a) = peerstore::as_v4(&p.info.addr) {
                self.peerlist.remove_ip(PeerlistEntry::encode_addr(a).0);
            }
            self.publish_bans();
        }
        tracing::warn!(
            conn,
            peer = %p.origin,
            explicit = p.explicit,
            ban_secs = ban.as_secs(),
            detail,
            "peer is on an older fork"
        );
        self.downloader.on_peer_closed(conn);
    }

    /// Logs once when our chain enters a block-version fork (and at start-up if it is in one).
    fn log_fork_activation(&mut self) {
        let tip = self.chain.tip().height;
        let active = self.chain.params().forks().last_version_fork_at(tip);
        let index = active.map(|f| f.index);
        if index == self.logged_fork {
            return;
        }
        self.logged_fork = index;
        if let Some(f) = active {
            tracing::info!(
                fork = f.index,
                fork_height = ?f.height,
                major_version = f.major_version,
                tip,
                "block-version fork active; peers that stay at or below the fork height will be dropped"
            );
        }
    }

    /// Every [`FORK_LOG_TICKS`]: height and readiness of an upcoming block-version fork.
    fn log_upcoming_fork(&self) {
        let chain = self.chain.clone();
        tokio::task::spawn_blocking(move || {
            let status = match chain.fork_status() {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "fork status unavailable");
                    return;
                }
            };
            if let (Some(index), Some(height)) = (status.next_index, status.next_height)
                && status.signal_index == Some(index)
            {
                tracing::info!(
                    fork = index,
                    fork_height = height,
                    blocks_left = height.saturating_sub(status.tip_height),
                    readiness = ?status.readiness,
                    signalling = status.signalling,
                    window = status.window,
                    "upcoming block-version fork"
                );
            }
        });
    }

    /// Block-rate monitor (Monero `block_rate_notify`): warns, at most every
    /// [`BLOCK_RATE_WARN_INTERVAL`], when the blocks of the last hour are far off the target.
    /// Silent while syncing and on fixed-difficulty networks.
    fn check_block_rate(&mut self, now: Instant) {
        let synced =
            self.chain.tip().height + 1 >= self.network_height() && !self.downloader.downloading();
        if !synced {
            return;
        }
        let rate = match self.chain.block_rate(unix_now()) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "block rate unavailable");
                return;
            }
        };
        if !rate.status.is_abnormal()
            || self
                .last_rate_warning
                .is_some_and(|t| now.duration_since(t) < BLOCK_RATE_WARN_INTERVAL)
        {
            return;
        }
        self.last_rate_warning = Some(now);
        tracing::warn!(
            blocks = rate.blocks,
            expected = rate.expected,
            window_secs = rate.window_secs,
            status = rate.status.as_str(),
            "block rate deviates strongly from the target (hashrate swing, timestamp skew or network split?)"
        );
    }

    // ------------------------------------------------------------------ peers

    fn encoded_state(&self) -> String {
        let now = Instant::now();
        let now_unix = unix_now();
        let listed: HashSet<SocketAddrV4> = self
            .peerlist
            .white
            .iter()
            .chain(&self.peerlist.gray)
            .map(PeerlistEntry::addr)
            .collect();
        let state = SavedState {
            peer_id: Some(self.legacy.peer_id),
            white: self.peerlist.white.clone(),
            gray: self.peerlist.gray.clone(),
            pruning_seeds: self
                .peer_seeds
                .entries()
                .filter(|(a, _)| listed.contains(a))
                .collect(),
            bans: self
                .bans
                .iter()
                .filter(|(_, until)| **until > now)
                .map(|(ip, until)| (*ip, now_unix + until.duration_since(now).as_secs()))
                .collect(),
        };
        peerstore::encode(&state, self.cfg.network, now_unix)
    }

    fn publish_bans(&self) {
        self.bans_tx
            .send_replace(Arc::new(self.bans.keys().copied().collect()));
    }

    fn is_banned(&self, ip: &IpAddr, now: Instant) -> bool {
        self.bans
            .get(&host_key(*ip))
            .is_some_and(|until| *until > now)
    }

    /// Bans `ip` (its [`host_key`]) until `until` (a longer existing ban stays). At most
    /// [`MAX_BANS`] addresses are kept; beyond that the ban that ends first is forgotten.
    fn ban(&mut self, ip: IpAddr, until: Instant) {
        let entry = self.bans.entry(host_key(ip)).or_insert(until);
        if *entry < until {
            *entry = until;
        }
        if self.bans.len() > MAX_BANS
            && let Some(first) = self
                .bans
                .iter()
                .min_by_key(|(_, until)| **until)
                .map(|(ip, _)| *ip)
        {
            self.bans.remove(&first);
        }
    }

    /// Publishes the inbound limits for the accept loop.
    fn publish_inbound(&self) {
        let mut snap = InboundSnapshot::default();
        for p in self.peers.values().filter(|p| !p.origin.is_hyper()) {
            let ip = p.info.addr.ip().to_canonical();
            if p.info.inbound {
                snap.inbound += 1;
                *snap.per_ip.entry(host_key(ip)).or_insert(0) += 1;
            } else {
                snap.outbound_ips.insert(ip);
            }
        }
        for t in &self.connecting {
            if let Ok(a) = t.parse::<SocketAddr>() {
                snap.outbound_ips.insert(a.ip().to_canonical());
            }
        }
        self.inbound_tx.send_replace(Arc::new(snap));
    }

    /// Adds a handshaken connection to the peer table and starts its task.
    fn register(
        &mut self,
        hs: Handshaked,
        target: Option<String>,
        explicit: bool,
        origin: PeerOrigin,
    ) -> Result<u64, String> {
        let Handshaked {
            peer,
            remote_peerlist,
            remote_time,
        } = hs;
        let info = peer.info.clone();
        let ip = info.addr.ip();
        let now = Instant::now();
        if !explicit && self.is_banned(&ip, now) {
            return Err("banned".into());
        }
        let tcp = !origin.is_hyper();
        let clash = peer_id_clash(
            self.peers.iter().map(|(c, p)| ConnectedId {
                conn: *c,
                peer_id: p.info.peer_id,
                ip: p.info.addr.ip(),
                inbound_tcp: p.info.inbound && !p.origin.is_hyper(),
            }),
            info.peer_id,
            ip,
            tcp && !info.inbound,
        );
        if clash == PeerIdClash::Refuse {
            return Err("already connected to this peer id".into());
        }
        if tcp && info.inbound {
            if self
                .peers
                .values()
                .filter(|p| p.info.inbound && !p.origin.is_hyper())
                .count()
                >= self.cfg.in_peers
            {
                return Err("too many inbound peers".into());
            }
            let same_ip = self
                .peers
                .values()
                .filter(|p| p.info.inbound && host_key(p.info.addr.ip()) == host_key(ip))
                .count();
            if !ip.is_loopback() && same_ip >= self.cfg.max_conns_per_ip {
                return Err(format!("too many connections from {ip}"));
            }
        }
        let now_unix = unix_now();
        if tcp && !info.inbound {
            if self
                .peerlist
                .merge_remote_from(
                    &remote_peerlist,
                    remote_time,
                    now_unix,
                    peerlist_source(&info.addr),
                )
                .is_err()
            {
                return Err("handshake peer list from the future".into());
            }
            if let Some(a) = peerstore::as_v4(&info.addr) {
                let (ip32, port) = PeerlistEntry::encode_addr(a);
                self.peerlist
                    .set_just_seen(info.peer_id, ip32, port, now_unix);
            }
        }

        if let PeerIdClash::ReplaceInbound(old) = clash {
            self.drop_peer(
                old,
                "a dialed connection to the same peer id replaces it",
                false,
            );
        }
        let conn = self.next_conn;
        self.next_conn += 1;
        let (handle, _task) = peer.spawn(
            self.legacy.clone(),
            self.local.clone(),
            conn,
            self.events_tx.clone(),
        );
        let height = u64::from(info.core_sync.current_height);
        tracing::info!(
            conn,
            peer = %origin,
            inbound = info.inbound,
            version = info.version,
            height,
            top = %info.core_sync.top_id,
            pruning_seed = info.core_sync.pruning_seed,
            "peer connected"
        );

        // C++ `try_ping`: back-ping the announced port of a fresh inbound TCP peer.
        if tcp && info.inbound && info.my_port != 0 {
            self.spawn_back_ping(ip, info.my_port, info.peer_id, now);
        }

        let input = SyncInput::Handshake {
            core_sync: info.core_sync.clone(),
            version: info.version,
        };
        self.peers.insert(
            conn,
            PeerEntry {
                handle,
                top_id: info.core_sync.top_id,
                remote_height: height,
                sync: PeerSync {
                    managed_download: true,
                    ..PeerSync::default()
                },
                explicit,
                target,
                origin,
                pruning_seed: info.core_sync.pruning_seed,
                info,
                serving: false,
                admitting: false,
                admit_queue: Vec::new(),
                admit_queue_bytes: 0,
            },
        );
        self.publish_inbound();
        self.remember_seed(conn);
        self.step(conn, input);
        self.check_fork_peer(conn);
        Ok(conn)
    }

    /// The peer-list address of a TCP peer: the dialled address, or `ip:my_port` of an inbound
    /// peer that announced a port. Hyperswarm peers have none.
    fn listen_addr(p: &PeerEntry) -> Option<SocketAddrV4> {
        if p.origin.is_hyper() {
            return None;
        }
        let addr = if p.info.inbound {
            if p.info.my_port == 0 || p.info.my_port > u32::from(u16::MAX) {
                return None;
            }
            SocketAddr::new(p.info.addr.ip(), p.info.my_port as u16)
        } else {
            p.info.addr
        };
        peerstore::as_v4(&addr)
    }

    /// Records the peer's pruning seed under its peer-list address (persisted with the lists),
    /// so a restarted node knows which addresses are stripe-pruned. The address is the one the
    /// peer listens on (`ip:my_port` of an inbound peer); the book keeps at most
    /// [`MAX_PEER_SEEDS`] addresses and forgets the one seen longest ago first, so reconnects
    /// with new ports cannot grow it.
    fn remember_seed(&mut self, conn: u64) {
        let Some(p) = self.peers.get(&conn) else {
            return;
        };
        let Some(addr) = Self::listen_addr(p) else {
            return;
        };
        self.peer_seeds.remember(addr, p.pruning_seed);
    }

    /// Whether the peer-list address may hold `range` in full as far as we know: unknown and
    /// full addresses may, a remembered pruned seed must hold it.
    fn may_hold(&self, addr: &str, range: Unserved) -> bool {
        addr.parse::<SocketAddrV4>()
            .ok()
            .and_then(|a| self.peer_seeds.get(a))
            .is_none_or(|raw| peer_seed(raw).holds_range(range.start, range.end))
    }

    /// While the downloader waits for a range no connected peer serves and every outbound TCP
    /// slot is taken, drops (without a ban) one outbound TCP peer without it
    /// ([`Downloader::rotation_victim`]) within the rotation budget
    /// ([`Downloader::may_rotate`]), so `maintain_connections` can dial an address that may
    /// hold it (remembered seed holding the range, or unknown). Only when the lists have such
    /// an address; explicit peers are kept.
    fn rotate_for_stripe(&mut self, now: Instant) {
        let Some(range) = self.downloader.rotation_range() else {
            return;
        };
        if !self.downloader.may_rotate(range, now) || !self.cfg.exclusive.is_empty() {
            return;
        }
        let outbound = self
            .peers
            .values()
            .filter(|p| !p.info.inbound && !p.origin.is_hyper())
            .count();
        if outbound < self.cfg.out_peers {
            return;
        }
        // Only worth it if the lists have an address that may hold the range.
        let candidate = self
            .peerlist
            .white
            .iter()
            .chain(&self.peerlist.gray)
            .map(|e| e.addr().to_string())
            .any(|a| self.may_dial(&a, now) && self.may_hold(&a, range));
        if candidate {
            self.drop_for_stripe(range, now, false);
        }
    }

    /// Drops the [`Downloader::rotation_victim`] for `range` among the outbound, non-explicit
    /// peers of one transport (`hyper`: Hyperswarm, else TCP), if there is one, and records the
    /// rotation in the downloader's budget. False if there was none.
    fn drop_for_stripe(&mut self, range: Unserved, now: Instant, hyper: bool) -> bool {
        let slots = self.peer_slots();
        let victim = self.downloader.rotation_victim(&slots, range, |slot| {
            self.peers
                .get(&slot.conn)
                .is_some_and(|p| !p.info.inbound && !p.explicit && p.origin.is_hyper() == hyper)
        });
        let Some(conn) = victim else {
            return false;
        };
        self.downloader.rotated(range, now);
        self.drop_peer(
            conn,
            "peer does not serve the needed stripe, making room for one that does",
            false,
        );
        true
    }

    /// C++ `try_ping`, at most once per IP address per [`BACK_PING_INTERVAL`] and
    /// [`MAX_BACK_PINGS`] at a time (skipped otherwise).
    fn spawn_back_ping(&mut self, ip: IpAddr, port: u32, peer_id: u64, now: Instant) {
        let Some(a) = peerstore::as_v4(&SocketAddr::new(ip, port as u16)) else {
            return;
        };
        let (ip32, _) = PeerlistEntry::encode_addr(a);
        if port > u32::from(u16::MAX) || !self.peerlist.is_ip_allowed(ip32) {
            return;
        }
        let key = IpAddr::V4(*a.ip());
        let recent = self
            .back_pinged
            .get(&key)
            .is_some_and(|at| now.saturating_duration_since(*at) < BACK_PING_INTERVAL);
        if recent || self.back_pinged.len() >= MAX_BACK_PING_MEMORY {
            tracing::debug!(%a, "back ping skipped");
            return;
        }
        let Ok(permit) = self.back_ping_permits.clone().try_acquire_owned() else {
            tracing::debug!(%a, "back ping skipped, too many running");
            return;
        };
        self.back_pinged.insert(key, now);
        let cfg = self.legacy.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let ok = matches!(ping(SocketAddr::V4(a), &cfg).await, Ok(id) if id == peer_id);
            let _ = tx.send(Internal::BackPing {
                ip,
                port,
                peer_id,
                ok,
            });
        });
    }

    fn drop_peer(&mut self, conn: u64, reason: &str, ban: bool) {
        if let Some(p) = self.peers.remove(&conn) {
            p.handle.close();
            let banned = ban && !p.explicit;
            if banned {
                let ip = p.info.addr.ip();
                self.ban(ip, Instant::now() + self.cfg.ban_time);
                if let Some(a) = peerstore::as_v4(&p.info.addr) {
                    self.peerlist.remove_ip(PeerlistEntry::encode_addr(a).0);
                }
                self.publish_bans();
            }
            tracing::info!(conn, peer = %p.origin, reason, ban = banned, "dropping peer");
            self.downloader.on_peer_closed(conn);
        }
    }

    /// One more failed attempt for `target`: the next dial waits 10 s, 20 s, ... up to
    /// [`MAX_BACKOFF`] ([`MAX_EXPLICIT_BACKOFF`] for explicit peers).
    fn bump_dial_backoff(&mut self, target: &str, explicit: bool) {
        let attempts = self.backoff.get(target).map_or(0, |b| b.0) + 1;
        let max = if explicit {
            MAX_EXPLICIT_BACKOFF
        } else {
            MAX_BACKOFF
        };
        let delay = Duration::from_secs(5u64 << attempts.min(12)).min(max);
        self.backoff
            .insert(target.to_owned(), (attempts, Instant::now() + delay));
    }

    fn is_connected_to(&self, target: &str) -> bool {
        self.peers
            .values()
            .any(|p| p.target.as_deref() == Some(target) || p.info.addr.to_string() == target)
    }

    fn may_dial(&self, target: &str, now: Instant) -> bool {
        !self.connecting.contains(target)
            && !self.is_connected_to(target)
            && self
                .backoff
                .get(target)
                .is_none_or(|(_, until)| now >= *until)
            && target
                .parse::<SocketAddr>()
                .map_or(true, |a| !self.is_banned(&a.ip(), now))
    }

    /// Keeps explicit peers connected and fills the outbound slots (C++ `connections_maker`).
    fn maintain_connections(&mut self) {
        let now = Instant::now();
        let explicit: Vec<String> = if self.cfg.exclusive.is_empty() {
            self.cfg.add_peers.clone()
        } else {
            self.cfg.exclusive.clone()
        };
        for t in explicit {
            if self.may_dial(&t, now) {
                self.dial(t, true);
            }
        }
        if !self.cfg.exclusive.is_empty() {
            return;
        }
        let outbound = self
            .peers
            .values()
            .filter(|p| !p.info.inbound && !p.origin.is_hyper())
            .count()
            + self.connecting.len();
        let mut want = self.cfg.out_peers.saturating_sub(outbound);
        if want == 0 {
            return;
        }
        // While the downloader waits for a stripe, skip addresses known to be pruned without it
        // (unless that leaves nothing to dial).
        let missing = self.downloader.unserved();
        let pick = |entries: &[PeerlistEntry]| -> Vec<String> {
            let all: Vec<String> = entries.iter().map(|e| e.addr().to_string()).collect();
            let Some(range) = missing else {
                return all;
            };
            let holding: Vec<String> = all
                .iter()
                .filter(|a| self.may_hold(a, range))
                .cloned()
                .collect();
            if holding.is_empty() { all } else { holding }
        };
        let white = pick(&self.peerlist.white);
        let gray = pick(&self.peerlist.gray);
        // Gray entries reported by two or more peers are preferred (one peer alone cannot put an
        // address there).
        let multi = pick(&self.peerlist.gray_multi_source());
        let use_seeds = white.len() + gray.len() < 16 || self.peers.is_empty();
        let seeds = if use_seeds {
            self.cfg.seeds.clone()
        } else {
            Vec::new()
        };
        let mut tries = 0;
        while want > 0 && tries < 32 {
            tries += 1;
            let roll = self.rng.below(100);
            let list = if !white.is_empty() && (roll < 70 || gray.is_empty()) {
                &white
            } else if !gray.is_empty() && roll < 90 {
                if !multi.is_empty() && self.rng.below(3) < 2 {
                    &multi
                } else {
                    &gray
                }
            } else if !seeds.is_empty() {
                &seeds
            } else if !gray.is_empty() {
                &gray
            } else {
                break;
            };
            let t = list[self.rng.below(list.len())].clone();
            if self.may_dial(&t, now) {
                self.dial(t, false);
                want -= 1;
            }
        }
    }

    fn dial(&mut self, target: String, explicit: bool) {
        self.connecting.insert(target.clone());
        self.publish_inbound();
        let cfg = self.legacy.clone();
        let local = self.local.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = connect_target(&target, &cfg, &local).await;
            let _ = tx.send(Internal::Outbound {
                target,
                explicit,
                result,
            });
        });
    }

    // ------------------------------------------------------------------ sync

    /// Feeds one input to a peer's sync machine and carries out its actions.
    fn step(&mut self, conn: u64, input: SyncInput) {
        let mut out = Vec::new();
        {
            let Some(p) = self.peers.get_mut(&conn) else {
                return;
            };
            let view = NodeView::new(&self.chain);
            sync::step(&mut p.sync, input, &view, &mut out);
        }
        for action in out {
            match action {
                SyncAction::Send(msg) => self.send(conn, msg),
                SyncAction::Disconnect(reason) => self.drop_peer(conn, reason, true),
                SyncAction::Event(e) => self.on_sync_event(conn, e),
            }
        }
    }

    fn on_sync_event(&mut self, conn: u64, event: SyncEvent) {
        match event {
            SyncEvent::NewBlock { block, hop, .. } => {
                if self.live_block_jobs >= MAX_QUEUED_LIVE_BLOCKS {
                    tracing::debug!(conn, "chain thread busy, relayed block dropped");
                    return;
                }
                self.live_block_jobs += 1;
                let _ = self.jobs.send(ChainJob::Block {
                    conn: Some(conn),
                    block,
                    hop,
                    local: false,
                    reply: None,
                });
            }
            SyncEvent::NewTx(txs) => self.admit_txs(Some(conn), txs),
            SyncEvent::PeerHeight(h) => {
                if let Some(p) = self.peers.get_mut(&conn) {
                    p.remote_height = u64::from(h);
                }
            }
            SyncEvent::Synchronized => tracing::debug!(conn, "peer synchronized"),
            SyncEvent::SyncRequired { remote_height } => {
                tracing::debug!(conn, remote_height, "peer has blocks we lack");
                self.downloader.want_sync();
            }
            SyncEvent::ChainEntry(entry) => {
                let total = entry.total_height;
                let mut out = Vec::new();
                let result = {
                    let view = NodeView::new(&self.chain);
                    self.downloader
                        .on_chain_entry(conn, entry, |h| view.has_block(h), &mut out)
                };
                match result {
                    Ok(()) => {
                        // The entry's total is the peer's height (as C++
                        // `handle_response_chain_entry` sets it).
                        if let Some(p) = self.peers.get_mut(&conn) {
                            p.remote_height = u64::from(total);
                            p.sync.remote_height = total;
                        }
                        self.run_dl_actions(out);
                    }
                    Err(reason) => self.drop_peer(conn, reason, true),
                }
            }
            SyncEvent::Objects(resp) => {
                let mut out = Vec::new();
                self.downloader.on_objects(conn, resp, &mut out);
                self.run_dl_actions(out);
            }
            SyncEvent::BlocksReceived { .. } => {
                tracing::warn!(
                    conn,
                    "unexpected self-managed batch (managed download is on)"
                );
            }
        }
    }

    fn peer_slots(&self) -> Vec<PeerSlot> {
        self.peers
            .iter()
            .map(|(conn, p)| PeerSlot {
                conn: *conn,
                remote_height: p.remote_height,
                syncing: p.sync.status == SyncStatus::Synchronizing,
                usable: matches!(
                    p.sync.status,
                    SyncStatus::Synchronizing | SyncStatus::Normal | SyncStatus::Idle
                ),
                pruning_seed: peer_seed(p.pruning_seed),
                outbound: !p.info.inbound,
            })
            .collect()
    }

    /// Lets the downloader request, buffer and submit; closes finished rounds.
    fn drive(&mut self) {
        // A state hash differed from the built-in list: the chain takes no more blocks, so
        // nothing is downloaded (`Chain::state_mismatch`).
        if self.chain.state_mismatch().is_some() {
            return;
        }
        let slots = self.peer_slots();
        let mut out = Vec::new();
        if self.downloader.needs_chain() && !slots.is_empty() {
            let view = NodeView::new(&self.chain);
            let sparse = build_sparse_chain(&view);
            let our_height = u64::from(view.top().0);
            self.downloader
                .request_chain(&slots, our_height, sparse, &mut out);
        }
        self.downloader.fill(&slots, &mut out);
        self.downloader.submit(&mut out);
        self.run_dl_actions(out);
        let wanted = self.downloader.unserved().map(|r| r.start);
        self.wanted_height_tx.send_if_modified(|w| {
            let changed = *w != wanted;
            *w = wanted;
            changed
        });
        if self.downloader.take_finished_round() {
            self.finish_round();
        }
    }

    fn run_dl_actions(&mut self, out: Vec<DlAction>) {
        for a in out {
            match a {
                DlAction::Send(conn, msg) => self.send(conn, msg),
                DlAction::Drop(conn, reason, ban) => self.drop_peer(conn, reason, ban),
                DlAction::Submit(job) => {
                    let _ = self.jobs.send(job);
                }
            }
        }
    }

    /// Every block the peers announced is in: synchronizing peers whose top we now know go
    /// to the normal state; if some still lead, another round starts.
    fn finish_round(&mut self) {
        let tip = self.chain.tip();
        let our_height = tip.height + 1;
        let syncing: Vec<(u64, Hash, u64)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.sync.status == SyncStatus::Synchronizing)
            .map(|(c, p)| (*c, p.top_id, p.remote_height))
            .collect();
        let mut still_behind = false;
        for (conn, top, height) in syncing {
            if height <= our_height || self.chain.has_block(&top) {
                self.step(conn, SyncInput::DownloadFinished);
            } else {
                still_behind = true;
            }
        }
        tracing::info!(height = tip.height, hash = %tip.hash, "sync round finished");
        if still_behind {
            self.downloader.want_sync();
        }
    }

    // ------------------------------------------------------------------ messages

    fn send(&mut self, conn: u64, msg: ProtocolMessage) {
        let failed = self
            .peers
            .get(&conn)
            .is_some_and(|p| p.handle.notify(msg).is_err());
        if failed {
            self.drop_peer(conn, "outbound queue full", false);
        }
    }

    /// `NOTIFY_REQUEST_GET_OBJECTS` from a peer syncing from us, served off the event loop on
    /// the bounded serve executor, one request per peer at a time (a C++ peer waits
    /// for each answer; a second request while one is served is ignored). At most 100 blocks
    /// and `MAX_RESPONSE_BYTES`. Only blocks held in full (`Reader::has_full_block`) are served;
    /// a stripe-pruned (or completely pruned) node lists the others in `missed_ids`
    /// ([`serve_get_objects_held`]; `docs/node-architecture.md`, "Pruned peers").
    fn serve_objects(&mut self, conn: u64, req: RequestGetObjects) {
        let Some(p) = self.peers.get_mut(&conn) else {
            return;
        };
        if p.serving {
            tracing::debug!(
                conn,
                "NOTIFY_REQUEST_GET_OBJECTS while one is served, ignored"
            );
            return;
        }
        p.serving = true;
        let handle = p.handle.clone();
        let chain = self.chain.clone();
        self.spawn_serve(conn, move || {
            let view = NodeView::new(&chain);
            let reader = view.reader();
            let resp = serve_get_objects_held(&view, &req, |id| {
                reader.is_some_and(
                    |r| matches!(r.height_of(id), Ok(Some(height)) if r.has_full_block(height)),
                )
            });
            let msg = ProtocolMessage::ResponseGetObjects(resp);
            if handle.notify(msg).is_err() {
                handle.close();
            }
            None
        });
    }

    /// `NOTIFY_MISSING_TXS` (a peer completing a lite block we relayed): one database or pool
    /// lookup per hash, so it runs like [`Self::serve_objects`] (it used to run on the
    /// manager task). A request we cannot answer ends the connection, like C++.
    fn serve_missing(&mut self, conn: u64, req: MissingTxs) {
        let Some(p) = self.peers.get_mut(&conn) else {
            return;
        };
        if p.serving {
            tracing::debug!(
                conn,
                "NOTIFY_MISSING_TXS while a request is served, ignored"
            );
            return;
        }
        p.serving = true;
        let handle = p.handle.clone();
        let chain = self.chain.clone();
        self.spawn_serve(conn, move || {
            let view = NodeView::new(&chain);
            match serve_missing_txs(&view, &req) {
                Ok(answer) => {
                    if handle
                        .notify(ProtocolMessage::NewTransactions(answer))
                        .is_err()
                    {
                        handle.close();
                    }
                    None
                }
                Err(reason) => Some(reason),
            }
        });
    }

    /// Runs `job` on the blocking pool once a serve permit is free; reports
    /// [`Internal::ServeDone`].
    fn spawn_serve(&self, conn: u64, job: impl FnOnce() -> Option<&'static str> + Send + 'static) {
        let permits = self.serve_permits.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let failure = match permits.acquire_owned().await {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    job()
                })
                .await
                .unwrap_or(None),
                Err(_) => None,
            };
            let _ = tx.send(Internal::ServeDone { conn, failure });
        });
    }

    /// `relayBlock`: lite blocks to peers that understand them, full blocks to the rest.
    fn relay_block(&mut self, source: Option<u64>, block: &RawBlock, hop: u32) {
        let height = u32::try_from(self.chain.tip().height + 1).unwrap_or(u32::MAX);
        let (lite, full) = relay_block_messages(block, height, hop);
        let targets: Vec<(u64, bool)> = self
            .peers
            .iter()
            .filter(|(c, p)| Some(**c) != source && p.sync.status == SyncStatus::Normal)
            .map(|(c, p)| (*c, wants_lite_blocks(p.info.version)))
            .collect();
        for (conn, lite_ok) in targets {
            self.send(conn, if lite_ok { lite.clone() } else { full.clone() });
        }
    }

    /// `relayTransactions`, split into messages of at most [`RELAY_CHUNK_BYTES`] so peers that
    /// bound unsolicited messages (xkrd: 8 MiB) take them.
    fn relay_txs(&mut self, source: Option<u64>, txs: Vec<Vec<u8>>) {
        if txs.is_empty() {
            return;
        }
        let targets: Vec<u64> = self
            .peers
            .iter()
            .filter(|(c, p)| Some(**c) != source && p.sync.status == SyncStatus::Normal)
            .map(|(c, _)| *c)
            .collect();
        for chunk in chunk_by_bytes(txs, RELAY_CHUNK_BYTES) {
            let msg = ProtocolMessage::NewTransactions(NewTransactions { txs: chunk });
            for conn in &targets {
                self.send(*conn, msg.clone());
            }
        }
    }

    /// Pool admission (ring signatures) of relayed transactions on the bounded pool executor;
    /// accepted ones are relayed. One batch per peer at a time; what the peer sends meanwhile
    /// waits, up to [`MAX_QUEUED_ADMISSION_BYTES`]. Blobs larger than any block can hold
    /// are dropped here without being hashed.
    fn admit_txs(&mut self, from: Option<u64>, txs: Vec<Vec<u8>>) {
        let hard_max = max_block_cumulative_size(self.chain.tip().height + 1);
        let before = txs.len();
        let txs: Vec<Vec<u8>> = txs
            .into_iter()
            .filter(|t| t.len() as u64 <= hard_max)
            .collect();
        if txs.len() != before {
            tracing::debug!(
                conn = ?from,
                dropped = before - txs.len(),
                "relayed transactions larger than a block dropped"
            );
        }
        if txs.is_empty() {
            return;
        }
        let Some(conn) = from else {
            self.spawn_admission(None, txs);
            return;
        };
        let Some(p) = self.peers.get_mut(&conn) else {
            return;
        };
        if p.admitting {
            let bytes: usize = txs.iter().map(Vec::len).sum();
            if p.admit_queue_bytes.saturating_add(bytes) > MAX_QUEUED_ADMISSION_BYTES {
                tracing::debug!(
                    conn,
                    "pool admission queue full, relayed transactions dropped"
                );
                return;
            }
            p.admit_queue_bytes += bytes;
            p.admit_queue.extend(txs);
            return;
        }
        p.admitting = true;
        self.spawn_admission(Some(conn), txs);
    }

    /// One admission batch on the blocking pool once a pool permit is free; always reports
    /// [`Internal::TxsAccepted`].
    fn spawn_admission(&self, from: Option<u64>, txs: Vec<Vec<u8>>) {
        let chain = self.chain.clone();
        let tx = self.internal_tx.clone();
        let permits = self.pool_permits.clone();
        tokio::spawn(async move {
            let accepted = match permits.acquire_owned().await {
                Ok(permit) => tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    admit_blocking(&chain, txs)
                })
                .await
                .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let _ = tx.send(Internal::TxsAccepted {
                from,
                txs: accepted,
            });
        });
    }

    // ------------------------------------------------------------------ status

    fn publish_sync_data(&self) {
        let tip = self.chain.tip();
        self.sync_tx.send_replace(CoreSyncData {
            current_height: u32::try_from(tip.height + 1).unwrap_or(u32::MAX),
            top_id: tip.hash,
            pruning_seed: self.pruning_seed.raw(),
        });
    }

    /// Our height, or the highest height at least two peers announce if that is higher
    /// ([`network_height_estimate`]; with one peer, its height).
    fn network_height(&self) -> u64 {
        let ours = self.chain.tip().height + 1;
        network_height_estimate(self.peers.values().map(|p| p.remote_height)).max(ours)
    }

    /// Requests database maintenance once, when a long initial sync has just completed
    /// (`P2pConfig::auto_maintenance`).
    fn maybe_request_maintenance(&mut self) {
        if !self.cfg.auto_maintenance || self.maintenance_sent || self.downloader.active() {
            return;
        }
        let tip = self.chain.tip().height;
        if tip + 1 < self.network_height()
            || tip.saturating_sub(self.start_height) < AUTO_MAINTENANCE_MIN_BLOCKS
        {
            return;
        }
        self.maintenance_sent = true;
        tracing::info!(
            height = tip,
            synced_blocks = tip - self.start_height,
            "initial sync complete: database maintenance (repack + compact) queued"
        );
        let _ = self.jobs.send(ChainJob::Maintain);
    }

    #[allow(clippy::cast_precision_loss)]
    fn progress(&mut self, now: Instant) {
        self.maybe_request_maintenance();
        let dt = now.duration_since(self.sample.at).as_secs_f64();
        if dt < 1.0 {
            return;
        }
        let stats = self.downloader.stats;
        let blocks = stats.blocks_added.saturating_sub(self.sample.blocks) as f64 / dt;
        let bytes = stats.bytes_downloaded.saturating_sub(self.sample.bytes) as f64 / dt;
        self.rates = (blocks, bytes);
        self.sample = Sample {
            at: now,
            blocks: stats.blocks_added,
            bytes: stats.bytes_downloaded,
        };
        if self.downloader.active() {
            let tip = self.chain.tip();
            tracing::info!(
                height = tip.height,
                target = self.network_height().saturating_sub(1),
                entry_target = self.downloader.target_height.saturating_sub(1),
                blocks_per_sec = %format!("{blocks:.1}"),
                mb_per_sec = %format!("{:.2}", bytes / 1_048_576.0),
                downloaded = stats.blocks_downloaded,
                peers = self.peers.len(),
                in_flight = self.downloader.in_flight(),
                buffered_mb = %format!("{:.1}", self.downloader.ready_bytes() as f64 / 1_048_576.0),
                "syncing"
            );
        }
    }

    fn publish_status(&self) {
        let tip = self.chain.tip();
        let ours = tip.height + 1;
        let network_height = self.network_height();
        let legacy = |inbound: bool| {
            self.peers
                .values()
                .filter(|p| !p.origin.is_hyper() && p.info.inbound == inbound)
                .count()
        };
        let status = NetStatus {
            outgoing: legacy(false),
            incoming: legacy(true),
            hyper: self.peers.values().filter(|p| p.origin.is_hyper()).count(),
            white_peers: self.peer_lists.0.clone(),
            gray_peers: self.peer_lists.1.clone(),
            network_height,
            top_index: tip.height,
            top_hash: tip.hash.0,
            synced: ours >= network_height && !self.downloader.downloading(),
            downloading: self.downloader.downloading(),
            blocks_per_second: self.rates.0,
            bytes_per_second: self.rates.1,
            pruning_seed: self.pruning_seed.raw(),
            pruned_peers: self
                .peers
                .values()
                .filter(|p| !peer_seed(p.pruning_seed).is_full())
                .count(),
            missing_stripe: self.downloader.missing_stripe().map(PruningSeed::raw),
            hyper_net: self.hyper_net,
        };
        self.status.send_replace(Arc::new(status));
        self.publish_inbound();
    }
}

/// Pool admission of one batch (blocking). Blobs above the pool's size limit
/// (`max_transaction_allowed_size` of the block-size median) never reach the pool, which would
/// hash them first.
fn admit_blocking(chain: &Chain, txs: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let max = chain
        .reader()
        .ok()
        .and_then(|r| chain.block_median_size(&r).ok())
        .map_or(u64::MAX, max_transaction_allowed_size);
    let mut accepted = Vec::new();
    for blob in txs {
        if blob.len() as u64 > max {
            tracing::trace!(
                size = blob.len(),
                max,
                "relayed transaction too large for the pool"
            );
            continue;
        }
        match chain.submit_transaction(blob.clone(), TxSource::Peer) {
            Ok(Ok(hash)) => {
                tracing::debug!(%hash, "transaction added to the pool");
                accepted.push(blob);
            }
            Ok(Err(e)) => tracing::trace!(error = %e, "relayed transaction not added"),
            Err(e) => tracing::warn!(error = %e, "pool admission failed"),
        }
    }
    accepted
}

/// `n` per second over `elapsed` (0 for no time).
#[allow(clippy::cast_precision_loss)]
fn per_sec(n: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 { n as f64 / secs } else { 0.0 }
}

/// Splits `txs` into groups of at most `max` bytes (a larger blob forms a group of its own).
fn chunk_by_bytes(txs: Vec<Vec<u8>>, max: usize) -> Vec<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for t in txs {
        if !current.is_empty() && bytes.saturating_add(t.len()) > max {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes = bytes.saturating_add(t.len());
        current.push(t);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether `ip` is a private, shared or link-local IPv4 address (no network-size limits).
fn is_local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let shared = a == 100 && (b & 0xc0) == 64;
    ip.is_private() || ip.is_link_local() || ip.is_loopback() || shared
}

/// The /24 network of `ip`.
fn net24(ip: Ipv4Addr) -> [u8; 3] {
    let o = ip.octets();
    [o[0], o[1], o[2]]
}

/// Whitelists a back-pinged address: it replaces any other white entry of the same IP address
/// (one port per address), and a public /24 network keeps at most [`MAX_WHITE_PER_24`] white
/// addresses from back-pings; beyond that the entry goes to the gray list.
fn append_white_bounded(list: &mut PeerList, entry: PeerlistEntry) {
    let addr = entry.addr();
    let ip = *addr.ip();
    let same_ip: Vec<(u32, u32)> = list
        .white
        .iter()
        .filter(|e| *e.addr().ip() == ip && e.port != entry.port)
        .map(|e| (e.ip, e.port))
        .collect();
    for (ip32, port) in same_ip {
        list.remove(ip32, port);
    }
    if !is_local_v4(ip) && !list.white.iter().any(|e| e.addr() == addr) {
        let in_net: HashSet<Ipv4Addr> = list
            .white
            .iter()
            .map(|e| *e.addr().ip())
            .filter(|other| *other != ip && net24(*other) == net24(ip))
            .collect();
        if in_net.len() >= MAX_WHITE_PER_24 {
            list.append_gray(entry);
            return;
        }
    }
    list.append_white(entry);
}

/// The reporter key of a remote peer list (`PeerList::merge_remote_from`): the peer's IPv4
/// address in the peer-list encoding.
fn peerlist_source(addr: &SocketAddr) -> Option<u32> {
    peerstore::as_v4(addr).map(|a| PeerlistEntry::encode_addr(a).0)
}

/// Resolves `host:port` and connects to the first address that completes a handshake.
async fn connect_target(
    target: &str,
    cfg: &LegacyConfig,
    local: &LocalState,
) -> Result<Handshaked, PeerError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(target).await?.collect();
    let mut last = PeerError::Closed;
    for addr in addrs {
        match LegacyPeer::connect(addr, cfg, local).await {
            Ok(hs) => return Ok(hs),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// What the accept loop needs.
struct AcceptCtx {
    cfg: LegacyConfig,
    local: LocalState,
    tx: mpsc::UnboundedSender<Internal>,
    bans: watch::Receiver<Arc<HashSet<IpAddr>>>,
    inbound: watch::Receiver<Arc<InboundSnapshot>>,
    in_peers: usize,
    max_conns_per_ip: usize,
}

/// The key under which an address is counted for bans, inbound connections per address and
/// pending handshakes: the IPv4 address, or the /64 of an IPv6 address (one host usually owns
/// a whole /64). Unique local addresses (`fc00::/7`, which includes the
/// [`hyper_synthetic_addr`] of every Hyperswarm peer) stay whole.
fn host_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) if !v6.is_unique_local() => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(o))
        }
        other => other,
    }
}

/// Pending handshakes per [`host_key`], shared by the accept loop and its handshake tasks.
type PendingPerIp = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One pending handshake of an address; released on drop.
struct PendingSlot {
    map: PendingPerIp,
    key: IpAddr,
}

impl PendingSlot {
    /// Takes a slot for `key` unless it already holds [`MAX_PENDING_HANDSHAKES_PER_IP`].
    fn take(map: &PendingPerIp, key: IpAddr) -> Option<Self> {
        let mut m = map.lock().unwrap_or_else(PoisonError::into_inner);
        let n = m.entry(key).or_insert(0);
        if *n >= MAX_PENDING_HANDSHAKES_PER_IP {
            return None;
        }
        *n += 1;
        Some(Self {
            map: map.clone(),
            key,
        })
    }
}

impl Drop for PendingSlot {
    fn drop(&mut self) {
        let mut m = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = m.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.key);
            }
        }
    }
}

/// Whether an inbound connection from `ip` is refused before anything is read from it:
/// the inbound slots or the address's share are full. Addresses we are connected or connecting
/// to are exempt (their C++ back-pings must get through), and loopback is exempt.
fn refuse_at_accept(snap: &InboundSnapshot, ip: IpAddr, in_peers: usize, per_ip: usize) -> bool {
    let ip = ip.to_canonical();
    if ip.is_loopback() || snap.outbound_ips.contains(&ip) {
        return false;
    }
    snap.inbound >= in_peers || snap.per_ip.get(&host_key(ip)).copied().unwrap_or(0) >= per_ip
}

/// Accepts inbound connections and runs their handshakes (C++ back-pings from other nodes are
/// answered and closed inside `LegacyPeer::accept`). Before anything is read: banned
/// addresses, connections beyond `--in-peers` or `--max-conns-per-ip` (see
/// [`refuse_at_accept`]), beyond [`MAX_PENDING_HANDSHAKES`] in total or beyond
/// [`MAX_PENDING_HANDSHAKES_PER_IP`] per address are closed at once; the first message must
/// arrive within `LegacyConfig::inbound_handshake_timeout`.
async fn accept_loop(listener: TcpListener, ctx: AcceptCtx, shutdown: CancellationToken) {
    let pending = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    let per_ip: PendingPerIp = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            r = listener.accept() => r,
        };
        let (stream, addr) = match accepted {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if ctx.bans.borrow().contains(&host_key(addr.ip())) {
            tracing::debug!(%addr, "refusing a banned address");
            continue;
        }
        let refused = refuse_at_accept(
            &ctx.inbound.borrow(),
            addr.ip(),
            ctx.in_peers,
            ctx.max_conns_per_ip,
        );
        if refused {
            tracing::debug!(%addr, "inbound limits reached, connection refused");
            continue;
        }
        let Ok(permit) = pending.clone().try_acquire_owned() else {
            tracing::debug!(%addr, "too many handshakes in progress");
            continue;
        };
        let slot = if addr.ip().to_canonical().is_loopback() {
            None
        } else {
            let Some(slot) = PendingSlot::take(&per_ip, host_key(addr.ip())) else {
                tracing::debug!(%addr, "too many handshakes in progress from this address");
                continue;
            };
            Some(slot)
        };
        let (cfg, local, tx) = (ctx.cfg.clone(), ctx.local.clone(), ctx.tx.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let _slot = slot;
            match LegacyPeer::accept(stream, addr, &cfg, &local).await {
                Ok(Accepted::Peer(hs)) => {
                    let _ = tx.send(Internal::Inbound(*hs));
                }
                Ok(Accepted::Pinged) => {}
                Err(e) => tracing::debug!(%addr, error = %e, "inbound handshake failed"),
            }
        });
    }
}

/// A connected peer as far as [`peer_id_clash`] is concerned.
#[derive(Debug, Clone, Copy)]
struct ConnectedId {
    conn: u64,
    peer_id: u64,
    ip: IpAddr,
    /// An inbound TCP connection.
    inbound_tcp: bool,
}

/// What [`peer_id_clash`] decided for a new connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerIdClash {
    /// No connection has the peer id.
    Free,
    /// Keep the existing connection and refuse the new one.
    Refuse,
    /// Keep the new connection and close this inbound one.
    ReplaceInbound(u64),
}

/// One connection per peer id. A TCP connection we dialed replaces an inbound TCP connection
/// with the same peer id from another address (we picked the address we dialed); otherwise the
/// connection that came first stays.
fn peer_id_clash(
    connected: impl IntoIterator<Item = ConnectedId>,
    peer_id: u64,
    ip: IpAddr,
    dialed_tcp: bool,
) -> PeerIdClash {
    let Some(old) = connected.into_iter().find(|c| c.peer_id == peer_id) else {
        return PeerIdClash::Free;
    };
    if dialed_tcp && old.inbound_tcp && old.ip.to_canonical() != ip.to_canonical() {
        PeerIdClash::ReplaceInbound(old.conn)
    } else {
        PeerIdClash::Refuse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_connection_per_peer_id_prefers_dialed_tcp() {
        let ip = |last: u8| IpAddr::V4(Ipv4Addr::new(10, 0, 0, last));
        let inbound = ConnectedId {
            conn: 7,
            peer_id: 42,
            ip: ip(1),
            inbound_tcp: true,
        };
        let dialed = ConnectedId {
            inbound_tcp: false,
            ..inbound
        };
        assert_eq!(peer_id_clash([inbound], 43, ip(2), true), PeerIdClash::Free);
        // Dialed, another address: the inbound connection makes room.
        assert_eq!(
            peer_id_clash([inbound], 42, ip(2), true),
            PeerIdClash::ReplaceInbound(7)
        );
        // Same address (one node connected both ways), an inbound newcomer, or an existing
        // connection that is not inbound TCP: the first one stays.
        assert_eq!(
            peer_id_clash([inbound], 42, ip(1), true),
            PeerIdClash::Refuse
        );
        let mapped = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(
            peer_id_clash([inbound], 42, mapped, true),
            PeerIdClash::Refuse
        );
        assert_eq!(
            peer_id_clash([inbound], 42, ip(2), false),
            PeerIdClash::Refuse
        );
        assert_eq!(
            peer_id_clash([dialed], 42, ip(2), true),
            PeerIdClash::Refuse
        );
        assert_eq!(
            peer_id_clash([dialed], 42, ip(2), false),
            PeerIdClash::Refuse
        );
    }

    #[test]
    fn hyper_addresses_are_stable_and_distinct() {
        let a = hyper_synthetic_addr(&[1u8; 32]);
        assert_eq!(a, hyper_synthetic_addr(&[1u8; 32]));
        assert_ne!(a, hyper_synthetic_addr(&[2u8; 32]));
        assert!(matches!(a.ip(), IpAddr::V6(v6) if v6.octets()[0] == 0xfd));
        let o = PeerOrigin::Hyper {
            remote_public_key: [0xab; 32],
            topic: None,
        };
        assert!(o.is_hyper());
        assert_eq!(o.to_string(), "hyper:abababababababab");
    }

    /// Inbound connections are refused before the handshake when the slots or the
    /// address's share are full, except for loopback and addresses we dial (back-pings).
    #[test]
    fn inbound_limits_apply_at_accept() {
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "198.51.100.1".parse().unwrap();
        let mut snap = InboundSnapshot::default();
        assert!(!refuse_at_accept(&snap, a, 2, 1));
        snap.per_ip.insert(a, 1);
        snap.inbound = 1;
        assert!(refuse_at_accept(&snap, a, 2, 1), "per-IP share full");
        assert!(!refuse_at_accept(&snap, b, 2, 1));
        snap.inbound = 2;
        assert!(refuse_at_accept(&snap, b, 2, 1), "inbound slots full");
        assert!(!refuse_at_accept(&snap, IpAddr::from([127, 0, 0, 1]), 2, 1));
        snap.outbound_ips.insert(b);
        assert!(
            !refuse_at_accept(&snap, b, 2, 1),
            "back-ping of a peer we dial"
        );
        // An IPv4-mapped IPv6 address is the IPv4 address.
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert!(refuse_at_accept(&snap, mapped, 9, 1));
    }

    /// Inbound connections per address and bans count an IPv6 /64 as one address.
    #[test]
    fn ipv6_hosts_count_per_64() {
        let host: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same_net: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        let mut snap = InboundSnapshot::default();
        snap.per_ip.insert(host_key(host), 1);
        snap.inbound = 1;
        assert!(refuse_at_accept(&snap, same_net, 9, 1));
        assert!(!refuse_at_accept(&snap, other, 9, 1));
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(host_key(v4), v4);
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(host_key(mapped), v4);
        // Hyperswarm peers keep their whole synthetic address.
        let a = hyper_synthetic_addr(&[1; 32]).ip();
        let mut close = [1; 32];
        close[14] = 2;
        let b = hyper_synthetic_addr(&close).ip();
        assert_eq!(host_key(a), a);
        assert_ne!(host_key(a), host_key(b));
    }

    /// One address (one IPv6 /64) holds at most `MAX_PENDING_HANDSHAKES_PER_IP` pending
    /// handshakes; slots are released on drop.
    #[test]
    fn pending_handshakes_per_address_are_bounded() {
        let map: PendingPerIp = Arc::new(Mutex::new(HashMap::new()));
        let host: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same_net: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        assert_eq!(host_key(host), host_key(same_net));
        let held: Vec<PendingSlot> = (0..MAX_PENDING_HANDSHAKES_PER_IP)
            .map(|_| PendingSlot::take(&map, host_key(host)).unwrap())
            .collect();
        assert!(PendingSlot::take(&map, host_key(same_net)).is_none());
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(PendingSlot::take(&map, host_key(other)).is_some());
        drop(held);
        assert!(PendingSlot::take(&map, host_key(same_net)).is_some());
        assert!(map.lock().unwrap().get(&host_key(host)).is_none());
    }

    /// A back-pinged address replaces other white entries of its IP address; a public /24
    /// network gets at most `MAX_WHITE_PER_24` white addresses that way (more go to the gray
    /// list); private networks are not limited per /24.
    #[test]
    fn back_pinged_white_entries_are_bounded_per_address() {
        let entry = |addr: &str| {
            let a: SocketAddrV4 = addr.parse().unwrap();
            let (ip, port) = PeerlistEntry::encode_addr(a);
            PeerlistEntry {
                ip,
                port,
                id: 7,
                last_seen: 1,
            }
        };
        let white = |list: &PeerList| -> Vec<String> {
            let mut v: Vec<String> = list.white.iter().map(|e| e.addr().to_string()).collect();
            v.sort();
            v
        };
        let mut list = PeerList::new(false);
        append_white_bounded(&mut list, entry("198.51.100.7:11897"));
        append_white_bounded(&mut list, entry("198.51.100.7:11898"));
        assert_eq!(white(&list), ["198.51.100.7:11898"]);
        // The same address again only updates it.
        append_white_bounded(&mut list, entry("198.51.100.7:11898"));
        append_white_bounded(&mut list, entry("198.51.100.8:11897"));
        append_white_bounded(&mut list, entry("198.51.100.9:11897"));
        assert_eq!(white(&list), ["198.51.100.7:11898", "198.51.100.8:11897"]);
        assert_eq!(list.gray.len(), 1);
        assert_eq!(list.gray[0].addr().to_string(), "198.51.100.9:11897");
        // Another /24, and an address already white in a full /24, are whitelisted.
        append_white_bounded(&mut list, entry("198.51.101.9:11897"));
        append_white_bounded(&mut list, entry("198.51.100.8:11899"));
        assert_eq!(
            white(&list),
            [
                "198.51.100.7:11898",
                "198.51.100.8:11899",
                "198.51.101.9:11897"
            ]
        );

        let mut local = PeerList::new(true);
        for addr in ["10.0.0.1:1", "10.0.0.2:1", "10.0.0.3:1"] {
            append_white_bounded(&mut local, entry(addr));
        }
        assert_eq!(local.white.len(), 3);
        assert!(is_local_v4(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(!is_local_v4(Ipv4Addr::new(100, 128, 0, 1)));
    }

    /// Our relays are split so peers with the 8 MiB unsolicited limit take them.
    #[test]
    fn relays_are_chunked_by_bytes() {
        let txs = vec![
            vec![0u8; 600],
            vec![1u8; 600],
            vec![2u8; 2000],
            vec![3u8; 10],
        ];
        let chunks = chunk_by_bytes(txs.clone(), 1500);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2);
        assert_eq!(chunks[1], vec![vec![2u8; 2000]]);
        assert_eq!(chunks[2], vec![vec![3u8; 10]]);
        assert_eq!(chunks.concat(), txs);
        assert_eq!(chunk_by_bytes(Vec::new(), 10), Vec::<Vec<Vec<u8>>>::new());
    }
}
