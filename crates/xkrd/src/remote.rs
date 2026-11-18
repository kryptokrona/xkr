//! The main chain from remote nodes (`xkrd --hugin` / `--p2pool` without `--node`):
//! [`RemoteNodes`] keeps connections to xkrd nodes on the Hyperswarm clients topic
//! (`xkr/<network>/clients/v1`, [`crate::clients`], `docs/hyper-transport.md` "Clients topic")
//! as a client, is the built-in p2pool's [`MainChainSource`] and gives the Hugin relay its sync
//! state and tip (`/status` `remote`, `crate::hyper::follow_remote_sync`).
//!
//! * **Connections**: up to [`MAX_NODES`] nodes with distinct DHT keys, found by a lookup of
//!   the clients topic and dialed in client mode (the topic is never announced), each with the
//!   hello `service: "node"`. A node that closes, stops answering (a `getinfo` every
//!   [`PING_INTERVAL`] while no tip arrives) or fails a dial is dropped and dialed again only
//!   after a back-off (5 s doubling to 10 min). A free place is filled by the next lookup.
//! * **State hashes**: a node whose hello names a stable span with a state hash other than the
//!   built-in one (`xkr_chain::state`), or, above the built-in list, other than the one most of
//!   the connected nodes name for the same span, is dropped and not dialed for
//!   [`CHECK_BAN`].
//! * **Tip**: the highest tip that at least two connected nodes have or build on (a node above
//!   it is asked for its header at that height); with one node, that node's tip. Before a new
//!   tip is used, the headers since the last one (at most [`CHECKED_PER_TIP`]) are read from a
//!   node at the tip and checked: each names the one before as its previous block, and each
//!   block (`get_block_blob`) hashes to its header's id and meets its header's difficulty with
//!   its proof of work (one slow hash per block). The nodes that have a tip that fails the check
//!   are dropped and not dialed for [`CHECK_BAN`].
//! * **Miner data** comes from a node whose tip is the tip in use; **found blocks** go to every
//!   connected node (`submitblock`).
//! * **Synchronized** (the p2pool and miner gate, [`RemoteNodes::net_status`]): at least one
//!   connected node answers and says it is synchronized, and a node confirmed the tip in use
//!   less than [`STALE_AFTER`] ago.
//!
//! * **Stratum** ([`RemoteStratum`]): the Hugin relay's last job source. A stratum session on a
//!   connected node that runs p2pool (`stratum_open`, `stratum`, pushes `stratum`), over the
//!   same connection as the chain (Hyperswarm keeps one connection per key on a topic); nodes
//!   whose hello lists `stratum` and that have the tip in use are tried first.
//!
//! [`run_hyper`] is the Hyperswarm transport of this mode: the Bare workers with the Hugin
//! topics (the public-node topic only while the remote nodes are synchronized) and the p2pool
//! sidechain topic announced, and the clients topic looked up and dialed ([`BridgeDialer`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use xkr_chain::{MainChainHeader, MinerData, StateHashes, TipInfo, unix_now};
use xkr_consensus::{Network, NetworkParams, check_proof_of_work};
use xkr_hugin::PeerMeta;
use xkr_hugin::pool::{PoolConnector, PoolReader, PoolWriter};
use xkr_hyper_rpc::{Client, ClientOptions, Hello, Push, Service};
use xkr_p2p_hyper::{BareWorkers, Bridge, CloseHandle, CloseReason, HuginTopic, OpenFlags, topic};
use xkr_rpc::{HyperDhtState, HyperNetStatus, NetStatus, RemoteNodeStatus, RemoteStatus};
use xkr_txpool::PoolTxSummary;
use xkr_types::{Block, Hash, TxInput};

use crate::clients::{STRATUM_CLOSE, STRATUM_CLOSED, STRATUM_MESSAGE, STRATUM_OPEN};
use crate::hyper::{self, HuginRuntime, HyperConfig, P2poolTopic};
use crate::p2pool::{
    BlockOnChain, MainChainSource, MainChainState, Refusal, apply_headers, header_ranges,
};
use crate::term;

/// Nodes kept connected.
pub const MAX_NODES: usize = 3;
/// The tip counts as old (not synchronized) when no node confirmed it for this long.
pub const STALE_AFTER: Duration = Duration::from_mins(3);
/// A node from which nothing arrived for this long is asked for `getinfo`.
pub const PING_INTERVAL: Duration = Duration::from_secs(60);
/// A node that sent nothing and answered nothing for this long is dropped.
const SILENT_AFTER: Duration = Duration::from_mins(3);
/// How long a node whose chain or state hash failed a check is not dialed.
pub const CHECK_BAN: Duration = Duration::from_hours(1);
/// Newest blocks whose proof of work is checked when the tip moves.
pub const CHECKED_PER_TIP: u64 = 4;
/// Housekeeping: pings, dropped nodes, lookups, the status.
const TICK: Duration = Duration::from_secs(5);
/// Between lookups while places are free (sooner without any node).
const LOOKUP_INTERVAL: Duration = Duration::from_secs(30);
const LOOKUP_EMPTY: Duration = Duration::from_secs(10);
/// Peers asked for per lookup.
const LOOKUP_MAX: u16 = 32;
const DIAL_BACKOFF_BASE: Duration = Duration::from_secs(5);
const DIAL_BACKOFF_MAX: Duration = Duration::from_mins(10);
/// Headers asked for per `get_block_headers_range` call (the node answers at most 1 000).
const HEADERS_PER_CALL: u64 = 500;
/// Transactions remembered with the time they were first seen in miner data.
const MAX_SEEN_TXS: usize = 4_096;
/// The Hugin relay's p2pool entry for the stratum service of the remote nodes
/// ([`RemoteStratum`]; `crate::hyper::remote_relay_nodes`).
pub const REMOTE_STRATUM: &str = "hyperswarm";
/// Longest stratum line taken from the relay's pool client.
const STRATUM_LINE: u64 = 64 * 1024;
/// Buffer between the relay's pool client and a stratum session.
const STRATUM_PIPE: usize = 16 * 1024;
/// How long closing a stratum session may take.
const STRATUM_CLOSE_WAIT: Duration = Duration::from_secs(5);

/// A DHT public key.
pub type Key = [u8; 32];

/// A byte stream to a node (a Hyperswarm connection, or an in-memory one in tests).
pub trait RemoteStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> RemoteStream for T {}

/// A boxed [`RemoteStream`].
pub type BoxedStream = Box<dyn RemoteStream>;

/// How [`RemoteNodes`] finds and reaches nodes.
pub trait Dialer: Send + Sync + 'static {
    /// Keys of nodes on the clients topic.
    fn lookup(&self) -> BoxFuture<'static, Result<Vec<Key>, String>>;
    /// A stream to the node with `key`.
    fn dial(&self, key: Key) -> BoxFuture<'static, Result<BoxedStream, String>>;
}

/// Settings of [`RemoteNodes`].
#[derive(Debug, Clone)]
pub struct RemoteConfig {
    pub network: Network,
    /// The hello's `client` (`xkrd/<version>`).
    pub client: String,
    /// The built-in state hashes nodes are checked against.
    pub state_hashes: StateHashes,
    /// Nodes kept connected ([`MAX_NODES`]).
    pub max_nodes: usize,
    /// Settings of each connection.
    pub client_options: ClientOptions,
}

impl RemoteConfig {
    /// The defaults for `network`, with the built-in state hashes.
    #[must_use]
    pub fn new(network: Network, version: &str) -> Self {
        Self {
            network,
            client: format!("xkrd/{version}"),
            state_hashes: StateHashes::for_network(network),
            max_nodes: MAX_NODES,
            client_options: ClientOptions {
                call_timeout: Duration::from_secs(15),
                ..ClientOptions::default()
            },
        }
    }
}

/// A main-chain header as the nodes send it (`getblockheaderbyheight`,
/// `get_block_headers_range`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub height: u64,
    pub hash: Hash,
    pub prev: Hash,
    pub timestamp: u64,
    pub difficulty: u64,
}

impl Header {
    /// From a header object of the node's JSON-RPC.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let num = |k: &str| {
            v.get(k)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("header without {k}"))
        };
        let hash = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<Hash>().ok())
                .ok_or_else(|| format!("header without {k}"))
        };
        Ok(Self {
            height: num("height")?,
            hash: hash("hash")?,
            prev: hash("prev_hash")?,
            timestamp: num("timestamp")?,
            difficulty: num("difficulty")?,
        })
    }

    /// As p2pool reads it (the cumulative difficulty is not used there and stays 0).
    #[must_use]
    pub const fn main_chain(&self) -> MainChainHeader {
        MainChainHeader {
            height: self.height,
            hash: self.hash,
            timestamp: self.timestamp,
            difficulty: self.difficulty,
            cumulative_difficulty: 0,
        }
    }
}

/// Why a tip was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// The chain the nodes sent does not hold together (linkage, id, proof of work).
    Chain(String),
    /// The node did not answer as needed (closed, timed out, an error).
    Unreachable(String),
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::Chain(e) => write!(f, "chain check failed: {e}"),
            CheckError::Unreachable(e) => write!(f, "node did not answer: {e}"),
        }
    }
}

/// Checks headers `headers` (ascending, contiguous) ending in the tip `tip`: heights follow
/// each other, each names the one before as its previous block, the last is the tip.
pub fn check_linkage(headers: &[Header], tip: (u64, Hash)) -> Result<(), String> {
    let last = headers.last().ok_or("no headers")?;
    if (last.height, last.hash) != tip {
        return Err(format!(
            "the header at {} is not the tip {}",
            last.height, tip.1
        ));
    }
    for w in headers.windows(2) {
        if w[1].height != w[0].height + 1 {
            return Err(format!(
                "headers jump from {} to {}",
                w[0].height, w[1].height
            ));
        }
        if w[1].prev != w[0].hash {
            return Err(format!(
                "the block at {} does not name the block at {} as its previous block",
                w[1].height, w[0].height
            ));
        }
    }
    Ok(())
}

/// Checks a block (`get_block_blob`) against its header: the same id, previous block and
/// timestamp, and a proof of work that meets the header's difficulty. One slow hash: run it on
/// a blocking thread.
pub fn check_block(header: &Header, blob: &[u8], genesis: &Hash) -> Result<(), String> {
    let at = header.height;
    let block = xkr_serialization::decode_block(blob)
        .map_err(|e| format!("block at {at} does not decode: {e}"))?;
    let id = xkr_serialization::block_hash(&block)
        .map_err(|e| format!("block at {at} does not hash: {e}"))?;
    if id != header.hash {
        return Err(format!(
            "block at {at} hashes to {id}, its header says {}",
            header.hash
        ));
    }
    if block.header.prev_hash != header.prev || block.header.timestamp != header.timestamp {
        return Err(format!("block at {at} does not match its header"));
    }
    if header.difficulty == 0 {
        return Err(format!("header at {at} has difficulty 0"));
    }
    check_proof_of_work(&block, header.difficulty, genesis)
        .map_err(|e| format!("block at {at}: proof of work: {e}"))
}

/// The keys of the nodes whose stable state hash differs from the built-in one, or, for a span
/// above the built-in list, from the one more than half of the nodes that name that span name
/// (with at least two of them).
#[must_use]
pub fn state_outliers(nodes: &[(Key, Option<(u64, Hash)>)], builtin: &StateHashes) -> Vec<Key> {
    let mut out = Vec::new();
    let mut by_span: HashMap<u64, Vec<(Key, Hash)>> = HashMap::new();
    for (key, state) in nodes {
        let Some((span, hash)) = state else {
            continue;
        };
        match builtin.matches(*span, hash) {
            Some(true) => {}
            Some(false) => out.push(*key),
            None => by_span.entry(*span).or_default().push((*key, *hash)),
        }
    }
    for list in by_span.values() {
        let mut counts: HashMap<Hash, usize> = HashMap::new();
        for (_, h) in list {
            *counts.entry(*h).or_default() += 1;
        }
        let Some((major, n)) = counts.iter().max_by_key(|(_, n)| **n) else {
            continue;
        };
        if *n >= 2 && *n * 2 > list.len() {
            out.extend(list.iter().filter(|(_, h)| h != major).map(|(k, _)| *k));
        }
    }
    out
}

/// The hello's state hash as `(span, hash)`; `Err` when it names one that is not a hash.
fn hello_state(client: &Client) -> Result<Option<(u64, Hash)>, String> {
    let r = client.hello();
    let (Some(span), Some(h)) = (r.state_span, &r.state_hash) else {
        return Ok(None);
    };
    h.parse::<Hash>()
        .map(|h| Some((span, h)))
        .map_err(|_| "the hello's state hash is not a hash".to_owned())
}

/// `get_miner_data` as [`MinerData`]; transactions get the time they were first seen.
fn miner_data_from_json(
    v: &Value,
    seen: &mut HashMap<Hash, u64>,
    now: u64,
) -> Result<MinerData, String> {
    let num = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("miner data without {k}"))
    };
    let difficulty = v
        .get("difficulty")
        .and_then(Value::as_str)
        .and_then(|d| u64::from_str_radix(d, 16).ok())
        .ok_or("miner data without difficulty")?;
    let prev_id = v
        .get("prev_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Hash>().ok())
        .ok_or("miner data without prev_id")?;
    let mut tx_backlog = Vec::new();
    for t in v
        .get("tx_backlog")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let hash = t
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Hash>().ok())
            .ok_or("miner data: transaction without id")?;
        let size = t.get("weight").and_then(Value::as_u64).unwrap_or(0);
        let fee = t.get("fee").and_then(Value::as_u64).unwrap_or(0);
        if seen.len() >= MAX_SEEN_TXS && !seen.contains_key(&hash) {
            seen.clear();
        }
        let received = *seen.entry(hash).or_insert(now);
        tx_backlog.push(PoolTxSummary {
            hash,
            size: usize::try_from(size).unwrap_or(usize::MAX),
            fee,
            received,
        });
    }
    Ok(MinerData {
        major_version: u8::try_from(num("major_version")?).map_err(|_| "major version")?,
        height: num("height")?,
        prev_id,
        difficulty,
        median_weight: num("median_weight")?,
        already_generated_coins: num("already_generated_coins")?,
        median_timestamp: num("median_timestamp")?,
        unlock_window: num("unlock_window")?,
        tx_backlog,
    })
}

/// A node's tip as `(height, hash)` (`None` while its hash is not a hash).
fn tip_of(client: &Client) -> Option<(u64, Hash, bool)> {
    let t = client.tips().borrow().clone();
    t.hash.parse::<Hash>().ok().map(|h| (t.height, h, t.synced))
}

fn short(key: &Key) -> String {
    hex::encode(&key[..8])
}

/// A connected node.
struct Node {
    key: Key,
    client: Client,
    /// Last tip push or answer.
    heard: Instant,
    /// A `getinfo` is on its way.
    pinging: bool,
    state: Option<(u64, Hash)>,
    watcher: AbortHandle,
}

/// The tip in use.
#[derive(Debug, Clone)]
struct Chosen {
    header: Header,
    /// The nodes that have it or build on it.
    supporters: Vec<Key>,
}

#[derive(Default)]
struct State {
    nodes: Vec<Node>,
    dialing: HashSet<Key>,
    /// Key -> (no dial before, failed dials in a row).
    backoff: HashMap<Key, (Instant, u32)>,
    banned: HashMap<Key, Instant>,
    chosen: Option<Chosen>,
    /// Checked headers of the newest blocks.
    checked: BTreeMap<u64, Header>,
    /// When a node last confirmed the tip in use.
    confirmed: Option<Instant>,
    seen_txs: HashMap<Hash, u64>,
    last_lookup: Option<Instant>,
    lookup_running: bool,
}

enum Ev {
    Looked(Result<Vec<Key>, String>),
    Dialed {
        key: Key,
        result: Result<Client, String>,
    },
    Tip {
        key: Key,
    },
    Closed {
        key: Key,
    },
    Pinged {
        key: Key,
        ok: bool,
    },
    Reselect,
}

struct Inner {
    cfg: RemoteConfig,
    params: &'static NetworkParams,
    genesis: Hash,
    state: Mutex<State>,
    tip_tx: watch::Sender<TipInfo>,
    net_tx: watch::Sender<Arc<NetStatus>>,
    status_tx: watch::Sender<Arc<RemoteStatus>>,
    events: mpsc::UnboundedSender<Ev>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<Ev>>>,
    /// One tip selection at a time.
    selecting: tokio::sync::Mutex<()>,
}

/// The main chain from remote nodes (module docs). Cheap to clone.
#[derive(Clone)]
pub struct RemoteNodes {
    inner: Arc<Inner>,
}

impl fmt::Debug for RemoteNodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteNodes")
            .field("network", &self.inner.cfg.network)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl RemoteNodes {
    /// No node yet; [`RemoteNodes::run`] connects.
    ///
    /// # Panics
    /// Never: the genesis block of a built-in network always hashes.
    #[must_use]
    pub fn new(cfg: RemoteConfig) -> Self {
        let params = cfg.network.params();
        let genesis = xkr_consensus::genesis_hash(params).expect("the genesis block hashes");
        let (events, rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                cfg,
                params,
                genesis,
                state: Mutex::new(State::default()),
                tip_tx: watch::channel(TipInfo::default()).0,
                net_tx: watch::channel(Arc::new(NetStatus::default())).0,
                status_tx: watch::channel(Arc::new(RemoteStatus::default())).0,
                events,
                events_rx: Mutex::new(Some(rx)),
                selecting: tokio::sync::Mutex::new(()),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, ev: Ev) {
        let _ = self.inner.events.send(ev);
    }

    /// The sync state as p2pool and the built-in miner read it: `synced`, the tip in use as
    /// `top_index` / `top_hash`, the highest node tip as `network_height`.
    #[must_use]
    pub fn net_status(&self) -> watch::Receiver<Arc<NetStatus>> {
        self.inner.net_tx.subscribe()
    }

    /// `/status` `remote`.
    #[must_use]
    pub fn subscribe_status(&self) -> watch::Receiver<Arc<RemoteStatus>> {
        self.inner.status_tx.subscribe()
    }

    /// The status now.
    #[must_use]
    pub fn status(&self) -> RemoteStatus {
        self.status_at(Instant::now())
    }

    /// Whether the main chain counts as synchronized at `now` (module docs).
    #[cfg(test)]
    #[must_use]
    pub fn synced_at(&self, now: Instant) -> bool {
        let st = self.state();
        Self::synced_in(&st, now)
    }

    fn synced_in(st: &State, now: Instant) -> bool {
        let fresh = |t: Instant| now.saturating_duration_since(t) < STALE_AFTER;
        st.chosen.is_some()
            && st.confirmed.is_some_and(fresh)
            && st.nodes.iter().any(|n| {
                !n.client.is_closed()
                    && fresh(n.heard)
                    && tip_of(&n.client).is_some_and(|(_, _, synced)| synced)
            })
    }

    fn status_at(&self, now: Instant) -> RemoteStatus {
        let st = self.state();
        let chosen = st.chosen.as_ref();
        RemoteStatus {
            nodes: st
                .nodes
                .iter()
                .map(|n| {
                    let tip = tip_of(&n.client);
                    RemoteNodeStatus {
                        key: short(&n.key),
                        height: tip.map_or(0, |t| t.0),
                        synced: tip.is_some_and(|t| t.2),
                        on_tip: chosen.is_some_and(|c| c.supporters.contains(&n.key)),
                    }
                })
                .collect(),
            height: chosen.map(|c| c.header.height),
            hash: chosen.map(|c| c.header.hash.to_string()),
            synced: Self::synced_in(&st, now),
            tip_age: st
                .confirmed
                .map(|t| now.saturating_duration_since(t).as_secs()),
        }
    }

    /// Publishes the status and the sync state.
    fn publish(&self) {
        let now = Instant::now();
        let status = self.status_at(now);
        let network_height = {
            let st = self.state();
            st.nodes
                .iter()
                .filter_map(|n| tip_of(&n.client))
                .map(|t| t.0 + 1)
                .max()
                .unwrap_or(0)
        };
        let (top_index, top_hash) = {
            let st = self.state();
            st.chosen
                .as_ref()
                .map_or((0, [0; 32]), |c| (c.header.height, c.header.hash.0))
        };
        let net = NetStatus {
            hyper: status.nodes.len(),
            network_height,
            top_index,
            top_hash,
            synced: status.synced,
            downloading: false,
            ..NetStatus::default()
        };
        self.inner.net_tx.send_if_modified(|old| {
            let changed = old.synced != net.synced
                || old.top_index != net.top_index
                || old.top_hash != net.top_hash
                || old.network_height != net.network_height
                || old.hyper != net.hyper;
            if changed {
                *old = Arc::new(net);
            }
            changed
        });
        self.inner.status_tx.send_if_modified(|old| {
            if **old == status {
                false
            } else {
                *old = Arc::new(status);
                true
            }
        });
    }

    /// Waits until a tip is in use (`Some` with the status), `timeout` passes or `stop` is
    /// cancelled (`None`).
    pub async fn wait_first(
        &self,
        timeout: Duration,
        stop: &CancellationToken,
    ) -> Option<RemoteStatus> {
        let mut rx = self.subscribe_status();
        tokio::select! {
            () = stop.cancelled() => None,
            r = tokio::time::timeout(timeout, rx.wait_for(|s| s.height.is_some())) => {
                r.ok().and_then(Result::ok).map(|s| (**s).clone())
            }
        }
    }

    /// Keeps the connections and the tip until `stop`, then closes every connection.
    pub async fn run(&self, dialer: &dyn Dialer, stop: &CancellationToken) {
        let taken = self
            .inner
            .events_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut events) = taken else {
            tracing::error!("remote nodes: run called twice");
            return;
        };
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                Some(ev) = events.recv() => self.on_event(ev, dialer).await,
                _ = tick.tick() => self.on_tick(dialer),
            }
        }
        let nodes = std::mem::take(&mut self.state().nodes);
        for n in nodes {
            n.watcher.abort();
        }
        self.publish();
    }

    async fn on_event(&self, ev: Ev, dialer: &dyn Dialer) {
        match ev {
            Ev::Looked(result) => {
                self.state().lookup_running = false;
                match result {
                    Ok(keys) => self.dial_some(&keys, dialer),
                    Err(e) => tracing::debug!(error = %e, "remote nodes: lookup failed"),
                }
            }
            Ev::Dialed { key, result } => {
                self.state().dialing.remove(&key);
                match result {
                    Ok(client) => {
                        if self.admit(key, client) {
                            self.select().await;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(node = %short(&key), error = %e, "remote nodes: dial failed");
                        self.back_off(key);
                    }
                }
            }
            Ev::Tip { key } => {
                if let Some(n) = self.state().nodes.iter_mut().find(|n| n.key == key) {
                    n.heard = Instant::now();
                }
                self.select().await;
            }
            Ev::Closed { key } => {
                if self.drop_node(&key) {
                    tracing::info!(node = %short(&key), "remote node closed the connection");
                    self.back_off(key);
                    self.select().await;
                }
            }
            Ev::Pinged { key, ok } => {
                let now = Instant::now();
                let mut st = self.state();
                if let Some(n) = st.nodes.iter_mut().find(|n| n.key == key) {
                    n.pinging = false;
                    if ok {
                        n.heard = now;
                    }
                }
                if ok
                    && st
                        .chosen
                        .as_ref()
                        .is_some_and(|c| c.supporters.contains(&key))
                {
                    st.confirmed = Some(now);
                }
                drop(st);
                self.publish();
            }
            Ev::Reselect => self.select().await,
        }
    }

    /// Housekeeping every [`TICK`].
    fn on_tick(&self, dialer: &dyn Dialer) {
        let now = Instant::now();
        let mut silent = Vec::new();
        let mut pings = Vec::new();
        let mut lookup = false;
        {
            let mut st = self.state();
            st.banned.retain(|_, until| *until > now);
            st.backoff
                .retain(|_, (until, _)| now.saturating_duration_since(*until) < DIAL_BACKOFF_MAX);
            for n in &mut st.nodes {
                let quiet = now.saturating_duration_since(n.heard);
                if n.client.is_closed() || quiet >= SILENT_AFTER {
                    silent.push(n.key);
                } else if quiet >= PING_INTERVAL && !n.pinging {
                    n.pinging = true;
                    pings.push((n.key, n.client.clone()));
                }
            }
            let open = st.nodes.len() + st.dialing.len();
            let wait = if st.nodes.is_empty() {
                LOOKUP_EMPTY
            } else {
                LOOKUP_INTERVAL
            };
            if open < self.inner.cfg.max_nodes
                && !st.lookup_running
                && st
                    .last_lookup
                    .is_none_or(|t| now.saturating_duration_since(t) >= wait)
            {
                st.lookup_running = true;
                st.last_lookup = Some(now);
                lookup = true;
            }
        }
        for key in &silent {
            if self.drop_node(key) {
                tracing::info!(node = %short(key), "remote node silent; dropped");
                self.back_off(*key);
            }
        }
        if !silent.is_empty() {
            self.send(Ev::Reselect);
        }
        for (key, client) in pings {
            let this = self.clone();
            tokio::spawn(async move {
                let ok = client.call("getinfo", json!({})).await.is_ok();
                this.send(Ev::Pinged { key, ok });
            });
        }
        if lookup {
            let this = self.clone();
            let found = dialer.lookup();
            tokio::spawn(async move {
                let result = found.await;
                this.send(Ev::Looked(result));
            });
        }
        self.publish();
    }

    /// Dials found nodes into the free places.
    fn dial_some(&self, keys: &[Key], dialer: &dyn Dialer) {
        let now = Instant::now();
        let picks: Vec<Key> = {
            let mut st = self.state();
            let free = self
                .inner
                .cfg
                .max_nodes
                .saturating_sub(st.nodes.len() + st.dialing.len());
            let picks: Vec<Key> = keys
                .iter()
                .copied()
                .filter(|k| {
                    !st.nodes.iter().any(|n| n.key == *k)
                        && !st.dialing.contains(k)
                        && st.banned.get(k).is_none_or(|until| *until <= now)
                        && st.backoff.get(k).is_none_or(|(until, _)| *until <= now)
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .take(free)
                .collect();
            st.dialing.extend(picks.iter().copied());
            picks
        };
        for key in picks {
            let this = self.clone();
            let stream = dialer.dial(key);
            let hello = Hello::new(
                hyper::xkr_network(self.inner.cfg.network).as_str(),
                Service::Node,
                &self.inner.cfg.client,
            );
            let options = self.inner.cfg.client_options;
            tokio::spawn(async move {
                let result = match stream.await {
                    Ok(s) => Client::connect_with(s, hello, options)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                };
                this.send(Ev::Dialed { key, result });
            });
        }
    }

    /// Takes a connected node unless its hello disagrees with the state hashes; `true` when
    /// taken.
    fn admit(&self, key: Key, client: Client) -> bool {
        if !client.hello().serves(Service::Node) {
            tracing::debug!(node = %short(&key), "remote node does not serve `node`");
            self.back_off(key);
            return false;
        }
        let state = match hello_state(&client) {
            Ok(s) => s,
            Err(e) => {
                self.ban(key, &e);
                return false;
            }
        };
        if state_outliers(&[(key, state)], &self.inner.cfg.state_hashes).contains(&key) {
            self.ban(key, "its state hash differs from the built-in one");
            return false;
        }
        let watcher = {
            let this = self.clone();
            let mut tips = client.tips();
            tokio::spawn(async move {
                while tips.changed().await.is_ok() {
                    this.send(Ev::Tip { key });
                }
                this.send(Ev::Closed { key });
            })
            .abort_handle()
        };
        let outliers = {
            let mut st = self.state();
            if st.nodes.iter().any(|n| n.key == key) || st.nodes.len() >= self.inner.cfg.max_nodes {
                watcher.abort();
                return false;
            }
            st.backoff.remove(&key);
            st.nodes.push(Node {
                key,
                client,
                heard: Instant::now(),
                pinging: false,
                state,
                watcher,
            });
            let states: Vec<(Key, Option<(u64, Hash)>)> =
                st.nodes.iter().map(|n| (n.key, n.state)).collect();
            state_outliers(&states, &self.inner.cfg.state_hashes)
        };
        for k in &outliers {
            self.ban(*k, "its state hash differs from the other nodes'");
        }
        let taken = !outliers.contains(&key);
        if taken {
            tracing::info!(node = %short(&key), "remote node connected");
        }
        taken
    }

    /// Removes a node; `true` if it was connected.
    fn drop_node(&self, key: &Key) -> bool {
        let mut st = self.state();
        let Some(i) = st.nodes.iter().position(|n| n.key == *key) else {
            return false;
        };
        let n = st.nodes.remove(i);
        n.watcher.abort();
        if let Some(c) = st.chosen.as_mut() {
            c.supporters.retain(|k| k != key);
        }
        true
    }

    fn back_off(&self, key: Key) {
        let mut st = self.state();
        let attempts = st.backoff.get(&key).map_or(0, |b| b.1);
        let delay = DIAL_BACKOFF_BASE
            .saturating_mul(1 << attempts.min(10))
            .min(DIAL_BACKOFF_MAX);
        st.backoff
            .insert(key, (Instant::now() + delay, attempts.saturating_add(1)));
    }

    /// Drops `key` and does not dial it for [`CHECK_BAN`].
    fn ban(&self, key: Key, why: &str) {
        tracing::warn!(node = %short(&key), reason = why, "remote node dropped");
        self.drop_node(&key);
        self.state().banned.insert(key, Instant::now() + CHECK_BAN);
    }

    /// The tip selection (module docs); publishes the result.
    async fn select(&self) {
        let _one = self.inner.selecting.lock().await;
        let mut nodes: Vec<(Key, Client, (u64, Hash), bool)> = self
            .state()
            .nodes
            .iter()
            .filter_map(|n| tip_of(&n.client).map(|t| (n.key, n.client.clone(), (t.0, t.1), t.2)))
            .collect();
        // Nodes that are still downloading the chain have an old tip: they count only while
        // no node is synchronized.
        if nodes.iter().any(|n| n.3) {
            nodes.retain(|n| n.3);
        }
        if nodes.is_empty() {
            self.publish();
            return;
        }
        let mut candidates: Vec<(u64, Hash)> = nodes.iter().map(|n| n.2).collect();
        candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
        candidates.dedup();
        let mut pick = None;
        if nodes.len() == 1 {
            pick = Some((nodes[0].2, vec![nodes[0].0]));
        } else {
            for cand in candidates {
                let mut support = Vec::new();
                for (key, client, tip, _) in &nodes {
                    if *tip == cand
                        || (tip.0 > cand.0 && hash_at(client, cand.0).await == Some(cand.1))
                    {
                        support.push(*key);
                    }
                }
                if support.len() >= 2 {
                    pick = Some((cand, support));
                    break;
                }
            }
        }
        let Some((tip, supporters)) = pick else {
            tracing::debug!("remote nodes: no tip that two nodes share");
            self.publish();
            return;
        };
        let same = self
            .state()
            .chosen
            .as_ref()
            .is_some_and(|c| (c.header.height, c.header.hash) == tip);
        if same {
            let mut st = self.state();
            if let Some(c) = st.chosen.as_mut() {
                c.supporters = supporters;
            }
            st.confirmed = Some(Instant::now());
            drop(st);
            self.publish();
            return;
        }
        // A node at exactly the tip serves the check.
        let Some(at) = nodes
            .iter()
            .find(|n| n.2 == tip && supporters.contains(&n.0))
            .map(|n| (n.0, n.1.clone()))
        else {
            self.publish();
            return;
        };
        match self.check_tip(&at.1, tip).await {
            Ok(checked) => {
                let header = *checked.last().unwrap_or(&Header {
                    height: tip.0,
                    hash: tip.1,
                    prev: Hash::ZERO,
                    timestamp: 0,
                    difficulty: 0,
                });
                {
                    let mut st = self.state();
                    for h in &checked {
                        st.checked.insert(h.height, *h);
                    }
                    // Only the newest few are needed for the next linkage check.
                    while st.checked.len() > 64 {
                        st.checked.pop_first();
                    }
                    st.checked.retain(|h, _| *h <= tip.0);
                    st.chosen = Some(Chosen { header, supporters });
                    st.confirmed = Some(Instant::now());
                }
                tracing::debug!(height = tip.0, hash = %tip.1, "remote nodes: tip");
                self.inner.tip_tx.send_replace(TipInfo {
                    height: header.height,
                    hash: header.hash,
                    timestamp: header.timestamp,
                    difficulty: header.difficulty,
                    ..TipInfo::default()
                });
                self.publish();
            }
            Err(CheckError::Chain(e)) => {
                for k in &supporters {
                    self.ban(*k, &e);
                }
                self.send(Ev::Reselect);
            }
            Err(CheckError::Unreachable(e)) => {
                if self.drop_node(&at.0) {
                    tracing::info!(node = %short(&at.0), error = %e, "remote node dropped");
                    self.back_off(at.0);
                }
                self.send(Ev::Reselect);
            }
        }
    }

    /// Reads the headers up to `tip` from `client` that are newer than the last checked one
    /// (at most [`CHECKED_PER_TIP`], plus the one before them) and checks linkage, ids and
    /// proofs of work; returns the checked headers, the tip last.
    async fn check_tip(
        &self,
        client: &Client,
        tip: (u64, Hash),
    ) -> Result<Vec<Header>, CheckError> {
        let newest_checked = self
            .state()
            .checked
            .range(..tip.0)
            .next_back()
            .map(|(h, _)| *h);
        let lowest = tip.0.saturating_sub(CHECKED_PER_TIP - 1);
        let start = newest_checked.map_or(lowest, |h| (h + 1).max(lowest));
        let from = start.saturating_sub(1);
        let headers = headers_range(client, from, tip.0)
            .await
            .map_err(CheckError::Unreachable)?;
        check_linkage(&headers, tip).map_err(CheckError::Chain)?;
        for h in headers.iter().filter(|h| h.height >= start.max(1)) {
            let blob = block_blob(client, h.height)
                .await
                .map_err(CheckError::Unreachable)?;
            let (header, genesis) = (*h, self.inner.genesis);
            tokio::task::spawn_blocking(move || check_block(&header, &blob, &genesis))
                .await
                .map_err(|e| CheckError::Unreachable(e.to_string()))?
                .map_err(CheckError::Chain)?;
        }
        Ok(headers)
    }

    /// A node whose tip is the tip in use, with that tip.
    fn at_tip(&self) -> Option<(Client, Header)> {
        let st = self.state();
        let chosen = st.chosen.as_ref()?;
        let tip = (chosen.header.height, chosen.header.hash);
        st.nodes
            .iter()
            .filter(|n| !n.client.is_closed())
            .find(|n| tip_of(&n.client).is_some_and(|t| (t.0, t.1) == tip))
            .map(|n| (n.client.clone(), chosen.header))
    }

    /// The connected nodes for a stratum session, best first: nodes whose hello lists
    /// `stratum`, then the others (p2pool may have started after the hello); among them those
    /// with the tip in use first.
    #[must_use]
    pub fn stratum_clients(&self) -> Vec<Client> {
        let st = self.state();
        let on_tip = |k: &Key| st.chosen.as_ref().is_some_and(|c| c.supporters.contains(k));
        let mut nodes: Vec<(bool, bool, Client)> = st
            .nodes
            .iter()
            .filter(|n| !n.client.is_closed())
            .map(|n| {
                (
                    !n.client.hello().serves(Service::Stratum),
                    !on_tip(&n.key),
                    n.client.clone(),
                )
            })
            .collect();
        nodes.sort_by_key(|n| (n.0, n.1));
        nodes.into_iter().map(|n| n.2).collect()
    }

    /// Miner data at the tip in use, and the client that sent it.
    async fn miner_data(&self) -> Result<(MinerData, Client, Header), String> {
        for attempt in 0..2 {
            let Some((client, tip)) = self.at_tip() else {
                if attempt == 0 {
                    self.select().await;
                    continue;
                }
                return Err("no remote node at the main-chain tip".to_owned());
            };
            let v = client
                .call("get_miner_data", json!({}))
                .await
                .map_err(|e| format!("get_miner_data: {e}"))?;
            let data = {
                let mut st = self.state();
                miner_data_from_json(&v, &mut st.seen_txs, unix_now())?
            };
            if data.prev_id == tip.hash && data.height == tip.height + 1 {
                return Ok((data, client, tip));
            }
            // The node moved on since its last push: choose again.
            self.select().await;
        }
        Err("the remote nodes' tip moved while reading miner data".to_owned())
    }
}

/// The hash of `client`'s main-chain block at `height`.
async fn hash_at(client: &Client, height: u64) -> Option<Hash> {
    let v = client
        .call("getblockheaderbyheight", json!({ "height": height }))
        .await
        .ok()?;
    v.get("block_header")
        .and_then(|h| h.get("hash"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
}

/// Headers `from..=to` from `client`, in calls of [`HEADERS_PER_CALL`].
async fn headers_range(client: &Client, from: u64, to: u64) -> Result<Vec<Header>, String> {
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let end = to.min(start + HEADERS_PER_CALL - 1);
        let v = client
            .call(
                "get_block_headers_range",
                json!({ "start_height": start, "end_height": end }),
            )
            .await
            .map_err(|e| format!("get_block_headers_range: {e}"))?;
        let list = v
            .get("headers")
            .and_then(Value::as_array)
            .ok_or("get_block_headers_range: no headers")?;
        for (h, want) in list.iter().zip(start..=end) {
            let h = Header::from_json(h)?;
            if h.height != want {
                return Err(format!("asked for the header at {want}, got {}", h.height));
            }
            out.push(h);
        }
        if (list.len() as u64) < end - start + 1 {
            return Err(format!("short header answer for {start}..={end}"));
        }
        start = end + 1;
    }
    Ok(out)
}

/// `get_block_blob` at `height`.
async fn block_blob(client: &Client, height: u64) -> Result<Vec<u8>, String> {
    let v = client
        .call(crate::clients::GET_BLOCK_BLOB, json!({ "height": height }))
        .await
        .map_err(|e| format!("get_block_blob: {e}"))?;
    v.get("blob")
        .and_then(Value::as_str)
        .and_then(|s| hex::decode(s).ok())
        .ok_or_else(|| "get_block_blob: no blob".to_owned())
}

/// The coinbase height of a block.
fn coinbase_height(block: &Block) -> Option<u64> {
    if let Some(TxInput::Coinbase { height }) = block.miner_tx.prefix.inputs.first() {
        Some(u64::from(*height))
    } else {
        None
    }
}

impl MainChainSource for RemoteNodes {
    fn params(&self) -> &'static NetworkParams {
        self.inner.params
    }

    fn subscribe_tip(&self) -> watch::Receiver<TipInfo> {
        self.inner.tip_tx.subscribe()
    }

    fn refresh(
        &self,
        main: Arc<MainChainState>,
        read_to: Option<u64>,
    ) -> BoxFuture<'static, Result<(MinerData, u64), String>> {
        let this = self.clone();
        async move {
            let (data, client, tip) = this.miner_data().await?;
            let mut headers = Vec::new();
            for (start, end) in header_ranges(&main, &data, read_to) {
                // `[start, end)`, at most up to the tip in use.
                let last = end.min(tip.height + 1);
                if start >= last {
                    continue;
                }
                let got = headers_range(&client, start, last - 1).await?;
                headers.extend(got.iter().map(Header::main_chain));
            }
            if headers
                .last()
                .is_some_and(|h| h.hash != tip.hash && h.height == tip.height)
            {
                return Err("the remote node's headers do not end in the tip".to_owned());
            }
            let top = apply_headers(&main, &data, &headers);
            Ok((data, top))
        }
        .boxed()
    }

    fn submit_block(&self, blob: Vec<u8>) -> BoxFuture<'static, Result<BlockOnChain, Refusal>> {
        let this = self.clone();
        async move {
            let refused = |reason: String| Refusal {
                stale: false,
                reason,
            };
            let block = xkr_serialization::decode_block(&blob)
                .map_err(|e| refused(format!("not a block: {e}")))?;
            let id = xkr_serialization::block_hash(&block)
                .map_err(|e| refused(format!("not a block: {e}")))?;
            let height = coinbase_height(&block).ok_or_else(|| refused("no coinbase".into()))?;
            let (clients, tip) = {
                let st = this.state();
                let clients: Vec<(Key, Client)> = st
                    .nodes
                    .iter()
                    .map(|n| (n.key, n.client.clone()))
                    .collect();
                (clients, st.chosen.as_ref().map(|c| c.header.hash))
            };
            if clients.is_empty() {
                return Err(refused("no remote node connected".into()));
            }
            let hex_blob = hex::encode(&blob);
            let answers = futures::future::join_all(clients.iter().map(|(key, c)| {
                let params = json!([hex_blob]);
                async move { (*key, c.call("submitblock", params).await) }
            }))
            .await;
            let mut first_error = None;
            let mut taken = 0usize;
            for (key, a) in &answers {
                match a {
                    Ok(_) => taken += 1,
                    Err(e) => {
                        tracing::debug!(node = %short(key), error = %e, "remote node refused the found block");
                        first_error.get_or_insert_with(|| e.to_string());
                    }
                }
            }
            tracing::info!(
                height,
                %id,
                nodes = answers.len(),
                taken,
                "p2pool: found block sent to the remote nodes"
            );
            if taken > 0 {
                return Ok((height, id));
            }
            Err(Refusal {
                // Not on the tip in use: it came too late.
                stale: tip.is_some_and(|t| t != block.header.prev_hash),
                reason: first_error.unwrap_or_else(|| "refused".into()),
            })
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------------------------
// stratum over the clients topic

/// The Hugin relay's [`PoolConnector`] for [`REMOTE_STRATUM`]: a stratum session in the p2pool
/// of a connected remote node (`docs/hyper-transport.md`, "Service stratum"), as a line stream
/// for the relay's pool client. Each connection tries the next node first.
#[derive(Debug)]
pub struct RemoteStratum {
    nodes: RemoteNodes,
    next: AtomicUsize,
}

impl RemoteStratum {
    #[must_use]
    pub fn new(nodes: RemoteNodes) -> Self {
        Self {
            nodes,
            next: AtomicUsize::new(0),
        }
    }
}

impl PoolConnector for RemoteStratum {
    fn serves(&self, pool: &str) -> bool {
        pool == REMOTE_STRATUM
    }

    fn open(&self, _pool: &str) -> BoxFuture<'static, Result<(PoolReader, PoolWriter), String>> {
        let clients = self.nodes.stratum_clients();
        let first = self.next.fetch_add(1, Ordering::Relaxed);
        async move {
            if clients.is_empty() {
                return Err("no remote node connected".to_owned());
            }
            let mut last = String::new();
            for i in 0..clients.len() {
                let client = clients[(first + i) % clients.len()].clone();
                match open_stratum(client).await {
                    Ok(rw) => return Ok(rw),
                    Err(e) => last = e,
                }
            }
            Err(format!("no remote node opened a stratum session ({last})"))
        }
        .boxed()
    }
}

/// Opens a stratum session on `client`'s node and returns the relay's side of it.
async fn open_stratum(client: Client) -> Result<(PoolReader, PoolWriter), String> {
    // Subscribed before the session exists, so its first lines are not missed.
    let pushes = client.pushes();
    let v = client
        .call(STRATUM_OPEN, json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let session = v
        .get("session")
        .and_then(Value::as_u64)
        .ok_or("the node sent no session number")?;
    let (ours, theirs) = tokio::io::duplex(STRATUM_PIPE);
    tokio::spawn(pump_stratum(client, session, theirs, pushes));
    let (r, w) = tokio::io::split(ours);
    Ok((Box::new(r), Box::new(w)))
}

/// Carries one stratum session: the pool client's lines as `stratum` calls (one at a time, in
/// order), the session's pushes as lines. Ends when either side ends; then the session is
/// closed on the node.
async fn pump_stratum(
    client: Client,
    session: u64,
    stream: tokio::io::DuplexStream,
    mut pushes: broadcast::Receiver<Push>,
) {
    let (rd, mut wr) = tokio::io::split(stream);
    let up = {
        let client = client.clone();
        async move {
            let mut reader = tokio::io::BufReader::new(rd);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match (&mut reader)
                    .take(STRATUM_LINE)
                    .read_until(b'\n', &mut buf)
                    .await
                {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if buf.last() != Some(&b'\n') {
                    return;
                }
                let Ok(message) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                let params = json!({ "session": session, "message": message });
                if client.call(STRATUM_MESSAGE, params).await.is_err() {
                    return;
                }
            }
        }
    };
    let down = async move {
        loop {
            // Lost lines: start again with a new session.
            let Ok(push) = pushes.recv().await else {
                return;
            };
            if push.params.get("session").and_then(Value::as_u64) != Some(session) {
                continue;
            }
            if push.method == STRATUM_CLOSED {
                return;
            }
            if push.method != STRATUM_MESSAGE {
                continue;
            }
            let Some(message) = push.params.get("message") else {
                continue;
            };
            let mut line = message.to_string().into_bytes();
            line.push(b'\n');
            if wr.write_all(&line).await.is_err() {
                return;
            }
        }
    };
    tokio::select! {
        () = up => {}
        () = down => {}
        () = client.closed() => {}
    }
    let _ = tokio::time::timeout(
        STRATUM_CLOSE_WAIT,
        client.call(STRATUM_CLOSE, json!({ "session": session })),
    )
    .await;
}

/// The "Remote nodes" start-up step's detail: `3 nodes, height 2 641 203`.
#[must_use]
pub fn step_detail(status: &RemoteStatus) -> String {
    let n = status.nodes.len();
    let nodes = if n == 1 { "node" } else { "nodes" };
    match status.height {
        Some(h) => format!("{n} {nodes}, height {}", term::group(h)),
        None => format!("{n} {nodes}, no common tip yet"),
    }
}

// ---------------------------------------------------------------------------------------------
// the Hyperswarm transport without the node

/// Unannouncing gets this long at the stop.
const UNANNOUNCE_STOP: Duration = Duration::from_secs(3);
/// The Bare workers get this long to leave the DHT at the stop.
const WORKERS_STOP: Duration = Duration::from_secs(5);
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
const ANNOUNCE_RETRY: Duration = Duration::from_secs(10);

/// [`Dialer`] on the Hyperswarm clients topic: a lookup of the topic, dials in client mode with
/// the topic's `mid` (never announced). Follows bridge restarts.
#[derive(Debug, Clone)]
pub struct BridgeDialer {
    bridge: watch::Receiver<Option<Bridge>>,
    topic: HuginTopic,
}

impl BridgeDialer {
    /// The public clients topic of `network` on the bridge that `bridge` holds.
    #[must_use]
    pub fn new(bridge: watch::Receiver<Option<Bridge>>, network: Network) -> Self {
        Self {
            bridge,
            topic: topic::xkr_clients_topic(hyper::xkr_network(network)),
        }
    }

    fn current(&self) -> Result<Bridge, String> {
        self.bridge
            .borrow()
            .clone()
            .ok_or_else(|| "the Hyperswarm transport is not up".to_owned())
    }
}

impl Dialer for BridgeDialer {
    fn lookup(&self) -> BoxFuture<'static, Result<Vec<Key>, String>> {
        let bridge = self.current();
        let topic = self.topic.topic;
        async move {
            let peers = bridge?
                .lookup(topic, LOOKUP_MAX)
                .await
                .map_err(|e| e.to_string())?;
            Ok(peers.iter().map(|p| p.public_key).collect())
        }
        .boxed()
    }

    fn dial(&self, key: Key) -> BoxFuture<'static, Result<BoxedStream, String>> {
        let bridge = self.current();
        let (topic, mid) = (self.topic.topic, self.topic.mid());
        async move {
            let conn = bridge?
                .open_hugin(key, topic, OpenFlags::NONE, &mid)
                .await
                .map_err(|e| e.to_string())?;
            Ok(Box::new(conn) as BoxedStream)
        }
        .boxed()
    }
}

/// What [`run_hyper`] runs.
#[derive(Debug)]
pub struct RemoteHyper {
    /// Bridge settings and announce mode (`--hyper-client-only`: the p2pool topic in client
    /// mode too), the Hugin relay (`cfg.hugin`, with `cfg.remote` as its sync state and tip).
    pub cfg: HyperConfig,
    /// The built-in p2pool's sidechain topic.
    pub p2pool: Option<P2poolTopic>,
    /// The running bridge, for [`BridgeDialer`] (`None` while it is down).
    pub bridge: watch::Sender<Option<Bridge>>,
    /// The DHT node, for `/status` `hyper`.
    pub dht: watch::Sender<Option<HyperNetStatus>>,
}

enum End {
    Shutdown,
    Failed(String),
}

/// The Hyperswarm transport without the node until `shutdown`: starts the Hugin relay (once,
/// across bridge restarts) and the Bare workers (restarted with back-off, 1 s doubling to
/// 60 s), announces the relay's topics ([`HuginRuntime::spawn_announces`]) and the p2pool
/// sidechain topic once p2pool runs, hands the connections on them to the relay and to p2pool,
/// and offers the bridge to the [`BridgeDialer`]. At the stop: the topics are left, the relay's
/// connections closed, the workers leave the DHT.
pub async fn run_hyper(h: RemoteHyper, shutdown: CancellationToken) {
    let port_fixed = h.cfg.port_fixed();
    if let Err(reason) = xkr_p2p_hyper::bare::check_runtime() {
        h.dht.send_replace(Some(HyperNetStatus {
            state: HyperDhtState::Failed,
            port_fixed,
            ..HyperNetStatus::default()
        }));
        tracing::error!(%reason, "Hyperswarm transport off: remote nodes cannot be reached");
        return;
    }
    let hugin = h.cfg.hugin.as_ref().and_then(|settings| {
        HuginRuntime::start(
            settings,
            h.cfg.network,
            h.cfg.hugin_stats.clone(),
            &shutdown,
            None,
            h.cfg.remote.as_ref(),
        )
        .map_err(|e| tracing::error!(error = %e, "Hugin relay disabled"))
        .ok()
    });
    let mut delay = RESTART_MIN;
    loop {
        let started = tokio::select! {
            () = shutdown.cancelled() => break,
            r = h.cfg.start_bridge() => r,
        };
        let at = Instant::now();
        let why = match started {
            Ok((bridge, host)) => {
                match session(&h, hugin.as_ref(), bridge, host, &shutdown).await {
                    End::Shutdown => break,
                    End::Failed(why) => why,
                }
            }
            Err(e) => e.to_string(),
        };
        h.bridge.send_replace(None);
        if at.elapsed() >= Duration::from_mins(2) {
            delay = RESTART_MIN;
        }
        tracing::warn!(error = %why, retry_in_s = delay.as_secs(), "Hyperswarm transport down; restarting the bridge");
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(RESTART_MAX);
    }
    h.bridge.send_replace(None);
    tracing::info!("Hyperswarm transport stopped");
}

/// A Hugin topic announced (label, local key).
type Announced = (&'static str, Option<Key>);

async fn session(
    h: &RemoteHyper,
    hugin: Option<&HuginRuntime>,
    bridge: Bridge,
    mut host: BareWorkers,
    shutdown: &CancellationToken,
) -> End {
    let Some(mut incoming) = bridge.incoming() else {
        bridge.close();
        return End::Failed("incoming connections already taken".into());
    };
    let mut dht = bridge.dht_status(0);
    tracing::info!(
        node_key = %bridge.node_public_key().map_or_default(hex::encode),
        hugin = hugin.is_some(),
        p2pool = h.p2pool.is_some(),
        "Hyperswarm bridge up (no node: Hugin and p2pool topics, clients topic as a client)"
    );
    h.bridge.send_replace(Some(bridge.clone()));
    let (announced_tx, mut announced_rx) = mpsc::unbounded_channel::<Announced>();
    if let Some(rt) = hugin {
        rt.spawn_announces(&bridge, &announced_tx, |label, key| (label, key));
    }
    // The relay's connections: closed at the stop.
    let mut hugin_handles: HashMap<u64, CloseHandle> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();
    let p2pool_topic = h.p2pool.as_ref().map(|p| p.topic.topic);
    if let Some(p) = &h.p2pool {
        let bridge = bridge.clone();
        let (topic, mid, mode) = (p.topic.topic, p.topic.mid(), h.cfg.mode);
        let mut ready = p.ready.clone();
        tokio::spawn(async move {
            if ready.wait_for(|r| *r).await.is_err() {
                return;
            }
            loop {
                match bridge.announce(topic, mode, Some(mid.clone())).await {
                    Ok(_) => {
                        tracing::info!("p2pool topic announced on the DHT");
                        return;
                    }
                    Err(e) => {
                        if bridge.error().is_some() {
                            return;
                        }
                        tracing::warn!(error = %e, "announcing the p2pool topic failed; retrying");
                        tokio::time::sleep(ANNOUNCE_RETRY).await;
                    }
                }
            }
        });
    }
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let step = crate::stopping::step("Hyperswarm announcements");
                let hugin_topics = hugin.map_or_default(HuginRuntime::topic_keys);
                let left = tokio::time::timeout(UNANNOUNCE_STOP, async {
                    if let Some(t) = p2pool_topic {
                        let _ = bridge.unannounce(t).await;
                    }
                    for t in &hugin_topics {
                        let _ = bridge.unannounce(*t).await;
                    }
                })
                .await;
                match left {
                    Ok(()) => step.done_with("topics left"),
                    Err(_) => step.timed_out(UNANNOUNCE_STOP),
                }
                if hugin.is_some() {
                    let step = crate::stopping::step("Hugin relay");
                    let apps = hugin_handles.len();
                    for (_, c) in hugin_handles.drain() {
                        c.close(CloseReason::Normal);
                    }
                    step.done_with(format!("{apps} connections closed"));
                }
                h.bridge.send_replace(None);
                let step = crate::stopping::step("Hyperswarm Bare workers");
                let at = Instant::now();
                bridge.close();
                host.stop(WORKERS_STOP).await;
                if at.elapsed() >= WORKERS_STOP {
                    step.timed_out(WORKERS_STOP);
                } else {
                    step.done_with("exited");
                }
                return End::Shutdown;
            }
            err = bridge.wait_closed() => {
                host.start_kill();
                return End::Failed(format!("bridge failed: {err}"));
            }
            why = host.wait() => {
                bridge.close();
                return End::Failed(why);
            }
            Some((label, key)) = announced_rx.recv() => {
                if let Some(rt) = hugin {
                    rt.log_announced(label, key);
                }
            }
            Some(conn_id) = done_rx.recv() => {
                hugin_handles.remove(&conn_id);
            }
            item = incoming.next() => match item {
                Some(Ok(conn)) => {
                    let topic = conn.info().topic;
                    let role = topic.and_then(|t| hugin.and_then(|rt| rt.role_of(&t)));
                    if let (Some(rt), Some(role)) = (hugin, role) {
                        let info = conn.info();
                        let meta = PeerMeta {
                            role,
                            remote_key: info.remote_public_key,
                            remote_ip: info.addr.as_ref().map(SocketAddr::ip),
                        };
                        let conn_id = info.conn_id;
                        hugin_handles.insert(conn_id, conn.close_handle());
                        let relay = rt.relay().clone();
                        let done = done_tx.clone();
                        tokio::spawn(async move {
                            relay.serve(conn, meta).await;
                            let _ = done.send(conn_id);
                        });
                        continue;
                    }
                    match h.p2pool.as_ref().filter(|p| topic == Some(p.topic.topic)) {
                        Some(p) => {
                            if p.conns.send(conn).is_err() {
                                tracing::debug!("p2pool connection while p2pool is not running; closed");
                            }
                        }
                        None => conn.close(CloseReason::Normal),
                    }
                }
                Some(Err(e)) => {
                    bridge.close();
                    host.start_kill();
                    return End::Failed(format!("bridge failed: {e}"));
                }
                None => {
                    bridge.close();
                    host.start_kill();
                    return End::Failed("bridge closed".into());
                }
            },
            open = async {
                match dht.as_mut() {
                    Some(rx) => rx.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => {
                if !open {
                    dht = None;
                    continue;
                }
                let status = dht.as_mut().and_then(|rx| rx.borrow_and_update().clone());
                if let Some(d) = status {
                    h.dht.send_replace(Some(hyper::hyper_net_status(&d, h.cfg.port_fixed())));
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use futures::stream::BoxStream;
    use xkr_chain::{AddBlockOutcome, Chain, ChainConfig, MinerAddress, set_merge_mining_tag};
    use xkr_hyper_rpc::{
        Accepted, Handler, HelloReply, MAX_FRAME, Push, RpcError, ServerLimits, VERSION,
    };
    use xkr_rpc::{ClientStats, MiningStatus, NodeInfo, RpcState};
    use xkr_txpool::{PoolConfig, TxPool};
    use xkr_types::RawBlock;

    use super::*;
    use crate::clients::ClientsServer;

    const WAIT: Duration = Duration::from_secs(30);

    /// A testnet chain in a temporary directory, removed on drop.
    pub(crate) struct TestChain {
        pub dir: PathBuf,
        chain: Option<Arc<Chain>>,
    }

    impl TestChain {
        pub fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkrd-remote-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store =
                xkr_storage::Store::open_with_cache(dir.join("chain.redb"), 16 << 20).unwrap();
            let pool = Arc::new(TxPool::new(PoolConfig::default()));
            let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).unwrap();
            Self {
                dir,
                chain: Some(Arc::new(chain)),
            }
        }

        pub fn chain(&self) -> &Arc<Chain> {
            self.chain.as_ref().unwrap()
        }

        /// A block on the tip at the testnet's difficulty 1, as `submitblock` takes it.
        pub fn mined_blob(&self, miner: &MinerAddress) -> Vec<u8> {
            let t = self.chain().block_template(miner, &[]).unwrap();
            let mut block = t.block.clone();
            set_merge_mining_tag(&mut block).unwrap();
            xkr_serialization::encode_block(&block).unwrap()
        }

        pub fn add(&self, blob: Vec<u8>) {
            let r = self
                .chain()
                .add_block(RawBlock {
                    block: blob,
                    transactions: Vec::new(),
                })
                .unwrap();
            assert!(
                matches!(r.outcome, AddBlockOutcome::AddedToMain),
                "{:?}",
                r.outcome
            );
        }

        pub fn mine(&self, miner: &MinerAddress) {
            let blob = self.mined_blob(miner);
            self.add(blob);
        }
    }

    impl Drop for TestChain {
        fn drop(&mut self) {
            drop(self.chain.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn miner() -> MinerAddress {
        xkr_rpc::parse_miner_address(crate::fee::DEFAULT_FEE_ADDRESS).unwrap()
    }

    /// Another miner: a chain mined to it differs from one mined to [`miner`].
    fn other_miner() -> MinerAddress {
        let g = xkr_types::PublicKey(
            xkr_crypto::EdwardsPoint::mul_base(&xkr_crypto::Scalar::ONE)
                .compress()
                .to_bytes(),
        );
        let address = xkr_wallet_core::Address::new(g, g).encode();
        xkr_rpc::parse_miner_address(&address).unwrap()
    }

    /// The node's RPC state over `chain` (synchronized, no P2P layer).
    pub(crate) fn rpc_state(tc: &TestChain) -> Arc<RpcState> {
        let (_net_tx, net) = watch::channel(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        // The sender is dropped: the status stays as it is.
        let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
        let (commands, commands_rx) = mpsc::unbounded_channel();
        drop(commands_rx);
        Arc::new(RpcState {
            chain: tc.chain().clone(),
            net,
            commands,
            mining,
            hugin: None,
            p2pool: None,
            clients: None,
            explorer: false,
            info: NodeInfo {
                version: "remote-test".into(),
                start_time: 0,
                data_dir: tc.dir.clone(),
                db_path: tc.dir.join("chain.redb"),
                fee_address: String::new(),
                fee_amount: 0,
            },
        })
    }

    /// How a test node answers.
    #[derive(Clone, Default)]
    pub(crate) struct Twist {
        /// The hello names this stable state hash.
        state: Option<(u64, Hash)>,
        /// Headers are sent with this difficulty.
        difficulty: Option<u64>,
    }

    /// A node of the test: the phase-B server ([`ClientsServer`]) over a test chain, or, with
    /// a [`Twist`], a server with the same calls whose answers are changed.
    #[derive(Clone)]
    pub(crate) enum TestNode {
        Real(Arc<ClientsServer>),
        Twisted {
            rpc: Arc<RpcState>,
            twist: Twist,
            calls: Arc<Mutex<Vec<String>>>,
        },
    }

    impl TestNode {
        pub fn real(tc: &TestChain) -> Self {
            let stats = Arc::new(ClientStats::default());
            let server = ClientsServer::new(rpc_state(tc), stats, Network::Testnet, 16, None);
            Self::Real(server)
        }

        /// [`TestNode::real`] with p2pool's stratum server (`stratum` service).
        pub fn with_stratum(
            tc: &TestChain,
            stratum: Arc<dyn crate::clients::StratumSource>,
        ) -> Self {
            let stats = Arc::new(ClientStats::default());
            let server =
                ClientsServer::new(rpc_state(tc), stats, Network::Testnet, 16, Some(stratum));
            Self::Real(server)
        }

        fn twisted(tc: &TestChain, twist: Twist) -> Self {
            Self::Twisted {
                rpc: rpc_state(tc),
                twist,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Calls of `method` this node answered (a twisted node only).
        fn calls_of(&self, method: &str) -> usize {
            match self {
                Self::Real(..) => 0,
                Self::Twisted { calls, .. } => calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| *m == method)
                    .count(),
            }
        }

        /// The client end of a new connection, served in a task.
        fn open(&self, key: Key) -> tokio::io::DuplexStream {
            let (client_end, server_end) = tokio::io::duplex(4 * MAX_FRAME);
            match self.clone() {
                Self::Real(server) => {
                    let slot = server.admit(&key, None, Instant::now()).expect("admitted");
                    tokio::spawn(server.serve(slot, key, server_end));
                }
                Self::Twisted { rpc, twist, calls } => {
                    tokio::spawn(xkr_hyper_rpc::serve(
                        server_end,
                        ServerLimits::default(),
                        move |_hello| {
                            let tip = rpc.chain.tip();
                            let reply = HelloReply {
                                v: VERSION,
                                height: tip.height,
                                hash: tip.hash.to_string(),
                                synced: true,
                                services: vec!["node".into()],
                                state_span: twist.state.map(|s| s.0),
                                state_hash: twist.state.map(|s| s.1.to_string()),
                                pruning_seed: 0,
                            };
                            let pushes: BoxStream<'static, Push> = crate::clients::tip_pushes(&rpc);
                            Ok(Accepted {
                                reply,
                                handler: Arc::new(TwistedHandler { rpc, twist, calls }),
                                pushes: Some(pushes),
                            })
                        },
                    ));
                }
            }
            client_end
        }
    }

    struct TwistedHandler {
        rpc: Arc<RpcState>,
        twist: Twist,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Handler for TwistedHandler {
        fn call(
            &self,
            method: String,
            params: Value,
        ) -> BoxFuture<'static, Result<Value, RpcError>> {
            self.calls.lock().unwrap().push(method.clone());
            let (rpc, twist) = (self.rpc.clone(), self.twist.clone());
            async move {
                let mut v = tokio::task::spawn_blocking(move || {
                    crate::clients::node_call(&rpc, &method, &params)
                })
                .await
                .unwrap()?;
                if let Some(d) = twist.difficulty {
                    if let Some(h) = v.get_mut("block_header") {
                        h["difficulty"] = json!(d);
                    }
                    if let Some(list) = v.get_mut("headers").and_then(Value::as_array_mut) {
                        for h in list {
                            h["difficulty"] = json!(d);
                        }
                    }
                }
                Ok(v)
            }
            .boxed()
        }
    }

    /// Nodes in memory, by key.
    pub(crate) struct MemDialer(pub Vec<(Key, TestNode)>);

    impl Dialer for MemDialer {
        fn lookup(&self) -> BoxFuture<'static, Result<Vec<Key>, String>> {
            let keys = self.0.iter().map(|(k, _)| *k).collect();
            futures::future::ready(Ok(keys)).boxed()
        }

        fn dial(&self, key: Key) -> BoxFuture<'static, Result<BoxedStream, String>> {
            let r = self
                .0
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, n)| Box::new(n.open(key)) as BoxedStream)
                .ok_or_else(|| "unknown node".to_owned());
            futures::future::ready(r).boxed()
        }
    }

    fn key(i: u8) -> Key {
        [i; 32]
    }

    /// Remote nodes over `nodes`, running; with the given built-in state hashes.
    pub(crate) fn start(
        nodes: Vec<(Key, TestNode)>,
        builtin: StateHashes,
    ) -> (RemoteNodes, CancellationToken) {
        let mut cfg = RemoteConfig::new(Network::Testnet, "test");
        cfg.state_hashes = builtin;
        let remote = RemoteNodes::new(cfg);
        let stop = CancellationToken::new();
        let r = remote.clone();
        let s = stop.clone();
        tokio::spawn(async move { r.run(&MemDialer(nodes), &s).await });
        (remote, stop)
    }

    /// Waits until the status satisfies `f`.
    pub(crate) async fn wait_for(
        remote: &RemoteNodes,
        f: impl Fn(&RemoteStatus) -> bool,
    ) -> RemoteStatus {
        let mut rx = remote.subscribe_status();
        tokio::time::timeout(WAIT, async {
            loop {
                let s = rx.borrow_and_update().clone();
                if f(&s) {
                    return (*s).clone();
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("status not reached: {:?}", remote.status()))
    }

    fn mined(n: usize, to: &MinerAddress, name: &str) -> TestChain {
        let tc = TestChain::new(name);
        for _ in 0..n {
            tc.mine(to);
        }
        tc
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_node_gives_its_tip() {
        let tc = mined(3, &miner(), "one");
        let tip = tc.chain().tip();
        let (remote, stop) = start(vec![(key(1), TestNode::real(&tc))], StateHashes::default());
        let s = wait_for(&remote, |s| s.height.is_some()).await;
        assert_eq!(s.height, Some(tip.height));
        assert_eq!(s.hash, Some(tip.hash.to_string()));
        assert_eq!(s.nodes.len(), 1);
        assert!(s.synced);
        assert_eq!(
            *remote.subscribe_tip().borrow(),
            TipInfo {
                height: tip.height,
                hash: tip.hash,
                timestamp: tip.timestamp,
                difficulty: tip.difficulty,
                ..TipInfo::default()
            }
        );
        let net = remote.net_status().borrow().clone();
        assert!(net.synced && !net.downloading);
        assert_eq!(net.top_index, tip.height);
        assert_eq!(step_detail(&s), format!("1 node, height {}", tip.height));
        // a new block on the node is pushed and becomes the tip
        tc.mine(&miner());
        let s = wait_for(&remote, |s| s.height == Some(tip.height + 1)).await;
        assert_eq!(s.hash, Some(tc.chain().tip().hash.to_string()));
        stop.cancel();
    }

    /// Two nodes on the same chain, one a block behind: the tip both have or build on is the
    /// lower one, until the second has the block too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_nodes_agree_on_the_tip_they_share() {
        let ahead = mined(3, &miner(), "two-ahead");
        let behind = TestChain::new("two-behind");
        for h in 1..=2 {
            let blob = ahead
                .chain()
                .with_compressor(|c| ahead.chain().reader().unwrap().block_blob(h, c))
                .unwrap()
                .unwrap()
                .unwrap();
            behind.add(blob);
        }
        let shared = behind.chain().tip();
        assert_eq!(shared.height, 2);
        let (remote, stop) = start(
            vec![
                (key(1), TestNode::real(&ahead)),
                (key(2), TestNode::real(&behind)),
            ],
            StateHashes::default(),
        );
        let s = wait_for(&remote, |s| s.nodes.len() == 2 && s.height.is_some()).await;
        assert_eq!(s.height, Some(2));
        assert_eq!(s.hash, Some(shared.hash.to_string()));
        assert!(s.nodes.iter().all(|n| n.on_tip));
        // the second node gets the third block: both have it now
        let third = ahead
            .chain()
            .with_compressor(|c| ahead.chain().reader().unwrap().block_blob(3, c))
            .unwrap()
            .unwrap()
            .unwrap();
        behind.add(third);
        let s = wait_for(&remote, |s| s.height == Some(3)).await;
        assert_eq!(s.hash, Some(ahead.chain().tip().hash.to_string()));
        stop.cancel();
    }

    /// Three nodes, one on a longer chain of its own: the tip is the one the other two share.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_nodes_and_one_disagrees() {
        let main = mined(3, &miner(), "three-main");
        let other = mined(6, &other_miner(), "three-other");
        assert_ne!(
            main.chain().main_chain_headers(1, 2).unwrap(),
            other.chain().main_chain_headers(1, 2).unwrap()
        );
        let (remote, stop) = start(
            vec![
                (key(1), TestNode::real(&main)),
                (key(2), TestNode::real(&other)),
                (key(3), TestNode::real(&main)),
            ],
            StateHashes::default(),
        );
        let s = wait_for(&remote, |s| s.nodes.len() == 3 && s.height.is_some()).await;
        assert_eq!(s.height, Some(3));
        assert_eq!(s.hash, Some(main.chain().tip().hash.to_string()));
        let on_tip: Vec<bool> = s.nodes.iter().map(|n| n.on_tip).collect();
        assert_eq!(on_tip.iter().filter(|b| **b).count(), 2, "{s:?}");
        let off = s.nodes.iter().find(|n| !n.on_tip).unwrap();
        assert_eq!(off.key, short(&key(2)));
        assert_eq!(off.height, 6);
        stop.cancel();
    }

    /// A node whose headers carry a difficulty its blocks do not meet is dropped, and the tip
    /// comes from the others.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tip_that_fails_the_header_check_is_not_taken() {
        let tc = mined(3, &miner(), "check");
        let other = mined(5, &other_miner(), "check-other");
        // the difficulty check itself
        let hdr = |h: u64| {
            let v = crate::clients::node_call(
                &rpc_state(&tc),
                "getblockheaderbyheight",
                &json!({ "height": h }),
            )
            .unwrap();
            Header::from_json(&v["block_header"]).unwrap()
        };
        let blob = |h: u64| {
            let v = crate::clients::node_call(
                &rpc_state(&tc),
                crate::clients::GET_BLOCK_BLOB,
                &json!({ "height": h }),
            )
            .unwrap();
            hex::decode(v["blob"].as_str().unwrap()).unwrap()
        };
        let genesis = xkr_consensus::genesis_hash(Network::Testnet.params()).unwrap();
        let (second, third) = (hdr(2), hdr(3));
        assert_eq!(check_block(&third, &blob(3), &genesis), Ok(()));
        let hard = Header {
            difficulty: u64::MAX,
            ..third
        };
        assert!(
            check_block(&hard, &blob(3), &genesis)
                .unwrap_err()
                .contains("proof of work")
        );
        assert!(
            check_block(&third, &blob(2), &genesis).is_err(),
            "another block"
        );
        assert_eq!(check_linkage(&[second, third], (3, third.hash)), Ok(()));
        let unlinked = Header {
            prev: Hash([9; 32]),
            ..third
        };
        assert!(check_linkage(&[second, unlinked], (3, third.hash)).is_err());
        assert!(
            check_linkage(&[second, third], (3, second.hash)).is_err(),
            "not the tip"
        );

        // over the topic: a lone node with such headers gives no tip and is dropped
        let twisted = TestNode::twisted(
            &other,
            Twist {
                difficulty: Some(u64::MAX),
                ..Twist::default()
            },
        );
        let (remote, stop) = start(vec![(key(7), twisted)], StateHashes::default());
        tokio::time::timeout(WAIT, async {
            loop {
                if remote.state().banned.contains_key(&key(7)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the node is dropped");
        let s = remote.status();
        assert_eq!(s.height, None);
        assert!(!s.synced);
        assert!(s.nodes.is_empty());
        stop.cancel();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_with_another_state_hash_is_dropped() {
        let tc = mined(2, &miner(), "state");
        let (x, y) = (Hash([1; 32]), Hash([2; 32]));
        // against the built-in list
        let builtin = StateHashes::from_list([(0, x)]);
        let (remote, stop) = start(
            vec![
                (
                    key(1),
                    TestNode::twisted(
                        &tc,
                        Twist {
                            state: Some((0, y)),
                            ..Twist::default()
                        },
                    ),
                ),
                (
                    key(2),
                    TestNode::twisted(
                        &tc,
                        Twist {
                            state: Some((0, x)),
                            ..Twist::default()
                        },
                    ),
                ),
            ],
            builtin,
        );
        tokio::time::timeout(WAIT, async {
            loop {
                if remote.state().banned.contains_key(&key(1)) && remote.status().height.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{:?}", remote.status()));
        let s = remote.status();
        assert_eq!(s.nodes.len(), 1);
        assert_eq!(s.nodes[0].key, short(&key(2)));
        stop.cancel();

        // above the list: the majority of the nodes that name the span
        let (remote, stop) = start(
            vec![
                (
                    key(1),
                    TestNode::twisted(
                        &tc,
                        Twist {
                            state: Some((5, x)),
                            ..Twist::default()
                        },
                    ),
                ),
                (
                    key(2),
                    TestNode::twisted(
                        &tc,
                        Twist {
                            state: Some((5, y)),
                            ..Twist::default()
                        },
                    ),
                ),
                (
                    key(3),
                    TestNode::twisted(
                        &tc,
                        Twist {
                            state: Some((5, x)),
                            ..Twist::default()
                        },
                    ),
                ),
            ],
            StateHashes::default(),
        );
        tokio::time::timeout(WAIT, async {
            loop {
                let done = {
                    let st = remote.state();
                    st.banned.contains_key(&key(2)) && st.nodes.len() == 2
                };
                if done {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{:?}", remote.status()));
        stop.cancel();

        // the rule itself
        let none = StateHashes::default();
        assert!(
            state_outliers(&[(key(1), Some((5, x))), (key(2), Some((5, y)))], &none).is_empty(),
            "no majority of two"
        );
        assert_eq!(
            state_outliers(
                &[
                    (key(1), Some((5, x))),
                    (key(2), Some((5, y))),
                    (key(3), Some((5, x))),
                    (key(4), Some((6, y))),
                    (key(5), None)
                ],
                &none
            ),
            [key(2)]
        );
        assert_eq!(
            state_outliers(
                &[(key(1), Some((0, y))), (key(2), Some((0, x)))],
                &StateHashes::from_list([(0, x)])
            ),
            [key(1)]
        );
    }

    /// A found block goes to every connected node.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_found_block_goes_to_every_node() {
        let tc = mined(3, &miner(), "submit");
        let nodes: Vec<(Key, TestNode)> = (1..=3)
            .map(|i| (key(i), TestNode::twisted(&tc, Twist::default())))
            .collect();
        let (remote, stop) = start(nodes.clone(), StateHashes::default());
        wait_for(&remote, |s| s.nodes.len() == 3 && s.height.is_some()).await;
        let blob = tc.mined_blob(&miner());
        let (height, id) = remote.submit_block(blob.clone()).await.expect("taken");
        assert_eq!(height, 4);
        assert_eq!(tc.chain().tip().height, 4);
        assert_eq!(tc.chain().tip().hash, id);
        for (i, (_, n)) in nodes.iter().enumerate() {
            assert_eq!(n.calls_of("submitblock"), 1, "node {i}");
        }
        // a block no node takes is refused
        let mut bad = xkr_serialization::decode_block(&blob).unwrap();
        bad.header.prev_hash = Hash([5; 32]);
        let r = remote
            .submit_block(xkr_serialization::encode_block(&bad).unwrap())
            .await;
        assert!(r.is_err());
        stop.cancel();
    }

    /// The Hugin relay's job sources without the node: a TCP p2pool that does not answer
    /// first, then the stratum service of the connected remote node, over the connection that
    /// carries the chain: login, job, an accepted share in that node's p2pool.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stratum_over_the_clients_topic_is_the_last_job_source() {
        use xkr_hugin::pool::{Failover, LoginExtras, ShareSubmit, StratumConfig, StratumPool};

        let tc = mined(3, &miner(), "stratum");
        let (srv, sink, source) = crate::clients::test_stratum::server().await;
        let (remote, stop) = start(
            vec![
                (key(1), TestNode::real(&tc)),
                (key(2), TestNode::with_stratum(&tc, source)),
            ],
            StateHashes::default(),
        );
        wait_for(&remote, |s| s.nodes.len() == 2 && s.synced).await;
        let clients = remote.stratum_clients();
        assert_eq!(clients.len(), 2);
        assert!(clients[0].hello().serves(Service::Stratum), "listed first");
        assert!(!clients[1].hello().serves(Service::Stratum));

        // a TCP port nothing listens on
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let shutdown = CancellationToken::new();
        let pool = StratumPool::spawn_with_connector(
            StratumConfig {
                pools: vec![closed, REMOTE_STRATUM.to_owned()],
                login: "SEKRrelay".into(),
                pass: "x".into(),
                agent: "remote-test".into(),
            },
            LoginExtras::default(),
            Failover {
                switch_after: 1,
                return_to_first: None,
                max_backoff: Duration::from_secs(2),
            },
            Some(Arc::new(RemoteStratum::new(remote.clone()))),
            shutdown.clone(),
        );
        let mut jobs = pool.jobs();
        let job = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(j) = jobs.borrow_and_update().clone() {
                    return j;
                }
                jobs.changed().await.unwrap();
            }
        })
        .await
        .expect("a job over the clients topic");
        let status = pool.status();
        assert!(status.connected);
        assert_eq!(status.pool.as_deref(), Some(REMOTE_STRATUM));
        assert_eq!(srv.connections(), 1);

        let target = xkr_stratum::target::parse_target(&job.target).unwrap();
        let blob = job.blob.clone();
        let (nonce, hash) =
            tokio::task::spawn_blocking(move || crate::clients::test_stratum::mine(&blob, target))
                .await
                .unwrap();
        let outcome = pool
            .submit(ShareSubmit {
                job_id: job.job_id.clone(),
                nonce: hex::encode(nonce.to_le_bytes()),
                result: hex::encode(hash),
            })
            .await;
        assert_eq!(outcome, xkr_hugin::pool::SubmitOutcome::Accepted);
        assert_eq!(sink.shares.lock().unwrap().len(), 1);

        // the pool client stops: the session is closed on the node
        shutdown.cancel();
        tokio::time::timeout(WAIT, async {
            while srv.connections() > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the session ends");
        // the chain connection is still there
        assert_eq!(remote.status().nodes.len(), 2);
        stop.cancel();
    }

    /// Without a node that runs p2pool no stratum session opens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_stratum_without_p2pool_on_the_nodes() {
        let tc = mined(2, &miner(), "no-stratum");
        let empty = RemoteStratum::new(RemoteNodes::new(RemoteConfig::new(
            Network::Testnet,
            "test",
        )));
        assert!(empty.serves(REMOTE_STRATUM));
        assert!(!empty.serves("127.0.0.1:3333"));
        assert!(empty.open(REMOTE_STRATUM).await.is_err());
        let (remote, stop) = start(vec![(key(1), TestNode::real(&tc))], StateHashes::default());
        wait_for(&remote, |s| s.synced).await;
        let e = RemoteStratum::new(remote.clone())
            .open(REMOTE_STRATUM)
            .await
            .err()
            .expect("refused");
        assert!(e.contains("stratum"), "{e}");
        stop.cancel();
    }

    /// Synchronized only while a node confirmed the tip less than three minutes ago.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_old_tip_is_not_synced() {
        let tc = mined(2, &miner(), "stale");
        let (remote, stop) = start(vec![(key(1), TestNode::real(&tc))], StateHashes::default());
        wait_for(&remote, |s| s.synced).await;
        let now = Instant::now();
        assert!(remote.synced_at(now));
        assert!(
            remote.synced_at(
                (now + STALE_AFTER)
                    .checked_sub(Duration::from_secs(5))
                    .unwrap()
            )
        );
        assert!(!remote.synced_at(now + STALE_AFTER + Duration::from_secs(1)));
        assert!(
            remote
                .status_at(now + STALE_AFTER + Duration::from_secs(1))
                .tip_age
                .unwrap()
                >= 180
        );
        stop.cancel();
        // and without any node there is nothing to be synchronized with
        let empty = RemoteNodes::new(RemoteConfig::new(Network::Testnet, "test"));
        assert!(!empty.synced_at(Instant::now()));
    }

    #[test]
    fn miner_data_from_the_node() {
        let mut seen = HashMap::new();
        let v = json!({
            "status": "OK", "major_version": 7, "height": 10, "prev_id": "ab".repeat(32),
            "seed_hash": "00".repeat(32), "difficulty": "ff", "median_weight": 100000,
            "already_generated_coins": 5, "median_timestamp": 99, "unlock_window": 40,
            "tx_backlog": [{"id": "cd".repeat(32), "weight": 300, "fee": 10}],
        });
        let d = miner_data_from_json(&v, &mut seen, 1_000).unwrap();
        assert_eq!((d.major_version, d.height, d.difficulty), (7, 10, 255));
        assert_eq!(d.tx_backlog[0].received, 1_000);
        let again = miner_data_from_json(&v, &mut seen, 2_000).unwrap();
        assert_eq!(again.tx_backlog[0].received, 1_000, "first seen");
        assert!(miner_data_from_json(&json!({"height": 1}), &mut seen, 0).is_err());
    }

    #[test]
    fn step_details_fit() {
        let s = RemoteStatus {
            nodes: vec![RemoteNodeStatus::default(); 3],
            height: Some(2_641_203),
            ..RemoteStatus::default()
        };
        assert_eq!(step_detail(&s), "3 nodes, height 2 641 203");
        let none = RemoteStatus::default();
        assert_eq!(step_detail(&none), "0 nodes, no common tip yet");
        assert!(step_detail(&s).chars().count() <= 40);
    }
}
