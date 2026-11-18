//! The Hyperswarm transport of xkrd (`docs/hyper-transport.md`).
//!
//! Starts the `bridge/` on Bare worker threads inside xkrd through `xkr-p2p-hyper`, joins the
//! XKR topic of the network
//! (`xkr/<mainnet|testnet>/blocks/v1`) and hands every Hyperswarm connection to the P2P manager,
//! which runs the unchanged Levin protocol over it. No ports to open and no seed nodes: the DHT
//! finds peers and holepunches.
//!
//! * **One DHT node, one UDP port**: xkrd runs a single Bare worker. Its HyperDHT node carries
//!   every topic group (the XKR topics on the node key, each Hugin topic on its own identity,
//!   the built-in p2pool's sidechain topic on its own identity), so `--port` fixes one UDP port
//!   for all of them. Holepunching needs no open
//!   port; `--port` is for nodes behind a strict NAT ([`wants_port_hint`]). The worker reports
//!   its DHT state (`DhtStatus`), which reaches `/status` `hyper` and the console's `status`
//!   through [`P2pHandle::set_hyper_net`].
//! * **Discovery**: the bridge's Hyperswarm announces the topic and auto-dials the peers it
//!   finds (`AnnounceMode::Both`, or `Client` with `--hyper-client-only`). A periodic lookup tops
//!   up the outbound set with explicit dials, but only for peers that Hyperswarm has not
//!   connected within one lookup round, so the two rarely race.
//! * **One connection per remote key** ([`Tracker`]); duplicates are resolved the way
//!   Hyperswarm resolves them, so both ends keep the same connection ([`prefer_new`]).
//! * **Bans and back-off** are keyed on the remote DHT public key, which Noise authenticates: a
//!   failed Levin handshake (wrong network, too old, protocol violation) bans the key for a
//!   while; failed dials back off exponentially.
//! * **Supervision**: when a Bare worker dies or the bridge fails, every
//!   connection on it fails (the P2P manager drops those peers), and the bridge is restarted with
//!   back-off (1 s doubling to 60 s) and re-announces. On shutdown: unannounce, close every
//!   connection, close the bridge (the workers leave the DHT), terminate the workers if they
//!   linger ([`BareWorkers`]).
//! * **Stripe topics** (`docs/hyper-transport.md`, "Stripe topics"): the node also announces, in
//!   server mode, the topic of every stripe it holds in full (a full node all stripes of the
//!   default count, a stripe-pruned node its own stripe of its own count, a completely pruned
//!   node none). When the downloader needs a range no connected peer serves
//!   ([`P2pHandle::wanted_height`]), [`Session::find_stripe_peers`] looks up the topic of its
//!   holder under every lookup count (4 and 6, [`stripe_lookup_topics`]) and dials a random
//!   sample of what it finds; if `--hyper-max-peers` is reached and our own established
//!   outbound connections fill the outbound target, it asks the P2P manager to rotate out a
//!   peer without the stripe ([`P2pHandle::rotate_hyper_for_stripe`]) and dials the holders
//!   once a slot frees. Connections on stripe topics are ordinary Levin peers.
//! * **Hugin relay** (`--hugin`, default on; `docs/hugin-relay.md`): the node also joins
//!   hugin-node's three topics (node mesh, public nodes, private node; the testnet has its own)
//!   with Hugin `mid`s, each on its own DHT identity, and hands every connection on them to an
//!   `xkr_hugin::Relay` instead of the Levin P2P manager. The relay, its message pool and its
//!   p2pool client outlive bridge restarts. Jobs come from p2pool (the local node first,
//!   [`xkr_hugin::P2poolSource`]); the shares go there under the operator's fee address
//!   ([`crate::fee`]). Until the node is synchronized apps get no jobs and the public-node
//!   topic is not announced.
//! * **Built-in p2pool** (`--p2pool`, on mainnet also without mode flags; `crate::p2pool`):
//!   once p2pool runs the node joins the sidechain's topic ([`p2pool_topic`]) on its own
//!   identity and hands every connection on it to the p2pool P2P server
//!   ([`P2poolTopic::conns`]).
//! * **Private node** (`--private`, [`PrivateNode`]; `docs/hyper-transport.md`, "Private
//!   node"): every public topic is joined client-only, the address-based Hugin private topic
//!   is not joined, and the node listens only on two identities derived from its secret
//!   (`xkr_p2p_hyper::private`): one for block sync (Levin), one for Hugin apps. Both require
//!   the `mid` of the invite; [`Tracker::admit`] takes inbound Levin connections only from the
//!   sync identity. `invite new` (console) writes a new secret and restarts the bridge, so the
//!   identities of the old one are gone. `--private-peer <invite>` dials a private node's sync
//!   identity with the invite's `mid` and redials it with the usual back-off.
//!
//! * **Clients topic** (`xkr/<network>/clients/v1`, [`crate::clients`]): with the node on, the
//!   node announces it in server mode once synchronized (not in client mode), and with
//!   `--private` only the private clients topic of its invite; connections on either go to the
//!   [`ClientsServer`] (the `xkr-hyper-rpc` protocol), not the Tracker or the Levin P2P manager.
//!
//! Contract with `crate::p2p` (owned by the P2P side): `P2pManager::handle() -> P2pHandle`,
//! `P2pHandle: Clone + Send + Sync + 'static`,
//! `PeerOrigin::Hyper { remote_public_key: [u8; 32], topic: Option<[u8; 32]> }` and
//! `async fn add_stream(&self, BoxedPeerStream, PeerOrigin, outbound: bool) -> Result<_, E>`
//! with `E: std::error::Error + 'static` (normally `PeerError`) that resolves once the Levin
//! handshake succeeded and the peer is registered, or failed. Its only call site is
//! [`hand_off`].

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use xkr_consensus::Network;
use xkr_hugin::relay::GOSSIP_MIN_DIFFICULTY;
use xkr_hugin::{
    ChainTip, FeeInfo, HuginStats, NodeIdentity, P2poolConfig, P2poolInfo, P2poolSource, PeerMeta,
    PoolStatus, Relay, RelayConfig, Role,
};
use xkr_p2p_hyper::frame::{dht_state, nat};
use xkr_p2p_hyper::private::PRIVATE_FILE;
use xkr_p2p_hyper::{
    AnnounceMode, BareConfig, BareWorkers, Bridge, BridgeConfig, BridgeError, CloseHandle,
    CloseReason, DhtStatus, HuginMid, HuginTopic, HyperConn, Invite, OpenFlags, PeerAddr,
    PrivateSecret, PrivateTopics, XkrNetwork, topic,
};
use xkr_p2p_legacy::{BoxedPeerStream, PeerError};
use xkr_rpc::{HyperDhtState, HyperNat, HyperNetStatus, NetStatus};
use xkr_storage::pruning::{DEFAULT_STRIPES, PruningSeed};

use crate::cli::Args;
use crate::clients::ClientsServer;
use crate::downloader::stripe_holders_for;
use crate::fee::{Fee, FeeAddress};
use crate::p2p::{P2pHandle, PeerOrigin};
use crate::rng::{RandomSource as _, SystemRng};

/// A DHT public key (ed25519, the Noise static key of a peer).
pub(crate) type Key = [u8; 32];

/// The DHT key pair in the data directory (JSON `{ seed }`, mode 0600, created by xkrd; files of
/// earlier versions also carry `publicKey`). A stable key keeps the node's identity (bans, Hugin
/// identities, which derive from it) across restarts. xkrd runs one worker, which uses this file.
pub const KEYPAIR_FILE: &str = "hyper-keypair.json";

/// Said once when [`wants_port_hint`] holds (on the terminal by the status display, with
/// `--log-json` as a warning).
pub const PORT_HINT: &str =
    "Behind a strict firewall: forward a UDP port and start with --port <port>";

/// Bridge starts in a row that fail with the same error before Hyperswarm stays off.
const MAX_SAME_START_FAILURES: u32 = 5;

/// Why Hyperswarm stays off for the rest of the process (set once, read by the status display).
static OFF_REASON: OnceLock<String> = OnceLock::new();

/// The line said once when Hyperswarm stays off (on the terminal by the status display, with
/// `--log-json` as a warning).
#[must_use]
pub fn off_line(reason: &str) -> String {
    format!("Hyperswarm off: {reason}")
}

/// Why Hyperswarm stays off for the rest of the process; `None` while it runs or retries.
#[must_use]
pub fn off_reason() -> Option<&'static str> {
    OFF_REASON.get().map(String::as_str)
}

/// The text of a failed bridge start (`BridgeError::Spawn` already says "cannot start the
/// bridge").
fn start_error_text(e: &BridgeError) -> String {
    match e {
        BridgeError::Spawn(_) => e.to_string(),
        other => format!("cannot start the bridge: {other}"),
    }
}

/// What to do after a failed bridge start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartVerdict {
    /// An error not logged yet: log it and try again.
    Log,
    /// An error already logged: try again without logging it again.
    Quiet,
    /// The same error [`MAX_SAME_START_FAILURES`] times in a row: stop trying.
    GiveUp,
}

/// Failed bridge starts since the last start that worked.
#[derive(Debug, Default)]
struct StartFailures {
    /// The last error and how many starts in a row failed with it.
    last: Option<String>,
    in_a_row: u32,
    /// Errors already logged.
    logged: HashSet<String>,
}

impl StartFailures {
    fn record(&mut self, error: &str) -> StartVerdict {
        if self.last.as_deref() == Some(error) {
            self.in_a_row += 1;
        } else {
            self.last = Some(error.to_owned());
            self.in_a_row = 1;
        }
        if self.in_a_row >= MAX_SAME_START_FAILURES {
            StartVerdict::GiveUp
        } else if self.logged.insert(error.to_owned()) {
            StartVerdict::Log
        } else {
            StartVerdict::Quiet
        }
    }

    /// A start worked: count and log from scratch next time.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
/// A bridge session that lived this long resets the restart back-off.
const STABLE_SESSION: Duration = Duration::from_secs(120);
const ANNOUNCE_RETRY: Duration = Duration::from_secs(10);
/// Lookup period with no Hyperswarm peer.
const LOOKUP_EMPTY: Duration = Duration::from_secs(10);
/// Lookup period below the outbound target.
const LOOKUP_LOW: Duration = Duration::from_secs(30);
/// How often to re-check once the target is reached (no lookup is made then).
const LOOKUP_FULL: Duration = Duration::from_secs(120);
/// Peers asked for per lookup.
const LOOKUP_MAX: u16 = 64;
/// Explicit dials stop once this many Hyperswarm peers are connected (or `--hyper-max-peers`).
const OUT_TARGET: usize = 8;
const MAX_CONCURRENT_DIALS: usize = 4;
/// Levin handshakes over connections we initiated running at once. Every connection is admitted
/// into the tracker, and so counted against `--hyper-max-peers`, before its handshake starts;
/// further outbound connections are closed until one finishes.
const MAX_PENDING_HANDSHAKES: usize = 16;
/// Levin handshakes over connections the remote initiated running at once. A new inbound
/// connection beyond it replaces the one pending longest.
const MAX_PENDING_INBOUND: usize = 8;
/// Pending inbound connections from one IP address (IPv6: one /64).
const MAX_PENDING_INBOUND_PER_IP: usize = 2;
/// Inbound connections (pending and established) from one IP address (IPv6: one /64).
const MAX_INBOUND_PER_IP: usize = 4;
const DIAL_BACKOFF_BASE_SECS: u64 = 5;
const DIAL_BACKOFF_MAX: Duration = Duration::from_secs(600);
/// Peer on another network or with a too old P2P version.
const BAN_INCOMPATIBLE: Duration = Duration::from_hours(1);
/// The remote turned out to be this node (same Levin peer id).
const BAN_SELF: Duration = Duration::from_hours(24);
/// Levin protocol violation.
const BAN_PROTOCOL: Duration = Duration::from_mins(10);
/// How long we remember connections we closed on purpose.
const CLOSING_MEMORY: Duration = Duration::from_mins(5);
/// Most remote keys the ban list and the back-off list each keep; when one is full, the entry
/// that ends first makes room for a new key.
const MAX_TRACKED_KEYS: usize = 4_096;
const SHUTDOWN_UNANNOUNCE: Duration = Duration::from_secs(3);
/// Time the Bare worker threads get to end after the bridge closed.
const SHUTDOWN_WORKERS_STOP: Duration = Duration::from_secs(5);
/// The bridge accepts a Hyperswarm `maxPeers` up to this.
const BRIDGE_MAX_PEERS: usize = 65_535;
/// Least time between two lookups of the same stripe topic, and the period at which a missing
/// stripe is looked up again while it stays missing.
const STRIPE_LOOKUP_INTERVAL: Duration = Duration::from_secs(15);
/// Peers asked for per stripe lookup.
const STRIPE_LOOKUP_MAX: u16 = 16;
/// Explicit dials per stripe lookup.
const STRIPE_DIALS: usize = 2;

// ---------------------------------------------------------------------------------------------
// configuration

/// Settings of the Hyperswarm transport.
#[derive(Debug, Clone)]
pub struct HyperConfig {
    pub network: Network,
    /// How to start and talk to the Bare workers.
    pub bridge: BridgeConfig,
    /// `Both`, or `Client` with `--hyper-client-only` or `--private`.
    pub mode: AnnounceMode,
    /// Hard cap on Hyperswarm peers (`--hyper-max-peers`).
    pub max_peers: usize,
    /// The Hugin relay and its settings (`None` with the relay off, `Args::hugin_enabled`).
    pub hugin: Option<HuginSettings>,
    /// Counters of the Hugin relay (all zero with the relay off); for `/status`.
    pub hugin_stats: Arc<HuginStats>,
    /// The P2P status, whose sync state gates the relay's jobs (set by `main`).
    pub net: Option<watch::Receiver<Arc<NetStatus>>>,
    /// The built-in p2pool's sidechain topic (set by `main` with p2pool on).
    pub p2pool: Option<P2poolTopic>,
    /// The private node's secret (set by `main` with `--private`).
    pub private: Option<Arc<PrivateNode>>,
    /// Private nodes to stay connected to (`--private-peer`).
    pub private_peers: Vec<Invite>,
    /// The clients topic's server (set by `main` with the node on, unless
    /// `--hyper-client-only` without `--private`).
    pub clients: Option<Arc<ClientsServer>>,
}

/// The built-in p2pool on the Hyperswarm network: its sidechain topic, joined once p2pool
/// runs, and where the connections on it go.
#[derive(Debug, Clone)]
pub struct P2poolTopic {
    /// The topic ([`p2pool_topic`]).
    pub topic: HuginTopic,
    /// Every connection on the topic (`crate::p2pool` attaches it to its P2P server).
    pub conns: mpsc::UnboundedSender<HyperConn>,
    /// `true` once p2pool runs: the topic is announced from then on.
    pub ready: watch::Receiver<bool>,
}

/// The Hyperswarm swarm of the built-in p2pool's sidechain. The topic key is the sidechain's
/// topic key (`xkr_p2pool::p2p::hyperswarm_topic_key`, from the consensus id); the swarm is
/// announced with it like a Hugin topic, so the bridge gives it its own DHT identity and tags
/// every connection on it with the topic (the plain swarm cannot tell its server-side
/// connections apart, and keeps one connection per remote key), and peers that join it prove
/// with their `mid` that they know the topic key.
#[must_use]
pub fn p2pool_topic(network: Network, consensus_id: &xkr_types::Hash) -> HuginTopic {
    let topic_key = xkr_p2pool::p2p::hyperswarm_topic_key(network, consensus_id);
    let base_public_key = topic::ed25519_public_from_seed(&topic_key);
    HuginTopic {
        key: hex::encode(topic_key),
        topic_key,
        base_public_key,
        topic: topic::hugin_topic_on(xkr_network(network), &base_public_key),
    }
}

impl HyperConfig {
    /// From the command line; the key pair lives in `data_dir`.
    #[must_use]
    pub fn from_args(args: &Args, network: Network, data_dir: &Path) -> Self {
        let max_peers = args.hyper_max_peers.max(1);
        let bridge = BridgeConfig {
            bridge_dir: args.hyper_bridge_dir.clone(),
            keypair_file: Some(data_dir.join(KEYPAIR_FILE)),
            bootstrap: args.hyper_bootstrap.clone(),
            udp_port: args.port,
            max_peers: Some(u32::try_from(max_peers.min(BRIDGE_MAX_PEERS)).unwrap_or(u32::MAX)),
            log_json: args.log_json,
            ..BridgeConfig::default()
        };
        Self {
            network,
            bridge,
            mode: if args.hyper_client_only || args.private {
                AnnounceMode::Client
            } else {
                AnnounceMode::Both
            },
            max_peers,
            hugin: args
                .hugin_enabled()
                .then(|| HuginSettings::from_args(args, data_dir)),
            hugin_stats: Arc::new(HuginStats::default()),
            net: None,
            p2pool: None,
            private: None,
            private_peers: args.private_peer.clone(),
            clients: None,
        }
    }

    /// One Bare worker for every topic group: its one HyperDHT node (one UDP socket, the
    /// `--port`) carries the plain swarm of the XKR topics (blocks and stripes, under the node
    /// key) and one swarm per Hugin identity (`Bridge` routes Hugin announces to the last worker,
    /// which is this one). Hyperswarm has one key pair per swarm, so each Hugin identity needs
    /// its own swarm, but swarms share a DHT node (`new Hyperswarm({ dht, keyPair })`). The
    /// Hugin traffic is small next to block sync, and Hugin's share hashing runs in Rust, so one
    /// JS thread carries both.
    #[must_use]
    #[allow(clippy::unused_self)] // kept a method: the worker count may depend on the config again
    pub fn bare_config(&self) -> BareConfig {
        BareConfig {
            workers: 1,
            ..BareConfig::default()
        }
    }

    /// `--port` was given.
    #[must_use]
    pub fn port_fixed(&self) -> bool {
        self.bridge.udp_port.is_some()
    }

    /// Starts the Bare workers and connects to them. The future owns what it needs, so it does
    /// not borrow the configuration across the start.
    fn start_bridge(
        &self,
    ) -> impl Future<Output = Result<(Bridge, BareWorkers), BridgeError>> + Send + 'static {
        let config = self.bridge.clone();
        let bare = self.bare_config();
        async move { Bridge::start_in_process(config, &bare).await }
    }
}

/// The private node (`--private`): its secret file (`<data dir>/hyper-private.json`), shared
/// by the transport and the console (`invite`, `invite new`). A new secret reaches the running
/// session, which restarts the bridge with the new identities.
#[derive(Debug)]
pub struct PrivateNode {
    file: PathBuf,
    secret: watch::Sender<PrivateSecret>,
    /// The Hugin relay's address, once the relay runs (the start of the Hugin app invite).
    hugin_address: OnceLock<String>,
}

impl PrivateNode {
    /// Reads the secret file in `data_dir`, creating it on the first start.
    pub fn load(data_dir: &Path) -> Result<Arc<Self>, String> {
        let file = data_dir.join(PRIVATE_FILE);
        let secret = PrivateSecret::load_or_create(&file)?;
        Ok(Arc::new(Self {
            file,
            secret: watch::channel(secret).0,
            hugin_address: OnceLock::new(),
        }))
    }

    /// The invite of the current secret.
    #[must_use]
    pub fn invite(&self) -> Invite {
        self.secret.borrow().invite()
    }

    /// The Hugin app invite of the current secret, once the Hugin relay runs.
    #[must_use]
    pub fn hugin_app_invite(&self) -> Option<String> {
        let address = self.hugin_address.get()?;
        Some(self.secret.borrow().hugin_app_invite(address))
    }

    /// Records the Hugin relay's address for the Hugin app invite.
    fn set_hugin_address(&self, address: &str) {
        let _ = self.hugin_address.set(address.to_owned());
    }

    /// Writes a new secret and hands it to the transport; invites of the old one stop working.
    pub fn renew(&self) -> Result<Invite, String> {
        let secret = PrivateSecret::renew(&self.file)?;
        let invite = secret.invite();
        self.secret.send_replace(secret);
        Ok(invite)
    }

    fn subscribe(&self) -> watch::Receiver<PrivateSecret> {
        self.secret.subscribe()
    }
}

/// The `/status` `hyper` view of a worker's `DhtStatus`; `port_fixed`: `--port` was given
/// (`private` stays `false`: the caller sets it).
#[must_use]
pub fn hyper_net_status(d: &DhtStatus, port_fixed: bool) -> HyperNetStatus {
    let state = match d.state {
        dht_state::READY => HyperDhtState::Ready,
        dht_state::FAILED => HyperDhtState::Failed,
        _ => HyperDhtState::Starting,
    };
    HyperNetStatus {
        state,
        udp_port: (d.udp_port != 0).then_some(d.udp_port),
        port_fixed,
        firewalled: (state == HyperDhtState::Ready).then_some(d.firewalled),
        nat: match d.nat {
            nat::CONSISTENT => HyperNat::Consistent,
            nat::RANDOM => HyperNat::Random,
            _ => HyperNat::Unknown,
        },
        private: false,
    }
}

/// Whether to suggest `--port` ([`PORT_HINT`]): the DHT is ready, other nodes cannot reach its
/// UDP port, the NAT gives every destination another public port (holepunching between two
/// such nodes does not work, so a forwarded port is what helps), and no `--port` was given. A
/// firewalled node behind an ordinary NAT holepunches fine and gets no hint.
#[must_use]
pub fn wants_port_hint(s: HyperNetStatus) -> bool {
    s.state == HyperDhtState::Ready
        && s.firewalled == Some(true)
        && s.nat == HyperNat::Random
        && !s.port_fixed
}

/// Whether UDP `port` can be bound on every IPv4 address right now (the DHT binds
/// `0.0.0.0:<port>`); checked before the worker starts, so a taken `--port` shows up at once.
fn udp_port_free(port: u16) -> bool {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

/// Publishes a worker's `DhtStatus` and logs it; says [`PORT_HINT`] once per process
/// (`hint_said`) when [`wants_port_hint`] holds.
fn report_dht(p2p: &P2pHandle, d: &DhtStatus, cfg: &HyperConfig, hint_said: &mut bool) {
    let st = HyperNetStatus {
        private: cfg.private.is_some(),
        ..hyper_net_status(d, cfg.port_fixed())
    };
    p2p.set_hyper_net(st);
    match st.state {
        HyperDhtState::Ready => tracing::info!(
            udp_port = ?st.udp_port,
            firewalled = ?st.firewalled,
            nat = ?st.nat,
            port_fixed = st.port_fixed,
            "Hyperswarm DHT ready"
        ),
        HyperDhtState::Failed => tracing::warn!(
            port = ?cfg.bridge.udp_port,
            "Hyperswarm DHT could not start (is the --port UDP port taken?)"
        ),
        HyperDhtState::Starting => {
            tracing::debug!(udp_port = ?st.udp_port, "Hyperswarm DHT starting");
        }
    }
    if !*hint_said && wants_port_hint(st) {
        *hint_said = true;
        if cfg.bridge.log_json {
            tracing::warn!("{PORT_HINT}");
        } else {
            // The status display prints it on the terminal.
            tracing::info!("{PORT_HINT}");
        }
    }
}

/// The `xkr-p2p-hyper` name of a network.
#[must_use]
pub fn xkr_network(network: Network) -> XkrNetwork {
    match network {
        Network::Mainnet => XkrNetwork::Mainnet,
        Network::Testnet => XkrNetwork::Testnet,
    }
}

/// The topic xkrd announces: `cn_fast_hash("xkr/<network>/blocks/v1")`.
#[must_use]
pub fn blocks_topic(network: Network) -> Key {
    topic::xkr_blocks_topic(xkr_network(network))
}

/// The stripe topics this node announces (server mode): every stripe of [`DEFAULT_STRIPES`] for
/// a full node, its own stripe under its own count (`seed.stripes()`, 4, 6, ...) for a
/// stripe-pruned one, none for a completely pruned one ([`PruningSeed::RECENT_ONLY`]: it
/// holds no old stripe). Full nodes need no other count: every lookup includes the default
/// count ([`stripe_lookup_topics`]).
#[must_use]
pub fn held_stripe_topics(network: Network, seed: PruningSeed) -> Vec<Key> {
    let net = xkr_network(network);
    if seed.is_full() {
        topic::xkr_stripe_topics(net, DEFAULT_STRIPES)
    } else {
        stripe_topic(network, seed).into_iter().collect()
    }
}

/// The topic of the stripe a stripe `seed` names; `None` for the full and the
/// [`PruningSeed::RECENT_ONLY`] seed.
#[must_use]
pub fn stripe_topic(network: Network, seed: PruningSeed) -> Option<Key> {
    seed.is_stripe()
        .then(|| topic::xkr_stripe_topic(xkr_network(network), seed.stripes(), seed.stripe()))
}

/// The topics to look up for the holders of `height`: the topic of its stripe under every
/// lookup count ([`stripe_holders_for`]: stripe `k` of 4 and stripe `j` of 6, which differ),
/// with the seed each names. Full nodes are on the default-count topic, pruned nodes on the
/// topic of their own count.
#[must_use]
pub fn stripe_lookup_topics(network: Network, height: u64) -> Vec<(PruningSeed, Key)> {
    stripe_holders_for(height)
        .into_iter()
        .filter_map(|seed| Some((seed, stripe_topic(network, seed)?)))
        .collect()
}

fn short(key: &Key) -> String {
    hex::encode(&key[..6])
}

// ---------------------------------------------------------------------------------------------
// Hugin relay (`--hugin`, on without mode flags, `--no-hugin`; crate `xkr-hugin`)

/// Subdirectory of the data directory with the Hugin identity and message pool.
pub const HUGIN_DIR: &str = "hugin";
const PRIVATE_LABEL: &str = "hugin private node";
const PUBLIC_LABEL: &str = "hugin public nodes";

/// The relay settings (`--hugin-*`).
#[derive(Debug, Clone)]
pub struct HuginSettings {
    /// `<data dir>/hugin`: `identity.json`, `messages.jsonl`.
    pub dir: PathBuf,
    /// `--hugin-max-clients`.
    pub max_clients: usize,
    /// `--hugin-max-clients-per-ip` (0: no limit).
    pub max_clients_per_ip: usize,
    /// `--hugin-sync-slots` (`RelayConfig::sync_slots`).
    pub sync_slots: usize,
    /// `--hugin-queue-budget-mb` in bytes (`RelayConfig::queue_budget`).
    pub queue_budget: usize,
    /// p2pool stratum nodes in order (`--hugin-p2pool`, used as given; else, with p2pool on,
    /// the built-in p2pool's stratum address and the public ones,
    /// [`crate::p2pool::relay_nodes`]; else the local one and the public ones,
    /// [`default_p2pool_nodes`]).
    pub p2pool_nodes: Vec<String>,
    /// `--hugin-share-difficulty`: the app difficulty (`RelayConfig::app_difficulty`).
    pub share_difficulty: u64,
    /// `--hugin-private` (or `--private`): no public-node topic.
    pub private_only: bool,
    /// `--private`: the node mesh client-only and no address-based private topic (apps come
    /// through the private node's invite).
    pub private_node: bool,
    /// Concurrent share hashes (`RelayConfig::max_concurrent_hashes`): half of
    /// `--verify-threads`, at least 1, so Hugin PoW checks never take the cores block
    /// verification needs.
    pub hash_threads: usize,
    /// The fee address handle (set by `main` once it is loaded; `None`: shares are not
    /// submitted).
    pub fee: Option<Arc<FeeAddress>>,
}

impl HuginSettings {
    fn from_args(args: &Args, data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join(HUGIN_DIR),
            max_clients: args.hugin_max_clients.max(1),
            max_clients_per_ip: args.hugin_max_clients_per_ip,
            sync_slots: args.hugin_sync_slots.max(1),
            queue_budget: args
                .hugin_queue_budget_mb
                .max(1)
                .saturating_mul(1024 * 1024),
            p2pool_nodes: if args.hugin_p2pool.is_empty() {
                if args.p2pool_enabled() {
                    // The built-in p2pool's own stratum server: known to be ours, no port check.
                    crate::p2pool::relay_nodes(args.p2pool_stratum)
                } else {
                    default_p2pool_nodes()
                }
            } else {
                args.hugin_p2pool.clone()
            },
            share_difficulty: args.hugin_share_difficulty.max(1),
            private_only: args.hugin_private || args.private,
            private_node: args.private,
            hash_threads: (args.verify_thread_count() / 2).max(1),
            fee: None,
        }
    }

    /// The identity file, which also holds the fee address.
    #[must_use]
    pub fn identity_file(&self) -> PathBuf {
        self.dir.join(xkr_hugin::identity::IDENTITY_FILE)
    }
}

/// The default p2pool nodes: the local p2pool, then the public ones; on Linux the local one is
/// left out, with one warning, when another user holds its port
/// (`xkr_hugin::p2pool::default_nodes_checked`).
fn default_p2pool_nodes() -> Vec<String> {
    let (nodes, owner) = xkr_hugin::p2pool::default_nodes_checked();
    if let xkr_hugin::p2pool::LocalPortOwner::Other(uid) = owner {
        tracing::warn!(
            endpoint = xkr_hugin::p2pool::LOCAL_P2POOL,
            uid,
            "the local p2pool port belongs to another user; Hugin does not use it (--hugin-p2pool \
             names the p2pool nodes to use)"
        );
    }
    nodes
}

/// hugin-node's DHT seed for its client-facing swarms, `fill32(hex(cn_fast_hash(viewkey)))`
/// (`create_keys_from_seed(await hash(NodeId.viewkey))`). We use it for the private-node topic,
/// so an imported hugin-node `config.json` keeps its invite (address + DHT public key).
fn hugin_private_seed(view_key_hex: &str) -> [u8; 32] {
    topic::fill32(topic::hugin_hash_key(view_key_hex).as_bytes())
}

/// One Hugin topic the node joins.
#[derive(Debug, Clone)]
struct HuginJoin {
    label: &'static str,
    topic: HuginTopic,
    mode: AnnounceMode,
    /// Who connects on it.
    role: Role,
    mid: HuginMid,
    /// Announced only once the node is synchronized (the public-node topic: apps that find
    /// us there want jobs).
    after_sync: bool,
}

/// hugin-node's three swarms: node mesh (server + client, other relays), public nodes (server,
/// any Hugin app; skipped for a private node) and private node (server, apps with the invite).
/// The bridge gives each its own DHT identity, since a `mid` signs one key. On the testnet the
/// topics are the testnet ones (`HuginTopic::on_network`), so testnet relays never meet mainnet
/// apps. With `private_node` (`--private`) only the node mesh is joined, client-only; apps
/// reach the relay through the private node's own Hugin topic ([`PrivateNode`]).
fn hugin_topics(
    network: Network,
    address: &str,
    view_key_hex: &str,
    private_only: bool,
    private_node: bool,
) -> Vec<HuginJoin> {
    let net = xkr_network(network);
    let mesh = HuginTopic::node_mesh().on_network(net);
    let mut out = vec![HuginJoin {
        label: "hugin node mesh",
        mid: mesh.mid(),
        topic: mesh,
        mode: if private_node {
            AnnounceMode::Client
        } else {
            AnnounceMode::Both
        },
        role: Role::Node,
        after_sync: false,
    }];
    if private_node {
        return out;
    }
    if !private_only {
        let public = HuginTopic::public_nodes().on_network(net);
        out.push(HuginJoin {
            label: PUBLIC_LABEL,
            mid: public.mid(),
            topic: public,
            mode: AnnounceMode::Server,
            role: Role::Client,
            after_sync: true,
        });
    }
    let private = HuginTopic::private_node(address).on_network(net);
    let mut mid = private.mid();
    mid.identity_seed = Some(hugin_private_seed(view_key_hex));
    out.push(HuginJoin {
        label: PRIVATE_LABEL,
        mid,
        topic: private,
        mode: AnnounceMode::Server,
        role: Role::Client,
        after_sync: false,
    });
    out
}

/// Whether the node is synchronized enough to serve Hugin apps: what the built-in miner waits
/// for (`synced` and not downloading).
pub(crate) fn synced_now(net: &NetStatus) -> bool {
    net.synced && !net.downloading
}

/// The relay and its topics; lives across bridge restarts.
#[derive(Debug)]
struct HuginRuntime {
    relay: Relay,
    address: String,
    topics: Vec<HuginJoin>,
    /// `true` while the node is synchronized.
    synced: watch::Receiver<bool>,
}

impl HuginRuntime {
    /// Loads the identity, starts the p2pool client, the relay and the tasks that follow the
    /// sync state, the fee address and the p2pool connection.
    fn start(
        s: &HuginSettings,
        network: Network,
        stats: Arc<HuginStats>,
        shutdown: &CancellationToken,
        net: Option<&watch::Receiver<Arc<NetStatus>>>,
    ) -> Result<Self, String> {
        let identity = NodeIdentity::load_or_create(&s.identity_file())
            .map_err(|e| format!("Hugin identity: {e}"))?;
        let synced = follow_sync(net.cloned(), shutdown);
        let fee = s.fee.as_ref().map(|f| f.get());
        let fee_address = fee.as_ref().and_then(|f| f.address.clone());
        let source = P2poolSource::spawn(
            P2poolConfig {
                nodes: s.p2pool_nodes.clone(),
                fee_address: fee_address.clone(),
                app_difficulty: s.share_difficulty,
                agent: format!("xkrd/{}", env!("CARGO_PKG_VERSION")),
            },
            synced.clone(),
            follow_tip(net.cloned(), shutdown),
            shutdown,
        );
        stats.set_fee(FeeInfo {
            address: fee_address.clone(),
            donation: fee.as_ref().is_some_and(|f| f.donation),
        });
        if fee_address.is_none() {
            tracing::warn!("no fee address: Hugin shares are not submitted");
        }
        if let Some(f) = &s.fee {
            tokio::spawn(follow_fee(
                f.subscribe(),
                source.clone(),
                stats.clone(),
                shutdown.clone(),
            ));
        }
        tokio::spawn(follow_p2pool(
            source.status_changes(),
            stats.clone(),
            shutdown.clone(),
        ));
        let mut cfg = RelayConfig::new(identity.address.clone());
        cfg.max_clients = s.max_clients;
        cfg.max_clients_per_ip = s.max_clients_per_ip;
        cfg.sync_slots = s.sync_slots;
        cfg.queue_budget = s.queue_budget;
        cfg.app_difficulty = s.share_difficulty;
        cfg.data_dir = Some(s.dir.clone());
        cfg.max_concurrent_hashes = s.hash_threads;
        if s.share_difficulty < GOSSIP_MIN_DIFFICULTY {
            tracing::warn!(
                share_difficulty = s.share_difficulty,
                min = GOSSIP_MIN_DIFFICULTY,
                "Hugin app difficulty below the gossip floor: other relays drop these messages"
            );
        }
        let relay = Relay::new(cfg, source, stats, shutdown.clone())
            .map_err(|e| format!("Hugin message pool: {e}"))?;
        tracing::info!(
            address = %identity.address,
            fee = %fee_address.as_deref().unwrap_or("none"),
            p2pool = %s.p2pool_nodes.join(","),
            app_difficulty = s.share_difficulty,
            p2pool_difficulty = xkr_hugin::p2pool::login_difficulty(s.share_difficulty),
            max_clients_per_ip = s.max_clients_per_ip,
            sync_slots = s.sync_slots,
            queue_budget = s.queue_budget,
            messages = relay.pool_len(),
            "Hugin relay ready"
        );
        Ok(Self {
            relay,
            topics: hugin_topics(
                network,
                &identity.address,
                &identity.view_key_hex(),
                s.private_only,
                s.private_node,
            ),
            address: identity.address,
            synced,
        })
    }

    fn role_of(&self, topic: &Key) -> Option<Role> {
        self.topics
            .iter()
            .find(|t| t.topic.topic == *topic)
            .map(|t| t.role)
    }
}

/// A `bool` watch channel that follows [`synced_now`] of the node's P2P status (always `true`
/// without one, e.g. in tests). Stops with `shutdown`.
fn follow_sync(
    net: Option<watch::Receiver<Arc<NetStatus>>>,
    shutdown: &CancellationToken,
) -> watch::Receiver<bool> {
    let Some(mut net) = net else {
        return watch::channel(true).1;
    };
    let (tx, rx) = watch::channel(synced_now(&net.borrow_and_update()));
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let now = synced_now(&net.borrow_and_update());
            if tx.send_if_modified(|v| std::mem::replace(v, now) != now) {
                tracing::info!(
                    synced = now,
                    "{}",
                    if now {
                        "node synchronized: Hugin jobs go to apps"
                    } else {
                        "node behind: no Hugin jobs until it is synchronized"
                    }
                );
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                r = net.changed() => if r.is_err() { return },
            }
        }
    });
    rx
}

/// The node's main-chain tip from its P2P status (the feed [`follow_sync`] reads), with which
/// the relay dates its jobs; `None` without a status (e.g. in tests). Stops with `shutdown`.
fn follow_tip(
    net: Option<watch::Receiver<Arc<NetStatus>>>,
    shutdown: &CancellationToken,
) -> watch::Receiver<Option<ChainTip>> {
    let Some(mut net) = net else {
        return watch::channel(None).1;
    };
    let tip_of = |n: &NetStatus| ChainTip {
        height: n.top_index,
        hash: n.top_hash,
    };
    let (tx, rx) = watch::channel(Some(tip_of(&net.borrow_and_update())));
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let now = Some(tip_of(&net.borrow_and_update()));
            tx.send_if_modified(|v| std::mem::replace(v, now) != now);
            tokio::select! {
                () = shutdown.cancelled() => return,
                r = net.changed() => if r.is_err() { return },
            }
        }
    });
    rx
}

/// Applies every new fee address to the p2pool login and the status.
async fn follow_fee(
    mut fee: watch::Receiver<Fee>,
    source: Arc<P2poolSource>,
    stats: Arc<HuginStats>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            r = fee.changed() => if r.is_err() { return },
        }
        let f = fee.borrow_and_update().clone();
        source.set_fee_address(f.address.as_deref());
        stats.set_fee(FeeInfo {
            address: f.address.clone(),
            donation: f.donation,
        });
        tracing::info!(fee = %f.address.as_deref().unwrap_or("none"), "Hugin fee address changed");
    }
}

/// Keeps the p2pool connection in the status, and logs (info) once per outage that no p2pool
/// node is reachable: the built-in p2pool starts only once the node is synchronized, so this is
/// the normal state while syncing; `status` and `/status` show the connection.
async fn follow_p2pool(
    mut pool_rx: watch::Receiver<PoolStatus>,
    stats: Arc<HuginStats>,
    shutdown: CancellationToken,
) {
    let mut warned = false;
    loop {
        let s = pool_rx.borrow_and_update().clone();
        stats.set_p2pool(P2poolInfo {
            host: s.pool.clone(),
            connected: s.connected,
        });
        if s.unreachable && !warned {
            warned = true;
            tracing::info!("{NO_P2POOL}");
        } else if s.connected && warned {
            warned = false;
            tracing::info!(pool = %s.pool.as_deref().unwrap_or("?"), "p2pool reached: Hugin shares are submitted again");
        } else if s.connected {
            tracing::info!(pool = %s.pool.as_deref().unwrap_or("?"), "Hugin relay logged in to p2pool");
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            r = pool_rx.changed() => if r.is_err() { return },
        }
    }
}

/// What xkrd says when no p2pool node can be reached.
pub const NO_P2POOL: &str = "no p2pool reachable — Hugin shares are not submitted";

// ---------------------------------------------------------------------------------------------
// handing connections to the P2P manager

/// Why a Levin handshake over a Hyperswarm connection failed, as far as bans are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// Another network or a too old P2P version.
    Incompatible,
    /// We connected to ourselves.
    Ourselves,
    /// Levin protocol violation (`PeerError::is_protocol_violation`).
    Protocol,
    /// I/O, timeouts, closed, queue full: no ban (a deduplicated connection fails like this).
    Transient,
}

impl FailureKind {
    fn ban(self) -> Option<Duration> {
        match self {
            FailureKind::Incompatible => Some(BAN_INCOMPATIBLE),
            FailureKind::Ourselves => Some(BAN_SELF),
            FailureKind::Protocol => Some(BAN_PROTOCOL),
            FailureKind::Transient => None,
        }
    }
}

/// Finds a [`PeerError`] in `err` or its `source()` chain.
fn classify<E: std::error::Error + 'static>(err: &E) -> FailureKind {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(pe) = e.downcast_ref::<PeerError>() {
            return match pe {
                PeerError::WrongNetwork(_) | PeerError::VersionTooOld(_) => {
                    FailureKind::Incompatible
                }
                PeerError::SelfConnection => FailureKind::Ourselves,
                // Same notion of misbehaviour as for TCP peers.
                other if other.is_protocol_violation() => FailureKind::Protocol,
                _ => FailureKind::Transient,
            };
        }
        cur = e.source();
    }
    FailureKind::Transient
}

#[derive(Debug)]
struct HandoffError {
    kind: FailureKind,
    message: String,
}

/// The one call into the P2P manager: Levin handshake over `stream` (we send the handshake when
/// `outbound`, i.e. when we are the Hyperswarm initiator) and registration of the peer in the
/// same peer set, downloader and relay as TCP peers.
async fn hand_off(
    p2p: &P2pHandle,
    stream: BoxedPeerStream,
    origin: PeerOrigin,
    outbound: bool,
) -> Result<(), HandoffError> {
    if let Err(e) = p2p.add_stream(stream, origin, outbound).await {
        return Err(HandoffError {
            kind: classify(&e),
            message: e.to_string(),
        });
    }
    Ok(())
}

/// Tells the session when the P2P layer dropped a connection.
#[derive(Debug)]
struct DropGuard {
    conn_id: u64,
    events: mpsc::UnboundedSender<Event>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Dropped {
            conn_id: self.conn_id,
        });
    }
}

/// The stream handed to the P2P manager: a [`HyperConn`] that reports its drop.
#[derive(Debug)]
struct TrackedConn {
    conn: HyperConn,
    _guard: DropGuard,
}

impl AsyncRead for TrackedConn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().conn).poll_read(cx, buf)
    }
}

impl AsyncWrite for TrackedConn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().conn).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().conn).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().conn).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------------------------
// peer bookkeeping (pure, unit-tested)

/// What the tracker needs to know about one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConnMeta {
    conn_id: u64,
    key: Key,
    /// We dialed (Hyperswarm/Noise initiator); we then send the Levin handshake.
    initiator: bool,
    /// Noise handshake hash, identical on both ends: the deterministic tie-break.
    handshake_hash: [u8; 64],
    /// The remote's address as the bridge sees it ([`address_key`]), if known.
    ip: Option<IpAddr>,
    /// It came in on the private node's sync identity (`--private`), so the remote holds the
    /// invite.
    private: bool,
}

impl ConnMeta {
    fn of(conn: &HyperConn) -> Self {
        let i = conn.info();
        Self {
            conn_id: i.conn_id,
            key: i.remote_public_key,
            initiator: i.initiator,
            handshake_hash: i.handshake_hash,
            ip: i.addr.map(address_key),
            private: false,
        }
    }
}

/// The key under which connections per address are counted: the IPv4 address, or the /64 of
/// an IPv6 address.
pub(crate) fn address_key(addr: SocketAddr) -> IpAddr {
    match addr.ip().to_canonical() {
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(o))
        }
        v4 @ IpAddr::V4(_) => v4,
    }
}

#[derive(Debug, Clone, Copy)]
struct Live {
    meta: ConnMeta,
    /// The Levin handshake succeeded.
    established: bool,
    /// When the connection (or the pending one it replaced) was admitted.
    since: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    Ourselves,
    Banned,
    Duplicate,
    /// `--hyper-max-peers` reached, or the inbound share of it.
    Full,
    /// [`MAX_PENDING_HANDSHAKES`] outbound Levin handshakes are running.
    Busy,
    /// [`MAX_INBOUND_PER_IP`] or [`MAX_PENDING_INBOUND_PER_IP`] reached for the address.
    PerAddress,
    /// A private node (`--private`) takes inbound connections only on its sync identity.
    NotInvited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admit {
    Accept,
    /// Accept and close the older connection to the same key.
    Replace {
        old: u64,
    },
    /// Accept and close the inbound connection whose handshake was pending longest.
    AcceptEvicting {
        evicted: u64,
    },
    Reject(RejectReason),
}

/// Which of two connections to the same remote key to keep; both ends must decide alike.
///
/// Mirrors Hyperswarm (`_handleServerConnection`), which the bridge applies to its own swarm
/// connections: if the existing connection is established, the new one wins (the remote lost the
/// old one and reconnected; nobody dials a key it is connected to). Otherwise the connection
/// initiated by the peer with the higher public key wins; if both or neither were, the lower
/// Noise handshake hash (the same on both ends) wins.
fn prefer_new(local: &Key, old: &Live, new: &ConnMeta) -> bool {
    if old.established {
        return true;
    }
    let local_initiates = *local > new.key;
    let old_expected = old.meta.initiator == local_initiates;
    let new_expected = new.initiator == local_initiates;
    if old_expected == new_expected {
        new.handshake_hash < old.meta.handshake_hash
    } else {
        new_expected
    }
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    attempts: u32,
    until: Instant,
}

fn dial_backoff(attempts: u32) -> Duration {
    Duration::from_secs(DIAL_BACKOFF_BASE_SECS << attempts.min(8)).min(DIAL_BACKOFF_MAX)
}

/// Before `key` is added to `map`: at [`MAX_TRACKED_KEYS`] entries, the one with the earliest
/// `end` goes (nothing goes for a key already there).
pub(crate) fn make_room<V>(map: &mut HashMap<Key, V>, key: &Key, end: impl Fn(&V) -> Instant) {
    if map.len() < MAX_TRACKED_KEYS || map.contains_key(key) {
        return;
    }
    if let Some(first) = map.iter().min_by_key(|(_, v)| end(v)).map(|(k, _)| *k) {
        map.remove(&first);
    }
}

/// Hyperswarm peers of the node: one live connection per remote key, dials, bans, back-off.
/// Bans and back-off survive bridge restarts; connections do not.
#[derive(Debug)]
struct Tracker {
    local_key: Key,
    max_peers: usize,
    /// `--private`: inbound connections only on the private sync identity.
    invite_only: bool,
    live: HashMap<Key, Live>,
    by_id: HashMap<u64, Key>,
    dialing: HashSet<Key>,
    /// Connections we closed on purpose (duplicates, shutdown): their failure is no offence.
    closing: HashMap<u64, Instant>,
    bans: HashMap<Key, Instant>,
    backoff: HashMap<Key, Backoff>,
    /// Keys of the previous lookup: explicit dials only go to peers seen twice.
    seen_prev: HashSet<Key>,
    /// Picks which keys of a stripe lookup are dialed ([`Self::sample_stripe_keys`]).
    rng: SystemRng,
}

impl Tracker {
    fn new(max_peers: usize) -> Self {
        Self {
            local_key: [0; 32],
            max_peers: max_peers.max(1),
            invite_only: false,
            live: HashMap::new(),
            by_id: HashMap::new(),
            dialing: HashSet::new(),
            closing: HashMap::new(),
            bans: HashMap::new(),
            backoff: HashMap::new(),
            seen_prev: HashSet::new(),
            rng: SystemRng,
        }
    }

    /// A new bridge session: every connection of the old one is gone.
    fn start_session(&mut self, local_key: Key) {
        self.local_key = local_key;
        self.live.clear();
        self.by_id.clear();
        self.dialing.clear();
        self.closing.clear();
        self.seen_prev.clear();
    }

    fn connected(&self) -> usize {
        self.live.len()
    }

    fn out_target(&self) -> usize {
        self.max_peers.min(OUT_TARGET)
    }

    /// Slots of `--hyper-max-peers` only connections we initiate may take: the outbound
    /// target, at most half of the cap.
    fn out_reserve(&self) -> usize {
        self.out_target().min(self.max_peers / 2)
    }

    /// Most inbound connections (pending and established).
    fn inbound_cap(&self) -> usize {
        self.max_peers - self.out_reserve()
    }

    /// Live connections we initiated.
    fn outbound(&self) -> usize {
        self.live.values().filter(|l| l.meta.initiator).count()
    }

    /// Our own connections (with dials in flight) are below the outbound target.
    fn wants_more(&self) -> bool {
        self.outbound() + self.dialing.len() < self.out_target()
    }

    fn is_banned(&mut self, key: &Key, now: Instant) -> bool {
        match self.bans.get(key) {
            Some(until) if *until > now => true,
            Some(_) => {
                self.bans.remove(key);
                false
            }
            None => false,
        }
    }

    /// Admitted connections of one direction whose Levin handshake has not finished.
    fn pending(&self, initiator: bool) -> usize {
        self.live
            .values()
            .filter(|l| !l.established && l.meta.initiator == initiator)
            .count()
    }

    /// Inbound connections from `ip` (pending only with `pending_only`).
    fn inbound_from(&self, ip: IpAddr, pending_only: bool) -> usize {
        self.live
            .values()
            .filter(|l| {
                !l.meta.initiator && l.meta.ip == Some(ip) && !(pending_only && l.established)
            })
            .count()
    }

    /// The inbound connection whose handshake has been pending longest.
    fn oldest_pending_inbound(&self) -> Option<Live> {
        self.live
            .values()
            .filter(|l| !l.established && !l.meta.initiator)
            .min_by_key(|l| (l.since, l.meta.conn_id))
            .copied()
    }

    /// Admission of a new connection.
    ///
    /// * A private node (`invite_only`) refuses inbound connections that did not come in on
    ///   its sync identity.
    /// * One live connection per key ([`prefer_new`]); a replaced pending connection passes
    ///   its admission time on.
    /// * Outbound: up to `--hyper-max-peers` and [`MAX_PENDING_HANDSHAKES`] pending.
    /// * Inbound: up to [`Self::inbound_cap`], [`MAX_INBOUND_PER_IP`] and
    ///   [`MAX_PENDING_INBOUND_PER_IP`] per address; beyond [`MAX_PENDING_INBOUND`] pending, the
    ///   one pending longest is closed.
    fn admit(&mut self, c: ConnMeta, now: Instant) -> Admit {
        if self.invite_only && !c.initiator && !c.private {
            return Admit::Reject(RejectReason::NotInvited);
        }
        if c.key == self.local_key {
            return Admit::Reject(RejectReason::Ourselves);
        }
        if self.is_banned(&c.key, now) {
            return Admit::Reject(RejectReason::Banned);
        }
        if let Some(old) = self.live.get(&c.key).copied() {
            if !prefer_new(&self.local_key, &old, &c) {
                return Admit::Reject(RejectReason::Duplicate);
            }
            // Replacing an established connection adds a pending one.
            if old.established {
                let cap = if c.initiator {
                    MAX_PENDING_HANDSHAKES
                } else {
                    MAX_PENDING_INBOUND
                };
                if self.pending(c.initiator) >= cap {
                    return Admit::Reject(RejectReason::Busy);
                }
            }
            self.by_id.remove(&old.meta.conn_id);
            self.closing.insert(old.meta.conn_id, now);
            self.insert(c, if old.established { now } else { old.since });
            return Admit::Replace {
                old: old.meta.conn_id,
            };
        }
        if self.live.len() >= self.max_peers {
            return Admit::Reject(RejectReason::Full);
        }
        if c.initiator {
            if self.pending(true) >= MAX_PENDING_HANDSHAKES {
                return Admit::Reject(RejectReason::Busy);
            }
            self.insert(c, now);
            return Admit::Accept;
        }
        if self.live.len() - self.outbound() >= self.inbound_cap() {
            return Admit::Reject(RejectReason::Full);
        }
        if let Some(ip) = c.ip
            && (self.inbound_from(ip, false) >= MAX_INBOUND_PER_IP
                || self.inbound_from(ip, true) >= MAX_PENDING_INBOUND_PER_IP)
        {
            return Admit::Reject(RejectReason::PerAddress);
        }
        let evicted = if self.pending(false) >= MAX_PENDING_INBOUND {
            let Some(old) = self.oldest_pending_inbound() else {
                return Admit::Reject(RejectReason::Busy);
            };
            self.live.remove(&old.meta.key);
            self.by_id.remove(&old.meta.conn_id);
            self.closing.insert(old.meta.conn_id, now);
            Some(old.meta.conn_id)
        } else {
            None
        };
        self.insert(c, now);
        match evicted {
            Some(evicted) => Admit::AcceptEvicting { evicted },
            None => Admit::Accept,
        }
    }

    fn insert(&mut self, c: ConnMeta, since: Instant) {
        self.live.insert(
            c.key,
            Live {
                meta: c,
                established: false,
                since,
            },
        );
        self.by_id.insert(c.conn_id, c.key);
    }

    /// We are closing `conn_id` on purpose (shutdown).
    fn closing_on_purpose(&mut self, conn_id: u64, now: Instant) {
        self.closing.insert(conn_id, now);
    }

    /// The P2P layer dropped the connection. True if it was the live one for its key.
    fn on_dropped(&mut self, conn_id: u64) -> bool {
        let Some(key) = self.by_id.remove(&conn_id) else {
            return false;
        };
        if self
            .live
            .get(&key)
            .is_some_and(|l| l.meta.conn_id == conn_id)
        {
            self.live.remove(&key);
            return true;
        }
        false
    }

    /// Result of the Levin handshake; returns the ban applied, if any.
    fn on_handoff(
        &mut self,
        meta: ConnMeta,
        result: Result<(), FailureKind>,
        now: Instant,
    ) -> Option<Duration> {
        match result {
            Ok(()) => {
                self.backoff.remove(&meta.key);
                if let Some(l) = self.live.get_mut(&meta.key)
                    && l.meta.conn_id == meta.conn_id
                {
                    l.established = true;
                }
                None
            }
            Err(kind) => {
                if self.closing.remove(&meta.conn_id).is_some() {
                    return None;
                }
                if meta.initiator {
                    self.bump_backoff(meta.key, now);
                }
                let ban = kind.ban()?;
                make_room(&mut self.bans, &meta.key, |until| *until);
                self.bans.insert(meta.key, now + ban);
                Some(ban)
            }
        }
    }

    fn bump_backoff(&mut self, key: Key, now: Instant) {
        make_room(&mut self.backoff, &key, |b| b.until);
        let b = self.backoff.entry(key).or_insert(Backoff {
            attempts: 0,
            until: now,
        });
        b.until = now + dial_backoff(b.attempts);
        b.attempts = b.attempts.saturating_add(1);
    }

    /// Peers from a lookup worth an explicit dial (marked as dialing). Only peers that were also
    /// in the previous lookup: Hyperswarm auto-dials what it finds, so a peer still unconnected
    /// one round later needs our help.
    fn dial_candidates(&mut self, found: &[Key], now: Instant) -> Vec<Key> {
        let prev = std::mem::replace(&mut self.seen_prev, found.iter().copied().collect());
        let mut out = Vec::new();
        for key in found {
            if !self.wants_more() || self.dialing.len() >= MAX_CONCURRENT_DIALS {
                break;
            }
            if !prev.contains(key)
                || *key == self.local_key
                || self.live.contains_key(key)
                || self.dialing.contains(key)
                || self.is_banned(key, now)
                || self.backoff.get(key).is_some_and(|b| b.until > now)
            {
                continue;
            }
            self.dialing.insert(*key);
            out.push(*key);
        }
        out
    }

    /// Peers from a stripe lookup to dial now (marked as dialing). The downloader needs a stripe
    /// no connected peer holds, and nobody auto-dials stripe topics (they are joined in server
    /// mode only), so the two-sightings rule of [`Self::dial_candidates`] does not apply. Bounded
    /// by `--hyper-max-peers` (not the outbound target: the stripe is needed even when the
    /// target is reached), [`MAX_CONCURRENT_DIALS`] and [`STRIPE_DIALS`].
    fn stripe_dial_candidates(&mut self, found: &[Key], now: Instant) -> Vec<Key> {
        let mut out = Vec::new();
        for key in found {
            if out.len() >= STRIPE_DIALS
                || self.dialing.len() >= MAX_CONCURRENT_DIALS
                || self.at_max_peers()
            {
                break;
            }
            if !self.dialable(key, now) {
                continue;
            }
            self.dialing.insert(*key);
            out.push(*key);
        }
        out
    }

    /// The `--private-peer` keys to dial now (marked as dialing): every one that is
    /// [`Self::dialable`]. There are few, and the node stays connected to each, so neither the
    /// outbound target nor the dial limits apply; failures back off as usual.
    fn private_dial_candidates(&mut self, keys: &[Key], now: Instant) -> Vec<Key> {
        let mut out = Vec::new();
        for key in keys {
            if self.dialable(key, now) {
                self.dialing.insert(*key);
                out.push(*key);
            }
        }
        out
    }

    /// Connections and dials fill `--hyper-max-peers`.
    fn at_max_peers(&self) -> bool {
        self.live.len() + self.dialing.len() >= self.max_peers
    }

    /// Established connections we dialed (the outbound peers a stripe rotation may replace).
    fn outbound_established(&self) -> usize {
        self.live
            .values()
            .filter(|l| l.established && l.meta.initiator)
            .count()
    }

    /// The keys of a stripe lookup in random order, so the [`STRIPE_DIALS`] dialed (or waiting
    /// for a slot) are a random sample, not the first keys of the DHT's answer.
    fn sample_stripe_keys(&mut self, found: &[Key]) -> Vec<Key> {
        let mut keys = found.to_vec();
        self.rng.shuffle(&mut keys);
        keys
    }

    /// Not ourselves, not connected or being dialed, not banned, not backing off.
    fn dialable(&mut self, key: &Key, now: Instant) -> bool {
        *key != self.local_key
            && !self.live.contains_key(key)
            && !self.dialing.contains(key)
            && !self.is_banned(key, now)
            && self.backoff.get(key).is_none_or(|b| b.until <= now)
    }

    /// Keys from a stripe lookup that would be dialed but for `--hyper-max-peers` (at most
    /// [`STRIPE_DIALS`], not marked as dialing); empty below the cap. Non-empty means a
    /// connected outbound peer without the stripe has to make room (stripe rotation), so it is
    /// also empty unless our own established outbound connections fill the outbound target:
    /// inbound connections and dials in flight, which others control, never trigger a rotation.
    fn blocked_stripe_dials(&mut self, found: &[Key], now: Instant) -> Vec<Key> {
        let mut out = Vec::new();
        if !self.at_max_peers() || self.outbound_established() < self.out_target() {
            return out;
        }
        for key in found {
            if out.len() >= STRIPE_DIALS {
                break;
            }
            if self.dialable(key, now) {
                out.push(*key);
            }
        }
        out
    }

    fn on_dial_result(&mut self, key: Key, ok: bool, now: Instant) {
        self.dialing.remove(&key);
        if !ok {
            self.bump_backoff(key, now);
        }
    }

    /// Forgets expired bans, stale back-off and old purposeful closes.
    fn prune(&mut self, now: Instant) {
        self.bans.retain(|_, until| *until > now);
        self.backoff.retain(|_, b| b.until + DIAL_BACKOFF_MAX > now);
        self.closing
            .retain(|_, at| now.saturating_duration_since(*at) < CLOSING_MEMORY);
    }

    fn lookup_period(&self) -> Duration {
        if self.live.is_empty() {
            LOOKUP_EMPTY
        } else if self.wants_more() {
            LOOKUP_LOW
        } else {
            LOOKUP_FULL
        }
    }
}

// ---------------------------------------------------------------------------------------------
// supervisor and sessions

#[derive(Debug)]
enum Event {
    /// The topic is announced (with the local key used).
    Announced(Option<Key>),
    Looked(Result<Vec<PeerAddr>, BridgeError>),
    Dialed {
        key: Key,
        result: Result<HyperConn, BridgeError>,
    },
    HandedOff {
        meta: ConnMeta,
        result: Result<(), HandoffError>,
    },
    Dropped {
        conn_id: u64,
    },
    /// The stripe topics we hold are announced.
    StripesAnnounced(usize),
    /// A stripe-topic lookup finished.
    StripeLooked {
        topic: Key,
        result: Result<Vec<PeerAddr>, BridgeError>,
    },
    /// A Hugin topic is announced (with the identity key used).
    HuginAnnounced {
        label: &'static str,
        key: Option<Key>,
    },
    /// A Hugin relay connection ended.
    HuginDone {
        conn_id: u64,
    },
    /// The built-in p2pool's sidechain topic is announced (with the identity key used).
    P2poolAnnounced(Option<Key>),
    /// A topic of the private node is announced (with the identity key used).
    PrivateAnnounced {
        label: &'static str,
        key: Option<Key>,
    },
    /// A clients topic is announced (with the identity key used).
    ClientsAnnounced {
        label: &'static str,
        key: Option<Key>,
    },
    /// A client connection ended.
    ClientsDone {
        conn_id: u64,
    },
}

enum Step {
    Shutdown,
    Failed(String),
    Conn(HyperConn),
    Event(Event),
    Lookup,
    /// The wanted height changed (`false`: the P2P manager is gone).
    Wanted(bool),
    /// Periodic retry of a stripe that is still missing.
    StripeTick,
    /// The worker sent a `DhtStatus` (`false`: the bridge is gone).
    Dht(bool),
    /// The private node has a new secret (`false`: its handle is gone).
    Renewed(bool),
}

enum SessionEnd {
    Shutdown,
    Failed(String),
    /// The private node has a new secret: start the bridge again at once.
    Renewed,
}

/// Waits for a new private secret (`false` once the handle is gone); never resolves without
/// one.
async fn renewed(rx: Option<&mut watch::Receiver<PrivateSecret>>) -> bool {
    match rx {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Runs the Hyperswarm transport until `shutdown` is cancelled.
pub async fn run(cfg: HyperConfig, p2p: P2pHandle, shutdown: CancellationToken) {
    let mut tracker = Tracker::new(cfg.max_peers);
    tracker.invite_only = cfg.private.is_some();
    Supervisor {
        cfg,
        p2p,
        shutdown,
        tracker,
        hugin: None,
        port_hint_said: false,
    }
    .run()
    .await;
}

struct Supervisor {
    cfg: HyperConfig,
    p2p: P2pHandle,
    shutdown: CancellationToken,
    tracker: Tracker,
    hugin: Option<HuginRuntime>,
    /// [`PORT_HINT`] was said (once per process).
    port_hint_said: bool,
}

impl Supervisor {
    async fn run(mut self) {
        let topic = blocks_topic(self.cfg.network);
        let seed = self.p2p.pruning_seed();
        tracing::info!(
            topic = %hex::encode(topic),
            workers = self.cfg.bare_config().workers,
            udp_port = ?self.cfg.bridge.udp_port,
            mode = ?self.cfg.mode,
            max_peers = self.cfg.max_peers,
            private = self.cfg.private.is_some(),
            private_peers = self.cfg.private_peers.len(),
            clients = self.cfg.clients.is_some(),
            stripe_topics = held_stripe_topics(self.cfg.network, seed).len(),
            pruning_seed = seed.raw(),
            "Hyperswarm transport starting"
        );
        // The Bare addons need their shared libraries (libstdc++ on Linux); without them no
        // worker can start, so there is nothing to retry.
        if let Err(reason) = xkr_p2p_hyper::bare::check_runtime() {
            self.stay_off(&reason);
            return;
        }
        if let Some(settings) = &self.cfg.hugin {
            match HuginRuntime::start(
                settings,
                self.cfg.network,
                self.cfg.hugin_stats.clone(),
                &self.shutdown,
                self.cfg.net.as_ref(),
            ) {
                Ok(rt) => {
                    if let Some(p) = &self.cfg.private {
                        p.set_hugin_address(&rt.address);
                    }
                    self.hugin = Some(rt);
                }
                Err(e) => tracing::error!(error = %e, "Hugin relay disabled"),
            }
        }
        let mut delay = RESTART_MIN;
        let mut failures = StartFailures::default();
        loop {
            let started = Instant::now();
            let mut verdict = StartVerdict::Log;
            let starting = HyperNetStatus {
                port_fixed: self.cfg.port_fixed(),
                private: self.cfg.private.is_some(),
                ..HyperNetStatus::default()
            };
            self.p2p.set_hyper_net(starting);
            let failure = if let Some(port) = self.cfg.bridge.udp_port
                && !udp_port_free(port)
            {
                self.p2p.set_hyper_net(HyperNetStatus {
                    state: HyperDhtState::Failed,
                    ..starting
                });
                format!("UDP port {port} (--port) is in use")
            } else {
                let spawned = tokio::select! {
                    () = self.shutdown.cancelled() => break,
                    r = self.cfg.start_bridge() => r,
                };
                match spawned {
                    Ok((bridge, host)) => {
                        failures.reset();
                        match self.session(bridge, host, topic).await {
                            SessionEnd::Shutdown => break,
                            SessionEnd::Failed(why) => why,
                            SessionEnd::Renewed => {
                                delay = RESTART_MIN;
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        let text = start_error_text(&e);
                        verdict = failures.record(&text);
                        text
                    }
                }
            };
            if started.elapsed() >= STABLE_SESSION {
                delay = RESTART_MIN;
            }
            match verdict {
                StartVerdict::GiveUp => {
                    self.stay_off(&format!(
                        "{failure} ({MAX_SAME_START_FAILURES} tries in a row)"
                    ));
                    break;
                }
                StartVerdict::Log => tracing::warn!(
                    error = %failure,
                    retry_in_s = delay.as_secs(),
                    "Hyperswarm transport down; restarting the bridge"
                ),
                StartVerdict::Quiet => tracing::debug!(
                    error = %failure,
                    retry_in_s = delay.as_secs(),
                    "Hyperswarm transport down; restarting the bridge"
                ),
            }
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(RESTART_MAX);
        }
        tracing::info!("Hyperswarm transport stopped");
    }

    /// Hyperswarm stays off for the rest of the process: the status says failed and
    /// [`off_line`] is said once (on the terminal by the status display, which reads
    /// [`off_reason`]; with `--log-json` as a warning). The rest of the node keeps running.
    fn stay_off(&self, reason: &str) {
        self.p2p.set_hyper_net(HyperNetStatus {
            state: HyperDhtState::Failed,
            port_fixed: self.cfg.port_fixed(),
            private: self.cfg.private.is_some(),
            ..HyperNetStatus::default()
        });
        let _ = OFF_REASON.set(reason.to_owned());
        let line = off_line(reason);
        if self.cfg.bridge.log_json {
            tracing::warn!("{line}");
        } else {
            tracing::info!("{line}");
        }
    }

    async fn session(&mut self, bridge: Bridge, mut host: BareWorkers, topic: Key) -> SessionEnd {
        let Some(local_key) = bridge.node_public_key() else {
            bridge.close();
            return SessionEnd::Failed("the bridge did not send its public key".into());
        };
        let Some(mut incoming) = bridge.incoming() else {
            bridge.close();
            return SessionEnd::Failed("incoming connections already taken".into());
        };
        // Worker 0 is the node's one DHT node.
        let Some(mut dht) = bridge.dht_status(0) else {
            bridge.close();
            return SessionEnd::Failed("the bridge has no worker".into());
        };
        let mut dht_open = true;
        let first = dht.borrow_and_update().clone();
        if let Some(d) = first {
            report_dht(&self.p2p, &d, &self.cfg, &mut self.port_hint_said);
        }
        tracing::info!(
            node_key = %hex::encode(local_key),
            workers = bridge.workers(),
            "Hyperswarm bridge up"
        );
        self.tracker.start_session(local_key);
        // Unbounded on purpose: every event comes from a task this session spawned (announces, one
        // lookup, at most `MAX_CONCURRENT_DIALS` dials, one handoff and one drop guard per admitted
        // connection, one end per Hugin connection), so the queue is bounded by the connections,
        // which the bridge caps (`MAX_PENDING_INCOMING`) and the tracker admits
        // (`--hyper-max-peers`). A bounded channel would need `try_send` in `DropGuard::drop` and
        // could lose `Dropped` events, leaking tracker slots.
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let mut wanted = self.p2p.wanted_height();
        let mut wanted_open = true;
        // Client mode never announces; a node then only looks stripe topics up.
        let stripe_topics = if self.cfg.mode == AnnounceMode::Client {
            Vec::new()
        } else {
            held_stripe_topics(self.cfg.network, self.p2p.pruning_seed())
        };
        let mut xkr_topics: HashSet<Key> = stripe_topics.iter().copied().collect();
        xkr_topics.insert(topic);
        // The private node's identities of the secret in effect now, and the private nodes we
        // stay connected to; connections on their sync topics are Levin peers.
        let net = xkr_network(self.cfg.network);
        let mut renew = self.cfg.private.as_ref().map(|p| p.subscribe());
        let private = renew.as_mut().map(|rx| {
            let secret = rx.borrow_and_update().clone();
            OwnPrivate {
                topics: secret.topics(net),
                secret,
            }
        });
        let private_peers: Vec<PrivatePeer> = self
            .cfg
            .private_peers
            .iter()
            .map(|i| PrivatePeer {
                key: i.sync_key,
                topic: i.topics(net).sync,
            })
            .collect();
        xkr_topics.extend(private.iter().map(|p| p.topics.sync.topic));
        xkr_topics.extend(private_peers.iter().map(|p| p.topic.topic));
        let mut s = Session {
            p2p: &self.p2p,
            tracker: &mut self.tracker,
            hugin: self.hugin.as_ref(),
            p2pool: self.cfg.p2pool.as_ref(),
            mode: self.cfg.mode,
            bridge,
            topic,
            network: self.cfg.network,
            stripe_topics,
            xkr_topics,
            stripe_lookups: HashMap::new(),
            stripe_lookups_running: 0,
            stripe_waiting: Vec::new(),
            handles: HashMap::new(),
            hugin_handles: HashMap::new(),
            event_tx,
            announced: false,
            lookup_running: false,
            private,
            private_peers,
            clients: self.cfg.clients.as_ref(),
            clients_topic: topic::xkr_clients_topic(net),
            clients_handles: HashMap::new(),
        };
        s.spawn_announce(self.cfg.mode);
        s.spawn_stripe_announces();
        s.spawn_hugin_announces();
        s.spawn_p2pool_announce();
        s.spawn_private_announces();
        s.spawn_clients_announces();
        s.dial_private_peers(Instant::now());
        let lookup = tokio::time::sleep(LOOKUP_EMPTY);
        tokio::pin!(lookup);
        let mut stripe_tick = tokio::time::interval(STRIPE_LOOKUP_INTERVAL);
        stripe_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let step = tokio::select! {
                () = self.shutdown.cancelled() => Step::Shutdown,
                err = s.bridge.wait_closed() => Step::Failed(format!("bridge failed: {err}")),
                why = host.wait() => Step::Failed(why),
                item = incoming.next() => match item {
                    Some(Ok(conn)) => Step::Conn(conn),
                    Some(Err(e)) => Step::Failed(format!("bridge failed: {e}")),
                    None => Step::Failed("bridge closed".into()),
                },
                Some(ev) = events.recv() => Step::Event(ev),
                () = &mut lookup, if s.announced => Step::Lookup,
                r = wanted.changed(), if wanted_open => Step::Wanted(r.is_ok()),
                r = dht.changed(), if dht_open => Step::Dht(r.is_ok()),
                _ = stripe_tick.tick() => Step::StripeTick,
                ok = renewed(renew.as_mut()) => Step::Renewed(ok),
            };
            match step {
                Step::Shutdown => {
                    s.shutdown(&mut host).await;
                    return SessionEnd::Shutdown;
                }
                Step::Failed(why) => {
                    s.bridge.close();
                    host.start_kill();
                    return SessionEnd::Failed(why);
                }
                Step::Conn(conn) => s.on_conn(conn),
                Step::Event(Event::Announced(key)) => {
                    s.announced = true;
                    tracing::info!(key = %key.map_or_default(hex::encode), "XKR topic announced on the DHT");
                    lookup.as_mut().reset(tokio::time::Instant::now());
                }
                Step::Event(ev) => s.on_event(ev),
                Step::Lookup => {
                    s.start_lookup();
                    lookup
                        .as_mut()
                        .reset(tokio::time::Instant::now() + s.tracker.lookup_period());
                }
                Step::Wanted(open) => {
                    wanted_open = open;
                    let height = *wanted.borrow_and_update();
                    match height {
                        Some(h) => s.find_stripe_peers(h),
                        None => s.stripe_waiting.clear(),
                    }
                }
                Step::StripeTick => {
                    let height = *wanted.borrow();
                    if let Some(h) = height {
                        s.find_stripe_peers(h);
                    }
                    s.dial_private_peers(Instant::now());
                }
                Step::Dht(open) => {
                    dht_open = open;
                    let status = dht.borrow_and_update().clone();
                    if let Some(d) = status {
                        report_dht(s.p2p, &d, &self.cfg, &mut self.port_hint_said);
                    }
                }
                Step::Renewed(false) => renew = None,
                Step::Renewed(true) => {
                    tracing::info!("private node invite renewed; restarting the Hyperswarm bridge");
                    s.end_for_renewal(&mut host).await;
                    return SessionEnd::Renewed;
                }
            }
        }
    }
}

/// One bridge session (one set of Bare workers).
struct Session<'a> {
    p2p: &'a P2pHandle,
    tracker: &'a mut Tracker,
    hugin: Option<&'a HuginRuntime>,
    /// The built-in p2pool's sidechain topic (p2pool on).
    p2pool: Option<&'a P2poolTopic>,
    /// `Both`, or `Client` with `--hyper-client-only` (also for the p2pool topic).
    mode: AnnounceMode,
    bridge: Bridge,
    topic: Key,
    network: Network,
    /// Stripe topics we announce (server mode).
    stripe_topics: Vec<Key>,
    /// Topics whose connections are XKR (Levin) peers: the blocks topic, our stripe topics and
    /// every stripe topic we looked up.
    xkr_topics: HashSet<Key>,
    /// Stripe topic -> its last lookup.
    stripe_lookups: HashMap<Key, Instant>,
    /// Stripe lookups in flight (one per lookup count of the wanted height).
    stripe_lookups_running: usize,
    /// `(peer, stripe topic)` found by stripe lookups while `--hyper-max-peers` was reached:
    /// dialed when a connection ends (after a stripe rotation), dropped when nothing is wanted.
    stripe_waiting: Vec<(Key, Key)>,
    /// Close handles of the connections handed to the P2P manager.
    handles: HashMap<u64, CloseHandle>,
    /// Close handles of the connections served by the Hugin relay.
    hugin_handles: HashMap<u64, CloseHandle>,
    event_tx: mpsc::UnboundedSender<Event>,
    announced: bool,
    lookup_running: bool,
    /// The private node's secret and topics (`--private`).
    private: Option<OwnPrivate>,
    /// The private nodes to stay connected to (`--private-peer`).
    private_peers: Vec<PrivatePeer>,
    /// The clients topic's server.
    clients: Option<&'a Arc<ClientsServer>>,
    /// The public clients topic (`xkr/<network>/clients/v1`).
    clients_topic: HuginTopic,
    /// Close handles of the client connections.
    clients_handles: HashMap<u64, CloseHandle>,
}

/// The private node's secret of this session and its topics.
#[derive(Debug)]
struct OwnPrivate {
    secret: PrivateSecret,
    topics: PrivateTopics,
}

/// A private node to stay connected to: the key of its sync identity and the sync topic of its
/// invite (whose topic key signs our `mid`).
#[derive(Debug)]
struct PrivatePeer {
    key: Key,
    topic: HuginTopic,
}

const PRIVATE_SYNC_LABEL: &str = "private sync";
const PRIVATE_HUGIN_LABEL: &str = "private hugin";
const CLIENTS_LABEL: &str = "clients";
const PRIVATE_CLIENTS_LABEL: &str = "private clients";

impl Session<'_> {
    fn spawn_announce(&self, mode: AnnounceMode) {
        let bridge = self.bridge.clone();
        let events = self.event_tx.clone();
        let topic = self.topic;
        tokio::spawn(async move {
            loop {
                match bridge.announce(topic, mode, None).await {
                    Ok(ack) => {
                        let _ = events.send(Event::Announced(ack.local_public_key()));
                        return;
                    }
                    Err(e) => {
                        if bridge.error().is_some() || events.is_closed() {
                            return;
                        }
                        tracing::warn!(error = %e, "announcing the XKR topic failed; retrying");
                        tokio::time::sleep(ANNOUNCE_RETRY).await;
                    }
                }
            }
        });
    }

    /// Announces the stripe topics we hold, in server mode (findable, no auto-dials), retrying
    /// like the XKR topic.
    fn spawn_stripe_announces(&self) {
        if self.stripe_topics.is_empty() {
            return;
        }
        let bridge = self.bridge.clone();
        let events = self.event_tx.clone();
        let topics = self.stripe_topics.clone();
        tokio::spawn(async move {
            for topic in &topics {
                loop {
                    match bridge.announce(*topic, AnnounceMode::Server, None).await {
                        Ok(_) => break,
                        Err(e) => {
                            if bridge.error().is_some() || events.is_closed() {
                                return;
                            }
                            tracing::warn!(
                                error = %e,
                                topic = %hex::encode(topic),
                                "announcing a stripe topic failed; retrying"
                            );
                            tokio::time::sleep(ANNOUNCE_RETRY).await;
                        }
                    }
                }
            }
            let _ = events.send(Event::StripesAnnounced(topics.len()));
        });
    }

    /// Looks up the holders of `height` and dials what it finds, because the downloader needs
    /// that range and no connected peer holds it: one lookup per lookup count
    /// ([`stripe_lookup_topics`]: the topic of stripe `k` of 4 and of stripe `j` of 6). Full
    /// nodes announce every default stripe, so the default-count lookup finds them as well as
    /// pruned holders. One round of stripe lookups runs at a time, and each topic is looked up
    /// at most every [`STRIPE_LOOKUP_INTERVAL`].
    fn find_stripe_peers(&mut self, height: u64) {
        if self.stripe_lookups_running > 0 {
            return;
        }
        let now = Instant::now();
        let due: Vec<(PruningSeed, Key)> = stripe_lookup_topics(self.network, height)
            .into_iter()
            .filter(|(_, topic)| {
                self.stripe_lookups
                    .get(topic)
                    .is_none_or(|t| now.duration_since(*t) >= STRIPE_LOOKUP_INTERVAL)
            })
            .collect();
        if due.is_empty() {
            return;
        }
        self.tracker.prune(now);
        for (seed, topic) in due {
            self.stripe_lookups_running += 1;
            self.stripe_lookups.insert(topic, now);
            self.xkr_topics.insert(topic);
            tracing::info!(
                height,
                stripes = seed.stripes(),
                stripe = seed.stripe(),
                topic = %hex::encode(topic),
                "looking up peers that hold a missing stripe"
            );
            let bridge = self.bridge.clone();
            let events = self.event_tx.clone();
            tokio::spawn(async move {
                let result = bridge.lookup(topic, STRIPE_LOOKUP_MAX).await;
                let _ = events.send(Event::StripeLooked { topic, result });
            });
        }
    }

    /// Dials the stripe holders found while `--hyper-max-peers` was reached, now that a
    /// connection ended (within the usual stripe-dial bounds; the rest are forgotten, the next
    /// lookup finds them again).
    fn dial_waiting_stripe_peers(&mut self, now: Instant) {
        if self.stripe_waiting.is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut self.stripe_waiting);
        let keys: Vec<Key> = waiting.iter().map(|(key, _)| *key).collect();
        for key in self.tracker.stripe_dial_candidates(&keys, now) {
            let topic = waiting
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(self.topic, |(_, t)| *t);
            tracing::info!(peer = %short(&key), "dialing a stripe holder into a freed slot");
            self.spawn_dial(key, topic);
        }
    }

    /// Joins the Hugin topics (each with its `mid` and identity), retrying like the XKR topic.
    fn spawn_hugin_announces(&self) {
        let Some(h) = self.hugin else {
            return;
        };
        for join in &h.topics {
            let bridge = self.bridge.clone();
            let events = self.event_tx.clone();
            let (label, topic, mode, mid) =
                (join.label, join.topic.topic, join.mode, join.mid.clone());
            let mut synced = join.after_sync.then(|| h.synced.clone());
            tokio::spawn(async move {
                if let Some(rx) = &mut synced {
                    let ready = rx.wait_for(|s| *s).await.is_ok();
                    if !ready {
                        return;
                    }
                    tracing::info!(
                        topic = label,
                        "node synchronized: announcing the Hugin topic"
                    );
                }
                loop {
                    match bridge.announce(topic, mode, Some(mid.clone())).await {
                        Ok(ack) => {
                            let _ = events.send(Event::HuginAnnounced {
                                label,
                                key: ack.local_public_key(),
                            });
                            return;
                        }
                        Err(e) => {
                            if bridge.error().is_some() || events.is_closed() {
                                return;
                            }
                            tracing::warn!(topic = label, error = %e, "announcing a Hugin topic failed; retrying");
                            tokio::time::sleep(ANNOUNCE_RETRY).await;
                        }
                    }
                }
            });
        }
    }

    /// Joins the built-in p2pool's sidechain topic once p2pool runs, on its own identity (see
    /// [`p2pool_topic`]), retrying like the XKR topic.
    fn spawn_p2pool_announce(&self) {
        let Some(p) = self.p2pool else {
            return;
        };
        let bridge = self.bridge.clone();
        let events = self.event_tx.clone();
        let (topic, mid, mode) = (p.topic.topic, p.topic.mid(), self.mode);
        let mut ready = p.ready.clone();
        tokio::spawn(async move {
            if ready.wait_for(|r| *r).await.is_err() {
                return;
            }
            loop {
                match bridge.announce(topic, mode, Some(mid.clone())).await {
                    Ok(ack) => {
                        let _ = events.send(Event::P2poolAnnounced(ack.local_public_key()));
                        return;
                    }
                    Err(e) => {
                        if bridge.error().is_some() || events.is_closed() {
                            return;
                        }
                        tracing::warn!(error = %e, "announcing the p2pool topic failed; retrying");
                        tokio::time::sleep(ANNOUNCE_RETRY).await;
                    }
                }
            }
        });
    }

    /// Announces the private node's identities (`--private`) in server mode, each with its topic
    /// key and identity seed (`xkr_p2p_hyper::private`), retrying like the XKR topic: the sync
    /// identity always, the Hugin identity with the relay on.
    fn spawn_private_announces(&self) {
        let Some(p) = &self.private else {
            return;
        };
        let mut joins = vec![(
            PRIVATE_SYNC_LABEL,
            p.topics.sync.topic,
            p.secret.sync_mid(&p.topics),
        )];
        if self.hugin.is_some() {
            joins.push((
                PRIVATE_HUGIN_LABEL,
                p.topics.hugin.topic,
                p.secret.hugin_mid(&p.topics),
            ));
        }
        for (label, topic, mid) in joins {
            let bridge = self.bridge.clone();
            let events = self.event_tx.clone();
            tokio::spawn(async move {
                loop {
                    match bridge
                        .announce(topic, AnnounceMode::Server, Some(mid.clone()))
                        .await
                    {
                        Ok(ack) => {
                            let _ = events.send(Event::PrivateAnnounced {
                                label,
                                key: ack.local_public_key(),
                            });
                            return;
                        }
                        Err(e) => {
                            if bridge.error().is_some() || events.is_closed() {
                                return;
                            }
                            tracing::warn!(topic = label, error = %e, "announcing a private topic failed; retrying");
                            tokio::time::sleep(ANNOUNCE_RETRY).await;
                        }
                    }
                }
            });
        }
    }

    /// Dials every `--private-peer` that is not connected, being dialed, banned or backing off
    /// ([`Tracker::private_dial_candidates`]); called at the start of a session, every
    /// [`STRIPE_LOOKUP_INTERVAL`] and when a connection ends, so the node stays connected and
    /// redials with the usual back-off.
    fn dial_private_peers(&mut self, now: Instant) {
        if self.private_peers.is_empty() {
            return;
        }
        let keys: Vec<Key> = self.private_peers.iter().map(|p| p.key).collect();
        for key in self.tracker.private_dial_candidates(&keys, now) {
            let Some(topic) = self
                .private_peers
                .iter()
                .find(|p| p.key == key)
                .map(|p| p.topic.clone())
            else {
                continue;
            };
            tracing::debug!(peer = %short(&key), "dialing a private peer");
            let bridge = self.bridge.clone();
            let events = self.event_tx.clone();
            tokio::spawn(async move {
                let result = bridge
                    .open_hugin(key, topic.topic, OpenFlags::NONE, &topic.mid())
                    .await;
                let _ = events.send(Event::Dialed { key, result });
            });
        }
    }

    /// Announces the clients topic in server mode once the node is synchronized (not in client
    /// mode: `--hyper-client-only`, `--private`), and with `--private` the private clients
    /// topic of the invite at once, retrying like the XKR topic.
    fn spawn_clients_announces(&self) {
        let Some(server) = self.clients else {
            return;
        };
        let mut joins = Vec::new();
        if self.mode != AnnounceMode::Client {
            joins.push((
                CLIENTS_LABEL,
                self.clients_topic.topic,
                self.clients_topic.mid(),
                true,
            ));
        }
        if let Some(p) = &self.private {
            joins.push((
                PRIVATE_CLIENTS_LABEL,
                p.topics.clients.topic,
                p.secret.clients_mid(&p.topics),
                false,
            ));
        }
        for (label, topic, mid, after_sync) in joins {
            let bridge = self.bridge.clone();
            let events = self.event_tx.clone();
            let server = server.clone();
            tokio::spawn(async move {
                if after_sync {
                    if !server.wait_synced().await {
                        return;
                    }
                    tracing::info!(
                        topic = label,
                        "node synchronized: announcing the clients topic"
                    );
                }
                loop {
                    match bridge
                        .announce(topic, AnnounceMode::Server, Some(mid.clone()))
                        .await
                    {
                        Ok(ack) => {
                            let _ = events.send(Event::ClientsAnnounced {
                                label,
                                key: ack.local_public_key(),
                            });
                            return;
                        }
                        Err(e) => {
                            if bridge.error().is_some() || events.is_closed() {
                                return;
                            }
                            tracing::warn!(topic = label, error = %e, "announcing the clients topic failed; retrying");
                            tokio::time::sleep(ANNOUNCE_RETRY).await;
                        }
                    }
                }
            });
        }
    }

    /// Whether `topic` is the public or the private clients topic.
    fn is_clients_topic(&self, topic: &Key) -> bool {
        self.clients.is_some()
            && (*topic == self.clients_topic.topic
                || self
                    .private
                    .as_ref()
                    .is_some_and(|p| p.topics.clients.topic == *topic))
    }

    /// Hands a connection from a clients topic to the clients server, if it admits it.
    fn start_client(&mut self, conn: HyperConn) {
        let Some(server) = self.clients.cloned() else {
            conn.close(CloseReason::Normal);
            return;
        };
        let (conn_id, key, addr) = {
            let info = conn.info();
            (info.conn_id, info.remote_public_key, info.addr)
        };
        let slot = match server.admit(&key, addr, Instant::now()) {
            Ok(slot) => slot,
            Err(reason) => {
                tracing::debug!(peer = %short(&key), ?reason, "client connection refused");
                conn.close(CloseReason::Normal);
                return;
            }
        };
        self.clients_handles.insert(conn_id, conn.close_handle());
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            let end = server.serve(slot, key, conn).await;
            tracing::debug!(peer = %short(&key), %end, "client connection ended");
            let _ = events.send(Event::ClientsDone { conn_id });
        });
    }

    /// Hands a connection from a Hugin topic to the relay.
    fn start_hugin(&mut self, conn: HyperConn, role: Role) {
        let Some(h) = self.hugin else {
            conn.close(CloseReason::Normal);
            return;
        };
        let conn_id = conn.info().conn_id;
        let meta = PeerMeta {
            role,
            remote_key: conn.info().remote_public_key,
            remote_ip: conn.info().addr.as_ref().map(SocketAddr::ip),
        };
        self.hugin_handles.insert(conn_id, conn.close_handle());
        let relay = h.relay.clone();
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            relay.serve(conn, meta).await;
            let _ = events.send(Event::HuginDone { conn_id });
        });
    }

    fn start_lookup(&mut self) {
        self.tracker.prune(Instant::now());
        if self.lookup_running || !self.tracker.wants_more() {
            return;
        }
        self.lookup_running = true;
        let bridge = self.bridge.clone();
        let events = self.event_tx.clone();
        let topic = self.topic;
        tokio::spawn(async move {
            let result = bridge.lookup(topic, LOOKUP_MAX).await;
            let _ = events.send(Event::Looked(result));
        });
    }

    fn spawn_dial(&self, key: Key, topic: Key) {
        let bridge = self.bridge.clone();
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            let result = bridge.open(key, Some(topic), OpenFlags::NONE).await;
            let _ = events.send(Event::Dialed { key, result });
        });
    }

    fn on_conn(&mut self, conn: HyperConn) {
        let mut meta = ConnMeta::of(&conn);
        let conn_topic = conn.info().topic;
        if let Some(t) = conn_topic
            && !self.xkr_topics.contains(&t)
        {
            if self.is_clients_topic(&t) {
                self.start_client(conn);
            } else if let Some(p) = self.p2pool.filter(|p| p.topic.topic == t) {
                // The p2pool P2P server takes it; if p2pool is gone the connection is dropped,
                // which closes it.
                if p.conns.send(conn).is_err() {
                    tracing::debug!(peer = %short(&meta.key), "p2pool connection while p2pool is not running; closed");
                }
            } else if self
                .private
                .as_ref()
                .is_some_and(|p| p.topics.hugin.topic == t)
            {
                // An app with the private node's invite.
                self.start_hugin(conn, Role::Client);
            } else if let Some(role) = self.hugin.and_then(|h| h.role_of(&t)) {
                self.start_hugin(conn, role);
            } else {
                tracing::debug!(peer = %short(&meta.key), topic = %hex::encode(t), "connection on a foreign topic; closing");
                conn.close(CloseReason::Normal);
            }
            return;
        }
        meta.private = self
            .private
            .as_ref()
            .is_some_and(|p| conn_topic == Some(p.topics.sync.topic));
        match self.tracker.admit(meta, Instant::now()) {
            Admit::Reject(reason) => {
                tracing::debug!(peer = %short(&meta.key), initiator = meta.initiator, ?reason, "Hyperswarm connection refused");
                conn.close(CloseReason::Normal);
            }
            Admit::Replace { old } => {
                tracing::debug!(peer = %short(&meta.key), initiator = meta.initiator, "duplicate Hyperswarm connection; keeping the new one");
                if let Some(h) = self.handles.remove(&old) {
                    h.close(CloseReason::Normal);
                }
                self.start_handoff(conn, meta);
            }
            Admit::AcceptEvicting { evicted } => {
                tracing::debug!(peer = %short(&meta.key), evicted, "inbound handshakes full; closing the one pending longest");
                if let Some(h) = self.handles.remove(&evicted) {
                    h.close(CloseReason::Normal);
                }
                self.start_handoff(conn, meta);
            }
            Admit::Accept => self.start_handoff(conn, meta),
        }
    }

    /// Boxes `conn` and hands it to the P2P manager in a task; the result comes back as
    /// [`Event::HandedOff`].
    fn start_handoff(&mut self, conn: HyperConn, meta: ConnMeta) {
        // Every plain-swarm connection belongs to an XKR topic we join or looked up, also
        // server-side accepts whose handshake does not name it (tagged with the blocks topic).
        let topic = conn
            .info()
            .topic
            .filter(|t| self.xkr_topics.contains(t))
            .unwrap_or(self.topic);
        self.handles.insert(meta.conn_id, conn.close_handle());
        let stream: BoxedPeerStream = Box::new(TrackedConn {
            conn,
            _guard: DropGuard {
                conn_id: meta.conn_id,
                events: self.event_tx.clone(),
            },
        });
        let origin = PeerOrigin::Hyper {
            remote_public_key: meta.key,
            topic: Some(topic),
        };
        let p2p = self.p2p.clone();
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            let result = hand_off(&p2p, stream, origin, meta.initiator).await;
            let _ = events.send(Event::HandedOff { meta, result });
        });
    }

    fn on_event(&mut self, ev: Event) {
        let now = Instant::now();
        match ev {
            Event::Announced(_) => self.announced = true,
            Event::Looked(result) => {
                self.lookup_running = false;
                match result {
                    Ok(peers) => {
                        let keys: Vec<Key> = peers.iter().map(|p| p.public_key).collect();
                        let dials = self.tracker.dial_candidates(&keys, now);
                        tracing::debug!(
                            found = keys.len(),
                            dialing = dials.len(),
                            connected = self.tracker.connected(),
                            "Hyperswarm lookup"
                        );
                        for key in dials {
                            self.spawn_dial(key, self.topic);
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "Hyperswarm lookup failed"),
                }
            }
            Event::Dialed { key, result } => match result {
                Ok(conn) => {
                    self.tracker.on_dial_result(key, true, now);
                    self.on_conn(conn);
                }
                Err(e) => {
                    self.tracker.on_dial_result(key, false, now);
                    tracing::debug!(peer = %short(&key), error = %e, "Hyperswarm dial failed");
                }
            },
            Event::HandedOff { meta, result } => match result {
                Ok(()) => {
                    self.tracker.on_handoff(meta, Ok(()), now);
                    tracing::info!(
                        peer = %short(&meta.key),
                        outbound = meta.initiator,
                        hyper_peers = self.tracker.connected(),
                        "Hyperswarm peer connected"
                    );
                }
                Err(e) => {
                    let ban = self.tracker.on_handoff(meta, Err(e.kind), now);
                    if let Some(h) = self.handles.get(&meta.conn_id) {
                        h.close(CloseReason::Normal);
                    }
                    tracing::info!(
                        peer = %short(&meta.key),
                        outbound = meta.initiator,
                        error = %e.message,
                        ban_s = ban.as_ref().map(Duration::as_secs),
                        "Levin handshake over Hyperswarm failed"
                    );
                }
            },
            Event::StripesAnnounced(count) => {
                tracing::info!(count, "stripe topics announced on the DHT");
            }
            Event::StripeLooked { topic, result } => {
                self.stripe_lookups_running = self.stripe_lookups_running.saturating_sub(1);
                match result {
                    Ok(peers) => {
                        let found: Vec<Key> = peers.iter().map(|p| p.public_key).collect();
                        let keys = self.tracker.sample_stripe_keys(&found);
                        let dials = self.tracker.stripe_dial_candidates(&keys, now);
                        let blocked = if dials.is_empty() {
                            self.tracker.blocked_stripe_dials(&keys, now)
                        } else {
                            Vec::new()
                        };
                        tracing::info!(
                            topic = %hex::encode(topic),
                            found = keys.len(),
                            dialing = dials.len(),
                            waiting_for_slot = blocked.len(),
                            "stripe lookup"
                        );
                        for key in dials {
                            self.spawn_dial(key, topic);
                        }
                        if !blocked.is_empty() {
                            // Every slot is taken: have the P2P manager drop an outbound peer
                            // without the stripe (rate-limited there) and dial a holder into
                            // the freed slot.
                            self.stripe_waiting.retain(|(_, t)| *t != topic);
                            self.stripe_waiting
                                .extend(blocked.into_iter().map(|k| (k, topic)));
                            self.p2p.rotate_hyper_for_stripe();
                        }
                    }
                    Err(e) => tracing::debug!(error = %e, "stripe lookup failed"),
                }
            }
            Event::HuginAnnounced { label, key } => {
                tracing::info!(topic = label, key = %key.map_or_default(hex::encode), "Hugin topic announced on the DHT");
                if label == PRIVATE_LABEL
                    && let (Some(h), Some(k)) = (self.hugin, key)
                {
                    tracing::info!(
                        invite = %format!("{}{}", h.address, hex::encode(k)),
                        "Hugin private node invite (node address + key)"
                    );
                }
            }
            Event::HuginDone { conn_id } => {
                self.hugin_handles.remove(&conn_id);
            }
            Event::ClientsAnnounced { label, key } => {
                tracing::info!(topic = label, key = %key.map_or_default(hex::encode), "clients topic announced on the DHT");
            }
            Event::ClientsDone { conn_id } => {
                self.clients_handles.remove(&conn_id);
            }
            Event::P2poolAnnounced(key) => {
                tracing::info!(key = %key.map_or_default(hex::encode), "p2pool topic announced on the DHT");
            }
            Event::PrivateAnnounced { label, key } => {
                tracing::info!(topic = label, key = %key.map_or_default(hex::encode), "private topic announced on the DHT");
                if label == PRIVATE_SYNC_LABEL
                    && let Some(p) = &self.private
                {
                    tracing::info!(
                        invite = %p.secret.invite(),
                        "private node invite (console: invite)"
                    );
                }
                if label == PRIVATE_HUGIN_LABEL
                    && let (Some(p), Some(h)) = (&self.private, self.hugin)
                {
                    tracing::info!(
                        invite = %p.secret.hugin_app_invite(&h.address),
                        "Hugin app invite of the private node (console: invite)"
                    );
                }
            }
            Event::Dropped { conn_id } => {
                self.handles.remove(&conn_id);
                if self.tracker.on_dropped(conn_id) {
                    tracing::debug!(
                        conn_id,
                        hyper_peers = self.tracker.connected(),
                        "Hyperswarm peer gone"
                    );
                    self.dial_waiting_stripe_peers(now);
                    self.dial_private_peers(now);
                }
            }
        }
    }

    /// Ends the session for a new private secret: closes every connection and the bridge and
    /// lets the workers leave the DHT, so no identity of the old secret stays reachable. The
    /// supervisor starts the bridge again at once with the new secret.
    async fn end_for_renewal(&mut self, host: &mut BareWorkers) {
        let now = Instant::now();
        for (conn_id, h) in self.handles.drain() {
            self.tracker.closing_on_purpose(conn_id, now);
            h.close(CloseReason::Normal);
        }
        for (_, h) in self.hugin_handles.drain() {
            h.close(CloseReason::Normal);
        }
        for (_, h) in self.clients_handles.drain() {
            h.close(CloseReason::Normal);
        }
        self.bridge.close();
        host.stop(SHUTDOWN_WORKERS_STOP).await;
    }

    async fn shutdown(&mut self, host: &mut BareWorkers) {
        tracing::info!(
            hyper_peers = self.tracker.connected(),
            "Hyperswarm transport shutting down"
        );
        let bridge = &self.bridge;
        let (xkr_topic, hugin) = (self.topic, self.hugin);
        let p2pool_topic = self.p2pool.map(|p| p.topic.topic);
        let private_topics = self.private.as_ref().map(|p| {
            [
                p.topics.sync.topic,
                p.topics.hugin.topic,
                p.topics.clients.topic,
            ]
        });
        let clients_topic = self
            .clients
            .filter(|_| self.mode != AnnounceMode::Client)
            .map(|_| self.clients_topic.topic);
        let stripe_topics = &self.stripe_topics;
        let step = crate::stopping::step("Hyperswarm announcements");
        let unannounced = tokio::time::timeout(SHUTDOWN_UNANNOUNCE, async {
            let _ = bridge.unannounce(xkr_topic).await;
            for t in stripe_topics {
                let _ = bridge.unannounce(*t).await;
            }
            if let Some(t) = p2pool_topic {
                let _ = bridge.unannounce(t).await;
            }
            if let Some(t) = clients_topic {
                let _ = bridge.unannounce(t).await;
            }
            for t in private_topics.iter().flatten() {
                let _ = bridge.unannounce(*t).await;
            }
            for j in hugin.map_or(&[][..], |h| h.topics.as_slice()) {
                let _ = bridge.unannounce(j.topic.topic).await;
            }
        })
        .await;
        match unannounced {
            Ok(()) => step.done_with("topics left"),
            Err(_) => step.timed_out(SHUTDOWN_UNANNOUNCE),
        }
        let step = crate::stopping::step("Hyperswarm peers");
        let now = Instant::now();
        let peers = self.handles.len();
        for (conn_id, h) in self.handles.drain() {
            self.tracker.closing_on_purpose(conn_id, now);
            h.close(CloseReason::Normal);
        }
        step.done_with(format!("{peers} disconnected"));
        for (_, h) in self.clients_handles.drain() {
            h.close(CloseReason::Normal);
        }
        if hugin.is_some() {
            let step = crate::stopping::step("Hugin relay");
            let apps = self.hugin_handles.len();
            for (_, h) in self.hugin_handles.drain() {
                h.close(CloseReason::Normal);
            }
            step.done_with(format!("{apps} connections closed"));
        } else {
            for (_, h) in self.hugin_handles.drain() {
                h.close(CloseReason::Normal);
            }
        }
        // Closing the bridge ends the Bare workers' links (the workers leave the DHT and their
        // threads then terminate the worklets).
        let step = crate::stopping::step("Hyperswarm Bare workers");
        let started = Instant::now();
        self.bridge.close();
        host.stop(SHUTDOWN_WORKERS_STOP).await;
        if started.elapsed() >= SHUTDOWN_WORKERS_STOP {
            step.timed_out(SHUTDOWN_WORKERS_STOP);
        } else {
            step.done_with("exited");
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn meta(conn_id: u64, key: Key, initiator: bool, hash: u8) -> ConnMeta {
        ConnMeta {
            conn_id,
            key,
            initiator,
            handshake_hash: [hash; 64],
            ip: None,
            private: false,
        }
    }

    /// An inbound connection from `ip`.
    fn inbound_meta(conn_id: u64, key: Key, ip: &str) -> ConnMeta {
        ConnMeta {
            ip: Some(address_key(SocketAddr::new(ip.parse().unwrap(), 49737))),
            ..meta(conn_id, key, false, 0)
        }
    }

    fn numbered_key(i: usize) -> Key {
        let mut k = [0u8; 32];
        k[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
        k
    }

    fn tracker(local: Key, max: usize) -> Tracker {
        let mut t = Tracker::new(max);
        t.start_session(local);
        t
    }

    /// Both ends see the same two connections (with mirrored `initiator` flags and the same
    /// handshake hash) in any order and must keep the same one.
    #[test]
    fn duplicate_resolution_is_symmetric() {
        let a: Key = [0xa0; 32];
        let b: Key = [0x0b; 32];
        let now = Instant::now();
        // For two connections between a and b: (initiated by a?, handshake hash) of each.
        let cases = [
            ((true, 1), (false, 2)),
            ((false, 1), (true, 2)),
            ((true, 1), (true, 2)),
            ((false, 7), (false, 3)),
        ];
        let kept = |local: Key,
                    remote: Key,
                    initiated_here: [bool; 2],
                    hashes: [u8; 2],
                    first_arrives_first: bool| {
            let mut t = tracker(local, 8);
            let first = meta(1, remote, initiated_here[0], hashes[0]);
            let second = meta(2, remote, initiated_here[1], hashes[1]);
            let (earlier, later) = if first_arrives_first {
                (first, second)
            } else {
                (second, first)
            };
            assert_eq!(t.admit(earlier, now), Admit::Accept);
            match t.admit(later, now) {
                Admit::Replace { .. } => later.conn_id,
                Admit::Reject(RejectReason::Duplicate) => earlier.conn_id,
                other => panic!("unexpected {other:?}"),
            }
        };
        for ((one_by_a, hash_one), (two_by_a, hash_two)) in cases {
            for order_at_a in [true, false] {
                for order_at_b in [true, false] {
                    let hashes = [hash_one, hash_two];
                    let kept_at_a = kept(a, b, [one_by_a, two_by_a], hashes, order_at_a);
                    let kept_at_b = kept(b, a, [!one_by_a, !two_by_a], hashes, order_at_b);
                    assert_eq!(
                        kept_at_a, kept_at_b,
                        "one_by_a={one_by_a} two_by_a={two_by_a} order_at_a={order_at_a} order_at_b={order_at_b}"
                    );
                }
            }
        }
    }

    #[test]
    fn higher_key_is_the_expected_initiator() {
        let high: Key = [0xff; 32];
        let low: Key = [0x01; 32];
        let now = Instant::now();
        // On the high-key node the connection it initiated wins even with a worse hash.
        let mut t = tracker(high, 8);
        assert_eq!(t.admit(meta(1, low, false, 0), now), Admit::Accept);
        assert_eq!(
            t.admit(meta(2, low, true, 9), now),
            Admit::Replace { old: 1 }
        );
        // On the low-key node the remote-initiated one wins.
        let mut t = tracker(low, 8);
        assert_eq!(t.admit(meta(1, high, true, 0), now), Admit::Accept);
        assert_eq!(
            t.admit(meta(2, high, false, 9), now),
            Admit::Replace { old: 1 }
        );
    }

    #[test]
    fn an_established_connection_yields_to_a_reconnect() {
        let local: Key = [0xff; 32];
        let remote: Key = [0x01; 32];
        let now = Instant::now();
        let mut t = tracker(local, 8);
        let first = meta(1, remote, true, 0);
        assert_eq!(t.admit(first, now), Admit::Accept);
        assert_eq!(t.on_handoff(first, Ok(()), now), None);
        // The remote restarted and dials again: the new connection wins although we are the
        // expected initiator.
        assert_eq!(
            t.admit(meta(2, remote, false, 9), now),
            Admit::Replace { old: 1 }
        );
        // The replaced connection's drop does not remove the new one.
        assert!(!t.on_dropped(1));
        assert_eq!(t.connected(), 1);
        assert!(t.on_dropped(2));
        assert_eq!(t.connected(), 0);
    }

    #[test]
    fn ban_and_backoff_lists_are_bounded() {
        let now = Instant::now();
        let mut t = tracker([0xee; 32], 8);
        let key = |i: usize| -> Key {
            let mut k = [0; 32];
            k[..8].copy_from_slice(&(i as u64).to_le_bytes());
            k
        };
        let at = |i: usize| now + Duration::from_millis(i as u64);
        // The first key gets the longest ban, then shorter bans of later keys fill the list.
        assert_eq!(
            t.on_handoff(meta(0, key(0), false, 0), Err(FailureKind::Ourselves), now),
            Some(BAN_SELF)
        );
        for i in 1..=MAX_TRACKED_KEYS {
            t.on_handoff(
                meta(i as u64, key(i), false, 0),
                Err(FailureKind::Protocol),
                at(i),
            );
        }
        assert_eq!(t.bans.len(), MAX_TRACKED_KEYS);
        assert!(t.is_banned(&key(0), at(1)), "the ban ending last stays");
        assert!(
            !t.bans.contains_key(&key(1)),
            "the ban ending first made room"
        );
        assert!(t.is_banned(&key(MAX_TRACKED_KEYS), at(MAX_TRACKED_KEYS)));

        for i in 1..=MAX_TRACKED_KEYS {
            t.bump_backoff(key(i), at(i));
        }
        assert_eq!(t.backoff.len(), MAX_TRACKED_KEYS);
        // Another attempt of a listed key removes nothing; a new key replaces the earliest end.
        t.bump_backoff(key(MAX_TRACKED_KEYS), at(MAX_TRACKED_KEYS));
        assert_eq!(t.backoff.len(), MAX_TRACKED_KEYS);
        assert!(t.backoff.contains_key(&key(1)));
        t.bump_backoff(key(MAX_TRACKED_KEYS + 1), at(MAX_TRACKED_KEYS + 1));
        assert_eq!(t.backoff.len(), MAX_TRACKED_KEYS);
        assert!(!t.backoff.contains_key(&key(1)));
        assert!(t.backoff.contains_key(&key(MAX_TRACKED_KEYS + 1)));
    }

    #[test]
    fn self_bans_and_limit() {
        let local: Key = [5; 32];
        let now = Instant::now();
        let mut t = tracker(local, 2);
        assert_eq!(
            t.admit(meta(1, local, true, 0), now),
            Admit::Reject(RejectReason::Ourselves)
        );
        assert_eq!(t.admit(meta(2, [1; 32], true, 0), now), Admit::Accept);
        assert_eq!(t.admit(meta(3, [2; 32], false, 0), now), Admit::Accept);
        assert_eq!(
            t.admit(meta(4, [3; 32], false, 0), now),
            Admit::Reject(RejectReason::Full)
        );

        // a wrong-network peer is banned for an hour, also across sessions
        let bad = meta(2, [1; 32], true, 0);
        assert_eq!(
            t.on_handoff(bad, Err(FailureKind::Incompatible), now),
            Some(BAN_INCOMPATIBLE)
        );
        assert!(t.on_dropped(2));
        t.start_session(local);
        assert_eq!(
            t.admit(meta(5, [1; 32], false, 0), now),
            Admit::Reject(RejectReason::Banned)
        );
        let later = now + BAN_INCOMPATIBLE + Duration::from_secs(1);
        assert_eq!(t.admit(meta(6, [1; 32], false, 0), later), Admit::Accept);

        // transient failures do not ban
        let flaky = meta(7, [4; 32], true, 0);
        assert_eq!(t.admit(flaky, later), Admit::Accept);
        assert_eq!(
            t.on_handoff(flaky, Err(FailureKind::Transient), later),
            None
        );
    }

    /// Streams count against `--hyper-max-peers` from admission on (before the Levin
    /// handshake), and at most `MAX_PENDING_HANDSHAKES` outbound handshakes run at once.
    #[test]
    fn pending_handshakes_are_bounded() {
        let local: Key = [0xee; 32];
        let now = Instant::now();
        let mut t = tracker(local, 100);
        let key = numbered_key;
        for i in 0..MAX_PENDING_HANDSHAKES {
            assert_eq!(t.admit(meta(i as u64, key(i), true, 0), now), Admit::Accept);
        }
        assert_eq!(
            t.connected(),
            MAX_PENDING_HANDSHAKES,
            "counted before the handshake"
        );
        let extra = MAX_PENDING_HANDSHAKES;
        assert_eq!(
            t.admit(meta(extra as u64, key(extra), true, 0), now),
            Admit::Reject(RejectReason::Busy)
        );
        // One handshake finishes: room for one more.
        assert_eq!(t.on_handoff(meta(0, key(0), true, 0), Ok(()), now), None);
        assert_eq!(
            t.admit(meta(extra as u64, key(extra), true, 0), now),
            Admit::Accept
        );
        // The cap of `--hyper-max-peers` still applies to admitted (pending) streams.
        let mut small = tracker(local, 2);
        assert_eq!(small.admit(meta(1, key(1), true, 0), now), Admit::Accept);
        assert_eq!(small.admit(meta(2, key(2), true, 0), now), Admit::Accept);
        assert_eq!(
            small.admit(meta(3, key(3), true, 0), now),
            Admit::Reject(RejectReason::Full)
        );
    }

    /// Pending inbound connections never refuse an outbound one; beyond `MAX_PENDING_INBOUND`
    /// a new inbound connection closes the one pending longest (a replaced connection keeps
    /// its admission time).
    /// A private node takes inbound connections only on its sync identity; its own dials
    /// (lookups, Hyperswarm auto-dials, private peers) are admitted as before.
    #[test]
    fn a_private_node_admits_inbound_only_on_its_identity() {
        let now = Instant::now();
        let mut t = tracker([0xee; 32], 16);
        t.invite_only = true;
        assert_eq!(
            t.admit(meta(1, numbered_key(1), false, 0), now),
            Admit::Reject(RejectReason::NotInvited)
        );
        let invited = ConnMeta {
            private: true,
            ..meta(2, numbered_key(2), false, 0)
        };
        assert_eq!(t.admit(invited, now), Admit::Accept);
        assert_eq!(
            t.admit(meta(3, numbered_key(3), true, 0), now),
            Admit::Accept
        );
        // the same inbound connection is fine on a node that is not private
        let mut open = tracker([0xee; 32], 16);
        assert_eq!(
            open.admit(meta(1, numbered_key(1), false, 0), now),
            Admit::Accept
        );
    }

    /// Private peers are dialed whenever they are dialable, beyond the outbound target, and
    /// back off after a failed dial.
    #[test]
    fn private_peers_are_redialed_with_back_off() {
        let t0 = Instant::now();
        let mut t = tracker([0xee; 32], 16);
        let keys = [numbered_key(1), numbered_key(2), numbered_key(1)];
        assert_eq!(
            t.private_dial_candidates(&keys, t0),
            [numbered_key(1), numbered_key(2)]
        );
        assert!(t.private_dial_candidates(&keys, t0).is_empty(), "dialing");
        t.on_dial_result(numbered_key(1), false, t0);
        t.on_dial_result(numbered_key(2), true, t0);
        assert_eq!(
            t.admit(meta(7, numbered_key(2), true, 0), t0),
            Admit::Accept
        );
        assert!(
            t.private_dial_candidates(&keys, t0).is_empty(),
            "backing off, connected"
        );
        let later = t0 + dial_backoff(0);
        assert_eq!(t.private_dial_candidates(&keys, later), [numbered_key(1)]);
        // a dropped connection is dialed again (the first key is being dialed)
        assert!(t.on_dropped(7));
        assert_eq!(t.private_dial_candidates(&keys, later), [numbered_key(2)]);
        // more private peers than the outbound target are all dialed
        let many: Vec<Key> = (10..10 + OUT_TARGET + 2).map(numbered_key).collect();
        assert_eq!(t.private_dial_candidates(&many, later).len(), many.len());
    }

    #[test]
    fn pending_inbound_does_not_block_outbound() {
        let local: Key = [0xee; 32];
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut t = tracker(local, 64);
        for i in 0..MAX_PENDING_INBOUND {
            assert_eq!(
                t.admit(meta(i as u64, numbered_key(i), false, 5), at(i as u64)),
                Admit::Accept
            );
        }
        // A new connection of the first key (lower hash wins) keeps the first admission time.
        assert_eq!(
            t.admit(meta(100, numbered_key(0), false, 0), at(50)),
            Admit::Replace { old: 0 }
        );
        let n = MAX_PENDING_INBOUND;
        assert_eq!(
            t.admit(meta(200, numbered_key(n), true, 0), at(60)),
            Admit::Accept,
            "outbound connections have their own pending limit"
        );
        assert_eq!(
            t.admit(meta(201, numbered_key(n + 1), false, 0), at(70)),
            Admit::AcceptEvicting { evicted: 100 }
        );
        assert_eq!(
            t.admit(meta(202, numbered_key(n + 2), false, 0), at(80)),
            Admit::AcceptEvicting { evicted: 1 }
        );
        assert_eq!(t.pending(false), MAX_PENDING_INBOUND);
        // The evicted connection's later failure is no offence and its drop changes nothing.
        let evicted = meta(1, numbered_key(1), false, 0);
        assert_eq!(
            t.on_handoff(evicted, Err(FailureKind::Protocol), at(90)),
            None
        );
        assert!(!t.on_dropped(1));
        assert!(!t.is_banned(&numbered_key(1), at(90)));
    }

    /// Inbound connections fill at most `--hyper-max-peers` minus the outbound share, and an
    /// address holds at most `MAX_PENDING_INBOUND_PER_IP` pending and `MAX_INBOUND_PER_IP`
    /// inbound connections.
    #[test]
    fn outbound_slots_are_reserved() {
        let local: Key = [0xee; 32];
        let now = Instant::now();
        let mut t = tracker(local, 12);
        assert_eq!(t.out_reserve(), OUT_TARGET.min(6));
        let inbound = t.inbound_cap();
        let mut conn = 0;
        while t.live.len() < inbound {
            let m = meta(conn, numbered_key(conn as usize), false, 0);
            assert_eq!(t.admit(m, now), Admit::Accept);
            assert_eq!(t.on_handoff(m, Ok(()), now), None);
            conn += 1;
        }
        assert_eq!(
            t.admit(meta(50, numbered_key(50), false, 0), now),
            Admit::Reject(RejectReason::Full)
        );
        assert!(
            t.wants_more(),
            "inbound peers do not count towards the outbound target"
        );
        for i in 0..t.out_reserve() {
            assert_eq!(
                t.admit(meta(60 + i as u64, numbered_key(60 + i), true, 0), now),
                Admit::Accept
            );
        }
        assert_eq!(t.connected(), 12);
        assert!(t.at_max_peers());

        // Per address.
        let mut t = tracker(local, 64);
        assert_eq!(
            t.admit(inbound_meta(1, [1; 32], "203.0.113.5"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(2, [2; 32], "203.0.113.5"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(3, [3; 32], "203.0.113.5"), now),
            Admit::Reject(RejectReason::PerAddress)
        );
        assert_eq!(
            t.admit(inbound_meta(4, [4; 32], "203.0.113.6"), now),
            Admit::Accept
        );
        for (id, key) in [(1, [1; 32]), (2, [2; 32])] {
            assert_eq!(
                t.on_handoff(inbound_meta(id, key, "203.0.113.5"), Ok(()), now),
                None
            );
        }
        assert_eq!(
            t.admit(inbound_meta(5, [5; 32], "203.0.113.5"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(6, [6; 32], "203.0.113.5"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(7, [7; 32], "203.0.113.5"), now),
            Admit::Reject(RejectReason::PerAddress),
            "four inbound connections from one address"
        );
        // One IPv6 /64 counts as one address; outbound connections are not limited per address.
        assert_eq!(
            t.admit(inbound_meta(8, [8; 32], "2001:db8:1:2::1"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(9, [9; 32], "2001:db8:1:2::2"), now),
            Admit::Accept
        );
        assert_eq!(
            t.admit(inbound_meta(10, [10; 32], "2001:db8:1:2:ffff::3"), now),
            Admit::Reject(RejectReason::PerAddress)
        );
        let out = ConnMeta {
            initiator: true,
            ..inbound_meta(11, [11; 32], "203.0.113.5")
        };
        assert_eq!(t.admit(out, now), Admit::Accept);
    }

    #[test]
    fn closing_a_duplicate_on_purpose_is_no_offence() {
        let local: Key = [0xff; 32];
        let remote: Key = [0x01; 32];
        let now = Instant::now();
        let mut t = tracker(local, 8);
        let old = meta(1, remote, false, 0);
        assert_eq!(t.admit(old, now), Admit::Accept);
        assert_eq!(
            t.admit(meta(2, remote, true, 9), now),
            Admit::Replace { old: 1 }
        );
        // the dropped stream may be reported before the handshake result
        assert!(!t.on_dropped(1));
        assert_eq!(t.on_handoff(old, Err(FailureKind::Protocol), now), None);
        assert_eq!(t.admit(meta(3, [9; 32], false, 0), now), Admit::Accept);
        assert!(!t.is_banned(&remote, now));
    }

    #[test]
    fn dials_need_two_sightings_and_back_off() {
        let local: Key = [0; 32];
        let now = Instant::now();
        let mut t = tracker(local, 32);
        let peers: Vec<Key> = (1..=6u8).map(|i| [i; 32]).collect();
        // first sighting: Hyperswarm gets a chance to auto-dial
        assert_eq!(t.dial_candidates(&peers, now), Vec::<Key>::new());
        let mut with_self = peers.clone();
        with_self.push(local);
        let dials = t.dial_candidates(&with_self, now);
        assert_eq!(dials.len(), MAX_CONCURRENT_DIALS);
        assert!(!dials.contains(&local));
        // in flight: not dialed again
        assert!(
            t.dial_candidates(&peers, now)
                .iter()
                .all(|k| !dials.contains(k))
        );
        // a failed dial backs off
        t.on_dial_result(dials[0], false, now);
        t.on_dial_result(dials[1], true, now);
        assert_eq!(t.admit(meta(1, dials[1], true, 0), now), Admit::Accept);
        let again = t.dial_candidates(&peers, now + Duration::from_secs(1));
        assert!(!again.contains(&dials[0]), "backing off");
        assert!(!again.contains(&dials[1]), "connected");
        assert_ne!(again, Vec::<Key>::new());
        for k in again.iter().chain(&dials[2..]) {
            t.on_dial_result(*k, false, now);
        }
        assert!(
            t.dial_candidates(&peers, now + dial_backoff(0) + Duration::from_secs(1))
                .contains(&dials[0])
        );
    }

    /// Stripe lookups dial at once (nobody auto-dials stripe topics), a few per lookup, never
    /// ourselves, connected, banned or backing-off keys, and also above the outbound target.
    #[test]
    fn stripe_dials_skip_the_second_sighting() {
        let local: Key = [0; 32];
        let now = Instant::now();
        let mut t = tracker(local, 32);
        // At the outbound target: plain lookups would not dial any more.
        for i in 0..OUT_TARGET as u8 {
            let key = [100 + i; 32];
            assert_eq!(
                t.admit(meta(u64::from(i) + 1, key, true, 0), now),
                Admit::Accept
            );
        }
        assert!(!t.wants_more());
        let mut found: Vec<Key> = vec![local, [100; 32]];
        found.extend((1..=6u8).map(|i| [i; 32]));
        let dials = t.stripe_dial_candidates(&found, now);
        assert_eq!(dials, vec![[1; 32], [2; 32]]);
        assert_eq!(dials.len(), STRIPE_DIALS);
        // In flight: not dialed again; a failure backs off.
        let next = t.stripe_dial_candidates(&found, now);
        assert_eq!(next, vec![[3; 32], [4; 32]]);
        t.on_dial_result([1; 32], false, now);
        let after = t.stripe_dial_candidates(&found, now);
        assert!(!after.contains(&[1; 32]), "backing off");
        // Never beyond `--hyper-max-peers`.
        let mut full = tracker(local, 1);
        assert_eq!(full.admit(meta(1, [100; 32], true, 0), now), Admit::Accept);
        assert_eq!(full.stripe_dial_candidates(&found, now).len(), 0);
    }

    /// At the peer cap a stripe lookup dials nobody but names the holders that wait for a slot
    /// (stripe rotation), once our established outbound connections fill the outbound target;
    /// below the cap, or when an inbound or a pending connection fills it, nothing waits.
    #[test]
    fn stripe_holders_wait_for_a_slot_at_the_cap() {
        let local: Key = [0; 32];
        let now = Instant::now();
        let mut t = tracker(local, 2);
        let dialed = meta(1, [100; 32], true, 0);
        assert_eq!(t.admit(dialed, now), Admit::Accept);
        assert_eq!(t.on_handoff(dialed, Ok(()), now), None);
        let found: Vec<Key> = vec![local, [100; 32], [1; 32], [2; 32], [3; 32]];
        assert_eq!(
            t.blocked_stripe_dials(&found, now).len(),
            0,
            "below the cap"
        );
        // An inbound connection fills the cap: others chose to connect, no rotation for it.
        let inbound = meta(2, [101; 32], false, 0);
        assert_eq!(t.admit(inbound, now), Admit::Accept);
        assert_eq!(t.on_handoff(inbound, Ok(()), now), None);
        assert_eq!(t.stripe_dial_candidates(&found, now).len(), 0);
        assert_eq!(
            t.blocked_stripe_dials(&found, now).len(),
            0,
            "an inbound peer fills the cap"
        );
        // An outbound connection whose Levin handshake is still running does not count either.
        assert!(t.on_dropped(2));
        let late = meta(3, [102; 32], true, 0);
        assert_eq!(t.admit(late, now), Admit::Accept);
        assert_eq!(
            t.blocked_stripe_dials(&found, now).len(),
            0,
            "a pending outbound peer fills the cap"
        );
        assert_eq!(t.on_handoff(late, Ok(()), now), None);
        // Never ourselves or a connected key; at most STRIPE_DIALS; nothing marked as dialing.
        assert_eq!(t.blocked_stripe_dials(&found, now), vec![[1; 32], [2; 32]]);
        assert_eq!(t.blocked_stripe_dials(&found, now), vec![[1; 32], [2; 32]]);
        t.bump_backoff([1; 32], now);
        assert_eq!(t.blocked_stripe_dials(&found, now), vec![[2; 32], [3; 32]]);
        // A connection ends: the waiting holders are dialed.
        assert!(t.on_dropped(3));
        assert_eq!(t.stripe_dial_candidates(&found, now), vec![[2; 32]]);
    }

    /// The keys of a stripe lookup are dialed in random order (a permutation of what was
    /// found), so the order of the DHT's answer does not decide which keys come first.
    #[test]
    fn stripe_lookups_dial_a_random_sample() {
        let mut t = tracker([0; 32], 32);
        let found: Vec<Key> = (1..=16u8).map(|i| [i; 32]).collect();
        let mut firsts = HashSet::new();
        for _ in 0..32 {
            let keys = t.sample_stripe_keys(&found);
            let mut sorted = keys.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, found);
            firsts.insert(keys[0]);
        }
        assert!(firsts.len() > 1, "always the same first key");
    }

    /// The holders of a height are looked up under every lookup count, each on the topic of its
    /// own stripe (the stripe numbers differ between counts).
    #[test]
    fn stripe_lookups_cover_four_and_six_stripes() {
        let span = xkr_storage::STRIPE_SPAN;
        let net = XkrNetwork::Mainnet;
        // Span 5: stripe 2 of 4, stripe 6 of 6.
        let looked = stripe_lookup_topics(Network::Mainnet, 5 * span + 10);
        assert_eq!(
            looked,
            vec![
                (
                    PruningSeed::new(4, 2).unwrap(),
                    topic::xkr_stripe_topic(net, 4, 2)
                ),
                (
                    PruningSeed::new(6, 6).unwrap(),
                    topic::xkr_stripe_topic(net, 6, 6)
                ),
            ]
        );
        // A full node is found on the default-count topic of every height.
        let full = held_stripe_topics(Network::Mainnet, PruningSeed::FULL);
        for h in [0, span, 5 * span, 11 * span + 1, 2_618_387] {
            let topics: Vec<Key> = stripe_lookup_topics(Network::Mainnet, h)
                .into_iter()
                .map(|(_, t)| t)
                .collect();
            assert_eq!(topics.len(), 2);
            assert!(full.contains(&topics[0]));
            // A pruned node of either count is found exactly when it keeps the height.
            for stripes in [4u8, 6] {
                for k in 1..=stripes {
                    let seed = PruningSeed::new(stripes, k).unwrap();
                    let held = held_stripe_topics(Network::Mainnet, seed);
                    assert_eq!(held.len(), 1);
                    assert_eq!(topics.contains(&held[0]), seed.keeps(h), "{seed} at {h}");
                }
            }
        }
        assert_ne!(
            stripe_lookup_topics(Network::Testnet, 0),
            stripe_lookup_topics(Network::Mainnet, 0)
        );
    }

    /// A full node announces every default stripe, a stripe-pruned one its own, a completely
    /// pruned one none; the stripe topics are per network and distinct from the blocks topic.
    #[test]
    fn stripe_topics_follow_the_seed() {
        let full = held_stripe_topics(Network::Mainnet, PruningSeed::FULL);
        assert_eq!(
            full,
            topic::xkr_stripe_topics(XkrNetwork::Mainnet, DEFAULT_STRIPES)
        );
        assert_eq!(full.len(), usize::from(DEFAULT_STRIPES));
        assert!(!full.contains(&blocks_topic(Network::Mainnet)));
        let seed = PruningSeed::new(DEFAULT_STRIPES, 2).unwrap();
        assert_eq!(
            held_stripe_topics(Network::Mainnet, seed),
            vec![topic::xkr_stripe_topic(
                XkrNetwork::Mainnet,
                DEFAULT_STRIPES,
                2
            )]
        );
        assert_eq!(stripe_topic(Network::Mainnet, seed), Some(full[1]));
        assert_eq!(stripe_topic(Network::Mainnet, PruningSeed::FULL), None);
        // A completely pruned node (`db prune --all`) holds no stripe and announces none.
        assert_eq!(
            stripe_topic(Network::Mainnet, PruningSeed::RECENT_ONLY),
            None
        );
        assert_eq!(
            held_stripe_topics(Network::Mainnet, PruningSeed::RECENT_ONLY).len(),
            0
        );
        assert_ne!(
            stripe_topic(Network::Testnet, seed),
            stripe_topic(Network::Mainnet, seed)
        );
        let six = PruningSeed::new(6, 5).unwrap();
        assert_eq!(
            stripe_topic(Network::Mainnet, six),
            Some(topic::xkr_stripe_topic(XkrNetwork::Mainnet, 6, 5))
        );
    }

    /// A start error is logged once; the same error five starts in a row turns Hyperswarm off,
    /// another error in between starts the count again, and a start that works resets it all.
    #[test]
    fn the_same_start_failure_five_times_stops_the_retries() {
        let addon = "cannot start the bridge: Bare worker 0: AddonError: CANNOT_LOAD";
        let closed = "cannot start the bridge: bridge closed: worker 0 closed during Hello";
        let mut f = StartFailures::default();
        assert_eq!(f.record(addon), StartVerdict::Log);
        assert_eq!(f.record(addon), StartVerdict::Quiet);
        assert_eq!(f.record(closed), StartVerdict::Log);
        // Back to the first error: already logged, counted from one again.
        for _ in 1..MAX_SAME_START_FAILURES {
            assert_eq!(f.record(addon), StartVerdict::Quiet);
        }
        assert_eq!(f.record(addon), StartVerdict::GiveUp);

        let mut g = StartFailures::default();
        for _ in 1..MAX_SAME_START_FAILURES {
            assert_ne!(g.record(closed), StartVerdict::GiveUp);
        }
        g.reset();
        assert_eq!(
            g.record(closed),
            StartVerdict::Log,
            "a working start resets the count"
        );
    }

    #[test]
    fn start_errors_and_the_off_line_read_well() {
        assert_eq!(
            start_error_text(&BridgeError::Spawn("Bare worker 0: AddonError: x".into())),
            "cannot start the bridge: Bare worker 0: AddonError: x"
        );
        assert_eq!(
            start_error_text(&BridgeError::Closed("worker 0 closed during Hello".into())),
            "cannot start the bridge: bridge closed: worker 0 closed during Hello"
        );
        assert_eq!(
            off_line(xkr_p2p_hyper::bare::CXX_RUNTIME_MISSING),
            "Hyperswarm off: libstdc++.so.6 not found (needed by the Hyperswarm addons). Install \
             it, or on NixOS add gcc's lib directory to LD_LIBRARY_PATH."
        );
    }

    #[test]
    fn dial_backoff_grows_and_caps() {
        assert_eq!(dial_backoff(0), Duration::from_secs(5));
        assert_eq!(dial_backoff(1), Duration::from_secs(10));
        assert_eq!(dial_backoff(3), Duration::from_secs(40));
        assert_eq!(dial_backoff(30), DIAL_BACKOFF_MAX);
    }

    #[test]
    fn lookup_period_follows_the_peer_count() {
        let mut t = tracker([0; 32], 2);
        assert_eq!(t.lookup_period(), LOOKUP_EMPTY);
        let now = Instant::now();
        t.admit(meta(1, [1; 32], true, 0), now);
        assert_eq!(t.lookup_period(), LOOKUP_LOW);
        t.admit(meta(2, [2; 32], true, 0), now);
        assert_eq!(t.lookup_period(), LOOKUP_FULL);
        assert!(!t.wants_more());
    }

    #[derive(Debug)]
    struct Wrapped(PeerError);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "handshake failed: {}", self.0)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn handshake_failures_are_classified() {
        assert_eq!(
            classify(&PeerError::WrongNetwork([0; 16])),
            FailureKind::Incompatible
        );
        assert_eq!(
            classify(&PeerError::VersionTooOld(1)),
            FailureKind::Incompatible
        );
        assert_eq!(classify(&PeerError::SelfConnection), FailureKind::Ourselves);
        assert_eq!(
            classify(&PeerError::Unexpected("second handshake")),
            FailureKind::Protocol
        );
        assert_eq!(classify(&PeerError::BadPing), FailureKind::Protocol);
        assert_eq!(
            classify(&PeerError::HandshakeTimeout),
            FailureKind::Transient
        );
        assert_eq!(classify(&PeerError::Closed), FailureKind::Transient);
        assert_eq!(
            classify(&PeerError::Io(io::Error::other("reset"))),
            FailureKind::Transient
        );
        assert_eq!(
            classify(&Wrapped(PeerError::WrongNetwork([1; 16]))),
            FailureKind::Incompatible
        );
        assert_eq!(
            classify(&io::Error::other("unrelated")),
            FailureKind::Transient
        );
    }

    #[test]
    fn config_from_the_command_line() {
        let args = Args::try_parse_from([
            "xkrd",
            "--network",
            "testnet",
            "--hyper-bootstrap",
            "127.0.0.1:49737",
            "--hyper-bootstrap",
            "10.0.0.1:1",
            "--hyper-udp-port",
            "21890",
            "--hyper-max-peers",
            "12",
            "--hyper-client-only",
            "--hyper-bridge-dir",
            "/opt/xkr/bridge",
        ])
        .expect("parse");
        let cfg = HyperConfig::from_args(&args, Network::Testnet, Path::new("/data"));
        assert_eq!(cfg.mode, AnnounceMode::Client);
        assert_eq!(cfg.max_peers, 12);
        let hugin = cfg.hugin.as_ref().expect("the relay is on by default");
        assert_eq!(hugin.dir, Path::new("/data").join(HUGIN_DIR));
        assert_eq!(
            hugin.identity_file(),
            Path::new("/data").join(HUGIN_DIR).join("identity.json")
        );
        assert_eq!(hugin.max_clients, 200);
        assert_eq!(hugin.sync_slots, xkr_hugin::relay::DEFAULT_SYNC_SLOTS);
        assert_eq!(hugin.queue_budget, 256 * 1024 * 1024);
        assert_eq!(
            hugin.p2pool_nodes,
            xkr_hugin::p2pool::default_nodes_checked().0
        );
        // the local p2pool first, unless another user holds its port on this machine
        if !matches!(
            xkr_hugin::p2pool::check_local_p2pool(),
            xkr_hugin::p2pool::LocalPortOwner::Other(_)
        ) {
            assert_eq!(hugin.p2pool_nodes[0], "127.0.0.1:3333");
        }
        assert_eq!(
            hugin.share_difficulty,
            xkr_hugin::relay::DEFAULT_APP_DIFFICULTY
        );
        assert_eq!(
            hugin.max_clients_per_ip,
            xkr_hugin::relay::DEFAULT_MAX_CLIENTS_PER_IP
        );
        assert!(!hugin.private_only);
        assert!(hugin.fee.is_none(), "main loads the fee address");
        assert_eq!(cfg.bridge.bootstrap, ["127.0.0.1:49737", "10.0.0.1:1"]);
        assert_eq!(cfg.bridge.udp_port, Some(21890));
        assert_eq!(cfg.bridge.max_peers, Some(12));
        assert_eq!(
            cfg.bridge.keypair_file.as_deref(),
            Some(Path::new("/data").join(KEYPAIR_FILE).as_path())
        );
        assert_eq!(
            cfg.bridge.bridge_dir.as_deref(),
            Some(Path::new("/opt/xkr/bridge"))
        );

        let plain = Args::try_parse_from(["xkrd"]).expect("parse");
        assert!(!plain.no_hyper);
        let cfg = HyperConfig::from_args(&plain, Network::Mainnet, Path::new("/data"));
        assert_eq!(cfg.mode, AnnounceMode::Both);
        assert!(
            cfg.bridge.bootstrap.is_empty(),
            "public DHT bootstrap by default"
        );
        assert!(cfg.hugin.is_some(), "plain xkrd runs the relay");
        let off = Args::try_parse_from(["xkrd", "--no-hugin"]).expect("parse");
        assert!(
            HyperConfig::from_args(&off, Network::Mainnet, Path::new("/d"))
                .hugin
                .is_none()
        );

        let custom = Args::try_parse_from([
            "xkrd",
            "--hugin-relay",
            "--hugin-p2pool",
            "127.0.0.1:3334",
            "--hugin-p2pool",
            "p2pool.example:3333",
            "--hugin-max-clients",
            "5",
            "--hugin-private",
            "--hugin-share-difficulty",
            "5000",
            "--hugin-max-clients-per-ip",
            "0",
            "--hugin-sync-slots",
            "0",
            "--hugin-queue-budget-mb",
            "64",
        ])
        .expect("parse");
        let h = HyperConfig::from_args(&custom, Network::Testnet, Path::new("/d"))
            .hugin
            .expect("hugin");
        assert_eq!(h.p2pool_nodes, ["127.0.0.1:3334", "p2pool.example:3333"]);
        assert_eq!(h.share_difficulty, 5000);
        assert_eq!(h.max_clients, 5);
        assert_eq!(h.max_clients_per_ip, 0);
        assert_eq!(h.sync_slots, 1, "at least one sync slot");
        assert_eq!(h.queue_budget, 64 * 1024 * 1024);
        assert!(h.private_only);
        assert!(!h.private_node);
    }

    /// `--private`: the public topics client-only (the XKR topics, the p2pool topic and the
    /// Hugin node mesh, which follow `mode`), no public or address-based Hugin topic; `main`
    /// loads the secret. `--private-peer` invites reach the configuration.
    #[test]
    fn private_config_from_the_command_line() {
        let invite = PrivateSecret {
            secret: [3; 32],
            identity: [4; 32],
        }
        .invite();
        let args =
            Args::try_parse_from(["xkrd", "--private", "--private-peer", &invite.to_string()])
                .expect("parse");
        let cfg = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"));
        assert_eq!(cfg.mode, AnnounceMode::Client);
        assert!(cfg.private.is_none(), "main loads the secret");
        assert_eq!(cfg.private_peers, [invite]);
        let h = cfg.hugin.expect("hugin");
        assert!(h.private_only);
        assert!(h.private_node);

        let peer_only =
            Args::try_parse_from(["xkrd", "--private-peer", &invite.to_string()]).expect("parse");
        let cfg = HyperConfig::from_args(&peer_only, Network::Mainnet, Path::new("/d"));
        assert_eq!(cfg.mode, AnnounceMode::Both);
        assert_eq!(cfg.private_peers, [invite]);
        assert!(!cfg.hugin.expect("hugin").private_node);
    }

    /// The secret file is created on the first start and read back; `renew` writes a new one,
    /// changes the invite and reaches the transport's subscription.
    #[test]
    fn the_private_node_keeps_and_renews_its_secret() {
        let dir = std::env::temp_dir().join(format!("xkrd-private-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = PrivateNode::load(&dir).expect("create");
        assert!(dir.join(PRIVATE_FILE).exists());
        let first = node.invite();
        assert_eq!(PrivateNode::load(&dir).expect("read").invite(), first);
        let mut rx = node.subscribe();
        let second = node.renew().expect("renew");
        assert_ne!(second, first);
        assert_ne!(second.secret, first.secret);
        assert_eq!(node.invite(), second);
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().invite(), second);
        assert_eq!(PrivateNode::load(&dir).expect("read").invite(), second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_bare_worker_for_every_topic_group() {
        let args = Args::try_parse_from(["xkrd", "--no-hugin"]).expect("parse");
        let cfg = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"));
        assert_eq!(cfg.bare_config().workers, 1, "XKR topics only");
        let args = Args::try_parse_from(["xkrd"]).expect("parse");
        let cfg = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"));
        assert!(cfg.hugin.is_some());
        assert_eq!(
            cfg.bare_config().workers,
            1,
            "XKR and Hugin topics share one DHT node"
        );
    }

    /// `--port` (and its alias `--hyper-udp-port`) reaches the one worker as `--port-fixed`, the
    /// port of the DHT node every topic group uses; without it the worker gets no fixed port.
    #[test]
    fn port_reaches_the_worker() {
        let pair = |args: &[String], flag: &str| {
            args.iter()
                .position(|x| x == flag)
                .map(|i| args[i + 1].clone())
        };
        for flag in ["--port", "--hyper-udp-port"] {
            let args = Args::try_parse_from(["xkrd", flag, "21890"]).expect("parse");
            assert_eq!(args.port, Some(21890), "{flag}");
            let cfg = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"));
            assert!(cfg.port_fixed());
            assert_eq!(cfg.bridge.udp_port, Some(21890));
            let workers = cfg.bare_config().workers;
            let worker = xkr_p2p_hyper::bare::worker_args(&cfg.bridge, 0, workers, &[0; 32]);
            assert_eq!(
                pair(&worker, "--port-fixed").as_deref(),
                Some("21890"),
                "{flag}"
            );
        }
        let args = Args::try_parse_from(["xkrd"]).expect("parse");
        assert_eq!(args.port, None);
        let cfg = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"));
        assert!(!cfg.port_fixed());
        let worker = xkr_p2p_hyper::bare::worker_args(&cfg.bridge, 0, 1, &[0; 32]);
        assert_eq!(
            pair(&worker, "--port-fixed"),
            None,
            "a free port by default"
        );
    }

    fn dht(state: u8, firewalled: bool, nat_seen: u8, udp_port: u16) -> DhtStatus {
        DhtStatus {
            state,
            firewalled,
            nat: nat_seen,
            udp_port,
            tlvs: Vec::new(),
        }
    }

    #[test]
    fn dht_status_for_the_status_route() {
        let starting = hyper_net_status(&dht(dht_state::STARTING, true, nat::UNKNOWN, 0), false);
        assert_eq!(starting, HyperNetStatus::default());
        let bound = hyper_net_status(&dht(dht_state::STARTING, true, nat::UNKNOWN, 49_737), false);
        assert_eq!(bound.udp_port, Some(49_737));
        assert_eq!(bound.firewalled, None, "only known once ready");

        let ready = hyper_net_status(&dht(dht_state::READY, true, nat::CONSISTENT, 21_890), true);
        assert_eq!(
            ready,
            HyperNetStatus {
                state: HyperDhtState::Ready,
                udp_port: Some(21_890),
                port_fixed: true,
                firewalled: Some(true),
                nat: HyperNat::Consistent,
                private: false,
            }
        );
        let failed = hyper_net_status(&dht(dht_state::FAILED, true, nat::UNKNOWN, 0), true);
        assert_eq!(failed.state, HyperDhtState::Failed);
        assert_eq!(failed.udp_port, None);
        assert_eq!(
            hyper_net_status(&dht(dht_state::READY, false, nat::RANDOM, 1), false).nat,
            HyperNat::Random
        );
        assert_eq!(
            hyper_net_status(&dht(9, false, 9, 1), false),
            HyperNetStatus {
                udp_port: Some(1),
                ..HyperNetStatus::default()
            },
            "unknown values read as starting / unknown"
        );
    }

    /// The `--port` hint: only a ready, firewalled node behind a NAT that changes the public
    /// port per destination, and only without `--port`.
    #[test]
    fn port_hint_only_behind_a_strict_nat_without_port() {
        let strict = hyper_net_status(&dht(dht_state::READY, true, nat::RANDOM, 49_737), false);
        assert!(wants_port_hint(strict));
        assert!(!wants_port_hint(HyperNetStatus {
            port_fixed: true,
            ..strict
        }));
        assert!(!wants_port_hint(HyperNetStatus {
            firewalled: Some(false),
            ..strict
        }));
        assert!(
            !wants_port_hint(HyperNetStatus {
                nat: HyperNat::Consistent,
                ..strict
            }),
            "an ordinary NAT holepunches"
        );
        assert!(!wants_port_hint(HyperNetStatus {
            state: HyperDhtState::Starting,
            ..strict
        }));
        assert!(PORT_HINT.contains("--port <port>"));
    }

    /// The relay's jobs wait for the node's sync state; a node without a P2P status (tests)
    /// counts as synchronized.
    #[tokio::test]
    async fn hugin_jobs_follow_the_sync_state() {
        let shutdown = CancellationToken::new();
        assert!(*follow_sync(None, &shutdown).borrow());
        let (tx, rx) = watch::channel(Arc::new(NetStatus::default()));
        let mut synced = follow_sync(Some(rx), &shutdown);
        assert!(!*synced.borrow());
        tx.send_replace(Arc::new(NetStatus {
            synced: true,
            downloading: true,
            ..NetStatus::default()
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!*synced.borrow(), "still downloading");
        tx.send_replace(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        tokio::time::timeout(Duration::from_secs(5), synced.wait_for(|s| *s))
            .await
            .unwrap()
            .unwrap();
        shutdown.cancel();
    }

    /// The relay's tip feed follows the main-chain top block of the P2P status.
    #[tokio::test]
    async fn hugin_tip_follows_the_main_chain() {
        let shutdown = CancellationToken::new();
        assert_eq!(*follow_tip(None, &shutdown).borrow(), None);
        let (tx, rx) = watch::channel(Arc::new(NetStatus::default()));
        let mut tip = follow_tip(Some(rx), &shutdown);
        assert_eq!(tip.borrow().map(|t| t.height), Some(0));
        tx.send_replace(Arc::new(NetStatus {
            top_index: 42,
            top_hash: [4; 32],
            ..NetStatus::default()
        }));
        let want = ChainTip {
            height: 42,
            hash: [4; 32],
        };
        tokio::time::timeout(Duration::from_secs(5), tip.wait_for(|t| *t == Some(want)))
            .await
            .unwrap()
            .unwrap();
        shutdown.cancel();
    }

    #[test]
    fn topics_are_per_network() {
        assert_eq!(
            blocks_topic(Network::Mainnet),
            topic::xkr_blocks_topic(XkrNetwork::Mainnet)
        );
        assert_ne!(
            blocks_topic(Network::Mainnet),
            blocks_topic(Network::Testnet)
        );
        // the Hugin topics are distinct from ours, so `on_conn` can tell them apart
        for network in [Network::Mainnet, Network::Testnet] {
            for j in hugin_topics(network, HUGIN_ADDRESS, &"0f".repeat(32), false, false) {
                assert_ne!(j.topic.topic, blocks_topic(Network::Mainnet));
                assert_ne!(j.topic.topic, blocks_topic(Network::Testnet));
            }
        }
        // the testnet relay has its own Hugin topics; mainnet keeps hugin-p2p 3.0's
        let main = hugin_topics(
            Network::Mainnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            false,
            false,
        );
        let test = hugin_topics(
            Network::Testnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            false,
            false,
        );
        assert_eq!(main[1].topic, HuginTopic::public_nodes());
        for (m, t) in main.iter().zip(&test) {
            assert_eq!(m.label, t.label);
            assert_ne!(m.topic.topic, t.topic.topic, "{}", m.label);
            assert_eq!(m.mid.topic_key, t.mid.topic_key);
        }
    }

    const HUGIN_ADDRESS: &str = "SEKReYU57DLLvUjNzmjVhaK7jqc8SdZZ3cyKJS5f4gWXK4NQQYChzKUUwzCGhgqUPkWQypeR94rqpgMPjXWG9ijnZKNw2LWXnZU";

    #[test]
    fn hugin_topics_and_roles() {
        let all = hugin_topics(
            Network::Mainnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            false,
            false,
        );
        let labels: Vec<_> = all
            .iter()
            .map(|j| (j.label, j.mode, j.role, j.after_sync))
            .collect();
        assert_eq!(
            labels,
            [
                ("hugin node mesh", AnnounceMode::Both, Role::Node, false),
                (PUBLIC_LABEL, AnnounceMode::Server, Role::Client, true),
                (PRIVATE_LABEL, AnnounceMode::Server, Role::Client, false),
            ]
        );
        assert_eq!(all[0].topic, HuginTopic::node_mesh());
        assert_eq!(all[2].topic, HuginTopic::private_node(HUGIN_ADDRESS));
        assert!(all[0].mid.identity_seed.is_none());
        assert_eq!(
            all[2].mid.identity_seed,
            Some(hugin_private_seed(&"0f".repeat(32)))
        );
        assert!(
            all.iter()
                .all(|j| j.mid.topic_key == Some(j.topic.topic_key))
        );
        let private = hugin_topics(
            Network::Mainnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            true,
            false,
        );
        assert_eq!(private.len(), 2);
        assert!(private.iter().all(|j| j.label != PUBLIC_LABEL));
        // `--private`: only the node mesh, client-only (apps come through the invite)
        let node = hugin_topics(
            Network::Mainnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            true,
            true,
        );
        let labels: Vec<_> = node.iter().map(|j| (j.label, j.mode, j.role)).collect();
        assert_eq!(
            labels,
            [("hugin node mesh", AnnounceMode::Client, Role::Node)]
        );
    }

    /// The private-node DHT key equals hugin-node's for the same view key (vector from
    /// `bridge/test/hugin-vectors.js`, computed with kryptokrona-utils and sodium).
    #[test]
    fn hugin_private_seed_matches_hugin_node() {
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/test-vectors/hugin-relay.json"))
                .expect("vectors");
        let id = &v["identity"];
        let seed = hugin_private_seed(id["viewkey"].as_str().expect("viewkey"));
        assert_eq!(hex::encode(seed), id["dht_seed"].as_str().expect("seed"));
        assert_eq!(
            hex::encode(topic::ed25519_public_from_seed(&seed)),
            id["dht_public_key"].as_str().expect("key")
        );
    }

    /// The p2pool sidechain topic: its key is the XKR topic of
    /// `xkr/<network>/p2pool/v1/<hex(cn_fast_hash("p2pool-topic" || consensus id))>` (spelled out
    /// here), the DHT topic the Hugin-style topic of that key, so the bridge accepts it as an
    /// identity topic; one per network and sidechain.
    #[test]
    fn p2pool_topic_from_the_consensus_id() {
        let id = xkr_p2pool::SidechainConfig::main().consensus_id();
        let mut inner = b"p2pool-topic".to_vec();
        inner.extend_from_slice(&id.0);
        let name = format!(
            "xkr/mainnet/p2pool/v1/{}",
            hex::encode(xkr_crypto::cn_fast_hash(&inner).0)
        );
        let t = p2pool_topic(Network::Mainnet, &id);
        assert_eq!(t.topic_key, topic::xkr_topic(&name));
        assert_eq!(t.key, hex::encode(t.topic_key));
        assert_eq!(
            t.base_public_key,
            topic::ed25519_public_from_seed(&t.topic_key)
        );
        assert_eq!(t.topic, topic::hugin_topic(&t.base_public_key));
        assert_eq!(t.mid().topic_key, Some(t.topic_key));
        let testnet = p2pool_topic(Network::Testnet, &id);
        assert_ne!(testnet.topic_key, t.topic_key);
        assert_eq!(
            testnet.topic,
            topic::hugin_topic_on(XkrNetwork::Testnet, &testnet.base_public_key)
        );
        let mini = p2pool_topic(
            Network::Mainnet,
            &xkr_p2pool::SidechainConfig::mini().consensus_id(),
        );
        assert_ne!(mini.topic, t.topic);
        // never one of the node's other topics
        assert_ne!(t.topic, blocks_topic(Network::Mainnet));
        for j in hugin_topics(
            Network::Mainnet,
            HUGIN_ADDRESS,
            &"0f".repeat(32),
            false,
            false,
        ) {
            assert_ne!(j.topic.topic, t.topic);
        }
    }

    /// With p2pool on the relay uses the built-in stratum server, as given or on loopback
    /// when it listens on every interface, then the public nodes; `--hugin-p2pool` still wins.
    #[test]
    fn relay_uses_the_built_in_p2pool() {
        let args = Args::try_parse_from(["xkrd", "--node", "--hugin", "--p2pool"]).expect("parse");
        let h = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"))
            .hugin
            .expect("hugin");
        assert_eq!(h.p2pool_nodes[0], "127.0.0.1:3333");
        assert_eq!(
            h.p2pool_nodes.len(),
            1 + xkr_hugin::p2pool::PUBLIC_P2POOL_NODES.len()
        );
        let args =
            Args::try_parse_from(["xkrd", "--p2pool-stratum", "0.0.0.0:3400"]).expect("parse");
        let h = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"))
            .hugin
            .expect("hugin");
        assert_eq!(h.p2pool_nodes[0], "127.0.0.1:3400");
        let args = Args::try_parse_from([
            "xkrd",
            "--node",
            "--hugin",
            "--p2pool",
            "--hugin-p2pool",
            "pool.example:3333",
        ])
        .expect("parse");
        let h = HyperConfig::from_args(&args, Network::Mainnet, Path::new("/d"))
            .hugin
            .expect("hugin");
        assert_eq!(h.p2pool_nodes, ["pool.example:3333"]);
    }
}
