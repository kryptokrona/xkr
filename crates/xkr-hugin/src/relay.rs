//! The relay: hugin-node's `HuginNode` + `Network` (`nodes.js`, `network.js`) over any byte
//! stream.
//!
//! Every connection is a [`Role::Client`] (a Hugin app that found us on the public-node or
//! private-node topic) or a [`Role::Node`] (another hugin-node or xkrd on the node-mesh topic).
//! Each runs bare-rpc (see [`crate::protocol`]): every request is answered with the fixed
//! `{"ok":true,...}` reply, and the JSON inside drives the logic below.
//!
//! Clients: `{ request: true, type: 'some', timestamp }` (pool sync), `{ type: 'job_request' }`,
//! `{ type: 'post', message }` (store-and-forward DM, call or room message, paid with PoW
//! shares), `{ type: 'register' | register: true }` (push registration, paid the same way),
//! `{ type: 'pow_tag' }` (no-op). Nodes: `{ version }`, `{ signal: true, message }` (gossip of
//! an accepted message), `{ push: true, message }` (for the push cluster, ignored), plus our
//! `xkrd_sync_request`/`xkrd_sync` extension between xkrd nodes. `docs/hugin-relay.md` has the
//! full protocol and every place where we differ from hugin-node.
//!
//! Apps mine at the relay's app difficulty ([`RelayConfig::app_difficulty`]): pool jobs reach
//! them with the app target, every post or registration needs one share that meets it, and
//! only the shares that also meet the pool job's own target are submitted to the pool. Client
//! connections are limited in total ([`RelayConfig::max_clients`]) and per remote IP address
//! and IPv6 prefix ([`RelayConfig::max_clients_per_ip`]), node connections in total and per
//! address ([`MAX_NODES_PER_ADDRESS`]); each connection keeps its own cooldown, rate windows,
//! gossip allowance and strikes, and has one post or registration checked at a time.
//!
//! What goes to the apps shares one send budget ([`RelayConfig::queue_budget`]) in two classes.
//! Control packets and notifications (hello, answers to posts, registrations and job
//! requests, PoW jobs, bare-rpc replies and the `new-message` notification that a stored
//! message arrived) go first, through each connection's queue ([`OUTBOX_BYTES`]). History
//! syncs (`some` answers, the stored messages an app fetches) come second: at most
//! [`RelayConfig::sync_slots`] at once, further requests wait in arrival order, an answer is
//! prepared only when it fits into the budget, and the answers being sent are written in turns of
//! [`SYNC_CHUNK`] bytes ([`RelayConfig::sync_writers`] at once), so every syncing app makes
//! progress. The apps of one address share a sync allowance ([`SYNC_ALLOWANCE_BYTES`] per
//! [`SYNC_ALLOWANCE_WINDOW`]) for the size of their answers. A connection writes its control
//! packets and notifications before it starts a sync answer; once started, an answer is one bare-rpc packet and is finished before the next packet
//! of that connection. While many syncs wait or the budget is mostly used, new app connections
//! are closed right away (admission, [`admission_paused`]); connected apps and node connections
//! are not affected.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use rand_core::RngCore;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedMutexGuard, Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use crate::pool::{
    ChainTip, JobSource, MAX_JOB_AGE, PoolJob, ShareSubmit, SubmitOutcome, VALID_JOBS,
};
use crate::pow::{
    self, MAX_MESSAGE_PAST_MS, PowJob, PrevIds, check_message, truthy, validate_pow_payload,
    validate_share_fast, verify_pow_auth,
};
use crate::protocol::{self, BareRpcCodec, RpcCodecError, RpcMessage};
use crate::store::{Insert, MessagePool, StoreLimits};

/// hugin-node `MAX_CLIENT_INBOUND_BYTES` / `MAX_NODE_INBOUND_BYTES`: larger PACKET data is a ban.
pub const HUGIN_INBOUND_LIMIT: usize = 9000;
/// Inbound PACKET limit for xkrd peers (sync batches).
pub const XKRD_INBOUND_LIMIT: usize = 1024 * 1024;
/// hugin-node `MAX_SYNC_MESSAGES`.
pub const MAX_SYNC_MESSAGES: usize = 500;
/// hugin-node `CLIENT_POST_COOLDOWN_MS`.
pub const POST_COOLDOWN: Duration = Duration::from_secs(1);
/// hugin-node `CLIENT_JOB_REQUEST_MAX_PER_10S` / `GLOBAL_JOB_REQUEST_MAX_PER_10S`.
pub const CLIENT_JOB_REQUESTS_PER_WINDOW: u32 = 10;
pub const GLOBAL_JOB_REQUESTS_PER_WINDOW: u32 = 500;
/// Pool syncs (`{ request: true, type: 'some' }`) per [`RATE_WINDOW`] and connection. Each one
/// clones and serializes up to [`MAX_SYNC_MESSAGES`] messages (up to ~4.5 MB) for a ~60-byte
/// request; apps sync on connect and then rely on pushes. Over the limit: `rate_limit`, no ban.
/// (Not in hugin-node, which answers every request.)
pub const CLIENT_SYNC_REQUESTS_PER_WINDOW: u32 = 5;
/// The app connections of one address (an IPv4 address, an IPv6 /64; a connection without an
/// address on its own) share a sync allowance: this many bytes of messages in `some` answers
/// per [`SYNC_ALLOWANCE_WINDOW`], refilled continuously and full at the start. An answer
/// larger than what is left keeps its newest messages that fit; once the allowance is used up,
/// answers hold only the messages after the newest one sent until it is full again.
/// (Not in hugin-node.)
pub const SYNC_ALLOWANCE_BYTES: u64 = 16 * 1024 * 1024;
/// See [`SYNC_ALLOWANCE_BYTES`].
pub const SYNC_ALLOWANCE_WINDOW: Duration = Duration::from_mins(10);
/// A `some` request within this of the connection's previous answer, with a `timestamp` older
/// than the newest message the connection was already sent, is answered with the messages
/// after that message.
pub const SYNC_REPEAT_WINDOW: Duration = Duration::from_secs(30);
/// hugin-node `REQUEST_RATE_WINDOW_MS`.
pub const RATE_WINDOW: Duration = Duration::from_secs(10);
/// hugin-node `CLIENT_REQUEST_SPAM_STRIKES`.
pub const RATE_STRIKES: u32 = 3;
/// Invalid PoW twice from one connection is a ban (hugin-node `strikes >= 2`).
pub const POW_STRIKES: u32 = 2;
/// hugin-node `timeout()`: a 60 s ban.
pub const TIMEOUT_BAN: Duration = Duration::from_mins(1);
/// hugin-node `ban()` lasts as long as its swarm; we use a day.
pub const BAN: Duration = Duration::from_hours(24);
/// hugin-node `acceptedPowAuthCacheTtlMs`.
pub const AUTH_CACHE_TTL: Duration = Duration::from_hours(2);
/// How long a share stays bound to the message it paid for (longer than any message lives).
pub const SHARE_BINDING_TTL: Duration = Duration::from_hours(25);
const SHARE_BINDING_CAP: usize = 200_000;
const AUTH_CACHE_CAP: usize = 200_000;
/// Wait for a pool re-login before retrying a submit (hugin-node `wait_for_pool_login`).
const RELOGIN_WAIT: Duration = Duration::from_secs(2);
/// Outgoing queue per connection; a peer that lets it fill up is disconnected.
const OUTBOX: usize = 1024;
/// Bytes of control packets and notifications queued per app connection (sync answers are sent
/// apart from this queue). Over the limit the packet is refused exactly like
/// on a full queue (the connection is stopped). A single packet larger than the limit is still
/// queued when nothing else is. The `new-message` notifications are one shared copy for all
/// apps, so their memory does not grow with the number of apps.
pub const OUTBOX_BYTES: usize = 2 * 1024 * 1024;
/// [`OUTBOX_BYTES`] for a relay connection: gossip packets are small, and an xkrd sync we serve
/// waits for room instead of queueing all its batches at once.
pub const NODE_OUTBOX_BYTES: usize = 3 * 1024 * 1024;
/// How long a sync we serve waits for room in the queue of a connection that does not read; an
/// app's sync answer that makes no progress for this long closes the connection.
const SYNC_SEND_WAIT: Duration = Duration::from_mins(1);
/// Default of [`RelayConfig::hello_timeout`]: a connection must send its first packet within
/// this. Hugin apps sync (`some`) or ask for a job right after connecting and
/// hugin-node sends `{ version }` at once (`network.js`), so a minute is generous.
pub const HELLO_TIMEOUT: Duration = Duration::from_mins(1);
/// Default of [`RelayConfig::idle_evict_after`]. Hugin apps stay silent for hours (they stop
/// polling after their start-up syncs and wait for pushes) and the protocol has no keepalive
/// packet, so there is no hard idle timeout. Instead, when a role is at its connection limit,
/// one connection of that role whose last served request is at least this old is closed to
/// make room for the newcomer: first those that never had a post, registration or gossiped
/// message accepted, the longest without a served request first; connections with such an
/// accepted message only after [`PROVEN_IDLE_FACTOR`] times as long. For an app connection
/// that never had a post or registration accepted, only the connection itself and accepted
/// posts and registrations count, so its slot can be given to another app this long after it
/// connected. Otherwise served requests are answered `some` syncs and job requests, accepted
/// posts and registrations, and from nodes their `version`, new valid gossip and sync traffic;
/// other packets (`pow_tag`, unknown types, refused or failed requests, replies) do not count.
pub const IDLE_EVICT_AFTER: Duration = Duration::from_mins(10);
/// See [`IDLE_EVICT_AFTER`].
pub const PROVEN_IDLE_FACTOR: u32 = 6;
/// hugin-node `MIN_SHARE_DIFFICULTY`: the least share difficulty hugin-node takes in gossip
/// (compact target `285c8f02`).
pub const MIN_SHARE_DIFFICULTY: u64 = 100;
/// Default of [`RelayConfig::app_difficulty`]: the share difficulty of the jobs handed to apps
/// (compact target `14ae4701`), about one second of mining per message on a multi-core phone.
pub const DEFAULT_APP_DIFFICULTY: u64 = 200;
/// Default of [`RelayConfig::min_share_difficulty`], the least difficulty of the job target a
/// gossiped or synced message carries ([`Relay::gossip_floor`]): the higher of
/// [`MIN_SHARE_DIFFICULTY`] and [`DEFAULT_APP_DIFFICULTY`], the same on every relay, so relays
/// with different app difficulties (this or higher) take each other's messages.
pub const GOSSIP_MIN_DIFFICULTY: u64 = if MIN_SHARE_DIFFICULTY > DEFAULT_APP_DIFFICULTY {
    MIN_SHARE_DIFFICULTY
} else {
    DEFAULT_APP_DIFFICULTY
};
/// Default of [`RelayConfig::max_clients_per_ip`].
pub const DEFAULT_MAX_CLIENTS_PER_IP: usize = 4;
/// Most app connections from one IPv6 /56 (and from one /48 below), when
/// [`RelayConfig::max_clients_per_ip`] is set; a higher per-address limit raises these to it.
pub const MAX_CLIENTS_PER_56: usize = 8;
/// See [`MAX_CLIENTS_PER_56`].
pub const MAX_CLIENTS_PER_48: usize = 16;
/// Most node-mesh connections from one IPv4 address or IPv6 /64.
pub const MAX_NODES_PER_ADDRESS: usize = 4;
/// Gossiped messages (`signal` packets) one node connection may send at once, and how many it
/// gets back per second (a token bucket); further ones are dropped unread, without a ban.
pub const GOSSIP_BURST: u32 = 200;
/// See [`GOSSIP_BURST`].
pub const GOSSIP_PER_SECOND: u32 = 20;
/// hugin-node `GOSSIP_PREV_ID_WINDOW`: a gossiped job's `prev_id` must be one of the last this
/// many distinct `prev_id`s of our own pool jobs (a zero `prev_id` always passes, see
/// [`Relay::check_gossip`]).
pub const GOSSIP_PREV_ID_WINDOW: usize = 6;
/// hugin-node `GOSSIP_PENDING_TTL_MS` / `GOSSIP_PENDING_MAX`: gossip with a `prev_id` we have
/// not seen yet waits this long, at most this many messages, for our next pool job.
pub const GOSSIP_PENDING_TTL: Duration = Duration::from_mins(1);
pub const GOSSIP_PENDING_MAX: usize = 256;
/// Most bytes of messages in one `some` answer (besides the [`MAX_SYNC_MESSAGES`] count).
pub const SOME_MAX_BYTES: usize = 4 * 1024 * 1024;
/// An xkrd sync is served at most once per remote key within this; keys are free,
/// so [`MAX_SYNC_SERVES_PER_COOLDOWN`] also bounds the total.
pub const SYNC_COOLDOWN: Duration = Duration::from_mins(10);
/// Most xkrd syncs served (distinct remote keys) per [`SYNC_COOLDOWN`].
pub const MAX_SYNC_SERVES_PER_COOLDOWN: usize = 16;
/// Most xkrd syncs we receive (and verify, one hash per new message) at once. A node we do not
/// ask still gossips and can ask us; it just does not send us its pool now.
pub const MAX_CONCURRENT_SYNCS_IN: usize = 2;
/// A sync slot is given to another node after this, so a node that never answers cannot
/// hold the slots.
pub const SYNC_SLOT_TIMEOUT: Duration = Duration::from_mins(2);
const SYNC_KEYS_CAP: usize = 4096;
/// Packets handled concurrently per connection.
const MAX_INFLIGHT: usize = 8;
/// Sync answers: messages per batch, bytes per batch, messages per sync.
const SYNC_BATCH_MESSAGES: usize = 500;
const SYNC_BATCH_BYTES: usize = 256 * 1024;
const SYNC_MAX_MESSAGES: usize = 20_000;
/// Default of [`RelayConfig::sync_slots`].
pub const DEFAULT_SYNC_SLOTS: usize = 8;
/// Default of [`RelayConfig::queue_budget`].
pub const DEFAULT_QUEUE_BUDGET: usize = 256 * 1024 * 1024;
/// Default of [`RelayConfig::sync_writers`].
pub const DEFAULT_SYNC_WRITERS: usize = 2;
/// Bytes of a sync answer written per turn.
pub const SYNC_CHUNK: usize = 64 * 1024;
/// Longest turn of one sync answer (an app that reads slowly gives its turn to the next).
const SYNC_TURN: Duration = Duration::from_millis(250);
/// New app connections are paused when the queued bytes reach this share of the budget...
pub const ADMISSION_PAUSE_PERCENT: usize = 80;
/// ...and resumed below this share ([`admission_paused`]).
pub const ADMISSION_RESUME_PERCENT: usize = 60;
/// How often a sync answer that waits for the budget looks again.
const BUDGET_POLL: Duration = Duration::from_millis(20);
/// How long a closing connection may flush its queue.
const CLOSE_FLUSH: Duration = Duration::from_secs(2);
const CLEAN_INTERVAL: Duration = Duration::from_mins(1);

/// Milliseconds since the Unix epoch, as JavaScript's `Date.now()`.
#[must_use]
pub fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
        .floor()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn json_bytes(v: &Value) -> Bytes {
    Bytes::from(serde_json::to_vec(v).unwrap_or_default())
}

// ---------------------------------------------------------------------------------------------
// configuration and statistics

/// Relay settings.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// The node's XKR address (sent to clients as `{ address, version }`).
    pub node_address: String,
    /// Our software version, advertised to xkrd peers.
    pub xkrd_version: String,
    /// Most client connections (public and private topics together, [`DEFAULT_MAX_CLIENTS`]).
    /// Below it, new app connections can still be paused while the relay is busy with history
    /// syncs ([`admission_paused`]).
    pub max_clients: usize,
    /// Most node-mesh connections.
    pub max_nodes: usize,
    pub store_limits: StoreLimits,
    /// Persist the pool in this directory (`<data-dir>/hugin`); `None` keeps it in memory.
    pub data_dir: Option<PathBuf>,
    /// How often accepted messages are pushed to clients (hugin-node: 5 s).
    pub flush_interval: Duration,
    /// Ask xkrd peers for their pool when they connect.
    pub sync_on_connect: bool,
    /// Push gossiped messages to our clients and re-gossip them to other nodes (hugin-node
    /// does neither; see `docs/hugin-relay.md`).
    pub forward_gossip: bool,
    /// Most CN-Turtle share hashes computed at once, relay-wide. Each runs on tokio's blocking
    /// pool; further checks wait for a permit (FIFO), and each connection has at most 8
    /// (`MAX_INFLIGHT`) packets in flight. Values below 1 count as 1.
    pub max_concurrent_hashes: usize,
    /// First packet deadline per connection ([`HELLO_TIMEOUT`]); closed without a ban.
    pub hello_timeout: Duration,
    /// See [`IDLE_EVICT_AFTER`].
    pub idle_evict_after: Duration,
    /// Least difficulty of a gossiped or synced message's job ([`GOSSIP_MIN_DIFFICULTY`], at
    /// least 1).
    pub min_share_difficulty: u64,
    /// Share difficulty of the jobs handed to apps ([`DEFAULT_APP_DIFFICULTY`], at least 1):
    /// every job goes out with this target instead of the pool's, posts and registrations need
    /// a share that meets it, and only shares that also meet the pool job's own target are
    /// submitted to the pool.
    pub app_difficulty: u64,
    /// Most client connections from one remote IP address, IPv6 counted per /64
    /// ([`DEFAULT_MAX_CLIENTS_PER_IP`]), with wider IPv6 limits per /56 ([`MAX_CLIENTS_PER_56`])
    /// and per /48 ([`MAX_CLIENTS_PER_48`]); 0: none of these limits. Connections without a
    /// known address only count toward [`Self::max_clients`]. Node connections have their own
    /// limit per address ([`MAX_NODES_PER_ADDRESS`]).
    pub max_clients_per_ip: usize,
    /// Most history syncs of apps (`some` answers) prepared and sent at once
    /// ([`DEFAULT_SYNC_SLOTS`], at least 1); further `some` requests wait in arrival order and
    /// are dropped when their connection closes. A slot is held until the answer is written.
    pub sync_slots: usize,
    /// Send budget in bytes of all app connections together ([`DEFAULT_QUEUE_BUDGET`]): queued
    /// control packets and notifications plus the sync answers being sent. A sync answer is
    /// prepared only when it fits (or when nothing is queued at all).
    pub queue_budget: usize,
    /// Sync answers written at once, each for one turn of [`SYNC_CHUNK`] bytes before the next
    /// waiting answer gets its turn ([`DEFAULT_SYNC_WRITERS`], at least 1).
    pub sync_writers: usize,
}

/// Default of [`RelayConfig::max_clients`].
pub const DEFAULT_MAX_CLIENTS: usize = 200;

/// Default of [`RelayConfig::max_concurrent_hashes`]: half the cores, at least one.
#[must_use]
pub fn default_hash_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
}

impl RelayConfig {
    #[must_use]
    pub fn new(node_address: String) -> Self {
        Self {
            node_address,
            xkrd_version: env!("CARGO_PKG_VERSION").to_owned(),
            max_clients: DEFAULT_MAX_CLIENTS,
            max_nodes: 32,
            store_limits: StoreLimits::default(),
            data_dir: None,
            flush_interval: Duration::from_secs(5),
            sync_on_connect: true,
            forward_gossip: true,
            max_concurrent_hashes: default_hash_threads(),
            hello_timeout: HELLO_TIMEOUT,
            idle_evict_after: IDLE_EVICT_AFTER,
            min_share_difficulty: GOSSIP_MIN_DIFFICULTY,
            app_difficulty: DEFAULT_APP_DIFFICULTY,
            max_clients_per_ip: DEFAULT_MAX_CLIENTS_PER_IP,
            sync_slots: DEFAULT_SYNC_SLOTS,
            queue_budget: DEFAULT_QUEUE_BUDGET,
            sync_writers: DEFAULT_SYNC_WRITERS,
        }
    }
}

/// Live counters of the relay (cheap to read from any thread, e.g. for `/status`).
#[derive(Debug, Default)]
pub struct HuginStats {
    pub clients: AtomicU64,
    pub nodes: AtomicU64,
    pub xkrd_nodes: AtomicU64,
    pub messages: AtomicU64,
    pub pool_bytes: AtomicU64,
    pub posts_accepted: AtomicU64,
    pub posts_rejected: AtomicU64,
    pub registers_accepted: AtomicU64,
    pub gossip_received: AtomicU64,
    pub gossip_sent: AtomicU64,
    pub synced_in: AtomicU64,
    /// App shares that paid for an accepted post or registration (they met the app target).
    pub shares_accepted: AtomicU64,
    /// Shares submitted to the pool: the accepted ones that also met the pool job's target.
    pub shares_submitted: AtomicU64,
    /// Submitted shares the pool did not accept (rejected or duplicate).
    pub shares_rejected_by_pool: AtomicU64,
    pub bans: AtomicU64,
    /// [`RelayConfig::max_clients`].
    pub apps_cap: AtomicU64,
    /// History syncs of apps being prepared or sent ([`RelayConfig::sync_slots`]).
    pub syncs_running: AtomicU64,
    /// `some` requests waiting for a sync slot.
    pub syncs_queued: AtomicU64,
    /// Bytes queued for all app connections ([`RelayConfig::queue_budget`]).
    pub queued_bytes: AtomicU64,
    /// New app connections are closed right away ([`admission_paused`]).
    pub admission_paused: AtomicBool,
    /// App connections closed because of [`Self::admission_paused`].
    pub refused_busy: AtomicU64,
    /// `some` answers that the sync allowance made smaller ([`SYNC_ALLOWANCE_BYTES`]).
    pub syncs_limited: AtomicU64,
    /// App connections whose sync allowance is used up and not yet full again.
    pub apps_on_allowance: AtomicU64,
    pub pool_ready: AtomicBool,
    /// The fee address the shares are submitted for (set by xkrd).
    pub fee: Mutex<FeeInfo>,
    /// The p2pool node in use (set by xkrd).
    pub p2pool: Mutex<P2poolInfo>,
}

/// The node operator's fee address as shown in `/status` and `status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FeeInfo {
    /// `None`: no fee address, shares are not submitted.
    pub address: Option<String>,
    /// The address is the default donation address.
    pub donation: bool,
}

/// The p2pool node the relay uses (`/status` `hugin.p2pool`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct P2poolInfo {
    /// `host:port` in use or being tried.
    pub host: Option<String>,
    /// Logged in.
    pub connected: bool,
}

/// A copy of [`HuginStats`] for serialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HuginStatsSnapshot {
    pub clients: u64,
    pub nodes: u64,
    pub xkrd_nodes: u64,
    pub messages: u64,
    pub pool_bytes: u64,
    pub posts_accepted: u64,
    pub posts_rejected: u64,
    pub registers_accepted: u64,
    pub gossip_received: u64,
    pub gossip_sent: u64,
    pub synced_in: u64,
    pub shares_accepted: u64,
    pub shares_submitted: u64,
    pub shares_rejected_by_pool: u64,
    pub bans: u64,
    pub apps_cap: u64,
    pub syncs_running: u64,
    pub syncs_queued: u64,
    pub queued_bytes: u64,
    pub admission_paused: bool,
    pub refused_busy: u64,
    pub syncs_limited: u64,
    pub apps_on_allowance: u64,
    pub pool_ready: bool,
    pub fee_address: Option<String>,
    pub fee_donation: bool,
    pub p2pool: P2poolInfo,
}

impl HuginStats {
    #[must_use]
    pub fn snapshot(&self) -> HuginStatsSnapshot {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let fee = lock(&self.fee).clone();
        HuginStatsSnapshot {
            clients: g(&self.clients),
            nodes: g(&self.nodes),
            xkrd_nodes: g(&self.xkrd_nodes),
            messages: g(&self.messages),
            pool_bytes: g(&self.pool_bytes),
            posts_accepted: g(&self.posts_accepted),
            posts_rejected: g(&self.posts_rejected),
            registers_accepted: g(&self.registers_accepted),
            gossip_received: g(&self.gossip_received),
            gossip_sent: g(&self.gossip_sent),
            synced_in: g(&self.synced_in),
            shares_accepted: g(&self.shares_accepted),
            shares_submitted: g(&self.shares_submitted),
            shares_rejected_by_pool: g(&self.shares_rejected_by_pool),
            bans: g(&self.bans),
            apps_cap: g(&self.apps_cap),
            syncs_running: g(&self.syncs_running),
            syncs_queued: g(&self.syncs_queued),
            queued_bytes: g(&self.queued_bytes),
            admission_paused: self.admission_paused.load(Ordering::Relaxed),
            refused_busy: g(&self.refused_busy),
            syncs_limited: g(&self.syncs_limited),
            apps_on_allowance: g(&self.apps_on_allowance),
            pool_ready: self.pool_ready.load(Ordering::Relaxed),
            fee_address: fee.address,
            fee_donation: fee.donation,
            p2pool: lock(&self.p2pool).clone(),
        }
    }

    fn inc(a: &AtomicU64) {
        a.fetch_add(1, Ordering::Relaxed);
    }

    /// Sets the fee address shown in the snapshot.
    pub fn set_fee(&self, fee: FeeInfo) {
        *lock(&self.fee) = fee;
    }

    /// Sets the p2pool node shown in the snapshot.
    pub fn set_p2pool(&self, p2pool: P2poolInfo) {
        *lock(&self.p2pool) = p2pool;
    }
}

// ---------------------------------------------------------------------------------------------
// small state helpers

/// A fixed-window rate limiter (hugin-node `allow_client_window` / `allow_global_window`).
#[derive(Debug, Default, Clone, Copy)]
struct Window {
    start: Option<Instant>,
    count: u32,
}

impl Window {
    fn allow(&mut self, now: Instant, limit: u32) -> bool {
        if self
            .start
            .is_none_or(|s| now.saturating_duration_since(s) >= RATE_WINDOW)
        {
            self.start = Some(now);
            self.count = 0;
        }
        self.count += 1;
        self.count <= limit
    }
}

/// A token bucket of `burst` tokens that refills `per_second` tokens a second; it starts full.
#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    /// Tokens as of `at` (`None`: never used, full).
    tokens: f64,
    at: Option<Instant>,
}

impl Bucket {
    /// Takes one token; false when none is left.
    fn take(&mut self, now: Instant, burst: u32, per_second: u32) -> bool {
        let burst = f64::from(burst);
        let tokens = match self.at {
            None => burst,
            Some(at) => (self.tokens
                + now.saturating_duration_since(at).as_secs_f64() * f64::from(per_second))
            .min(burst),
        };
        self.at = Some(now);
        if tokens >= 1.0 {
            self.tokens = tokens - 1.0;
            true
        } else {
            self.tokens = tokens;
            false
        }
    }
}

/// A sync allowance ([`SYNC_ALLOWANCE_BYTES`] per [`SYNC_ALLOWANCE_WINDOW`], refilled
/// continuously, full at the start), shared by the app connections of one address
/// ([`allowance_key`]); a connection without an address has its own. A `some` answer larger
/// than what is left keeps its newest messages that fit (at least one), as the answer's own
/// caps do, so the app goes on from its newest message; the allowance is then used up. Until it
/// is full again, a connection that was already answered gets only the messages after the
/// newest one it was sent ([`SyncPoint`]), and the requests wait behind the others for a sync
/// slot.
#[derive(Debug, Default, Clone, Copy)]
struct SyncAllowance {
    /// Bytes left as of `at` (`None`: never used, full).
    left: u64,
    at: Option<Instant>,
    /// Used up and not yet full again.
    used_up: bool,
}

impl SyncAllowance {
    /// Bytes left at `now`.
    fn left_at(&self, now: Instant) -> u64 {
        let Some(at) = self.at else {
            return SYNC_ALLOWANCE_BYTES;
        };
        let refill = now
            .saturating_duration_since(at)
            .as_nanos()
            .saturating_mul(u128::from(SYNC_ALLOWANCE_BYTES))
            / SYNC_ALLOWANCE_WINDOW.as_nanos();
        u64::try_from(u128::from(self.left).saturating_add(refill))
            .unwrap_or(u64::MAX)
            .min(SYNC_ALLOWANCE_BYTES)
    }

    /// Refills up to `now` and returns the bytes left; a used-up allowance that is full again
    /// is no longer used up.
    fn refill(&mut self, now: Instant) -> u64 {
        let left = self.left_at(now);
        self.left = left;
        self.at = Some(now);
        if left >= SYNC_ALLOWANCE_BYTES {
            self.used_up = false;
        }
        left
    }

    /// The allowance is used up at `now`.
    fn used_up_at(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.used_up
    }

    /// Used up at `now`, without refilling.
    fn is_used_up(&self, now: Instant) -> bool {
        self.used_up && self.left_at(now) < SYNC_ALLOWANCE_BYTES
    }

    /// Charges the messages of an answer (oldest first) prepared at `now`. Unless `keep_all`,
    /// an answer over what is left loses its oldest messages until the rest fits (the newest
    /// always stays) and the allowance is used up; true when messages were dropped. With
    /// `keep_all` (a used-up allowance answering only the messages after the newest one the
    /// connection was sent) the whole answer stays and is charged as well.
    fn charge(&mut self, now: Instant, list: &mut Vec<(f64, Bytes)>, keep_all: bool) -> bool {
        let left = self.refill(now);
        let mut total: u64 = list.iter().map(|(_, m)| m.len() as u64).sum();
        let mut gone = 0;
        if !keep_all && total > left {
            while total > left && gone + 1 < list.len() {
                total -= list[gone].1.len() as u64;
                gone += 1;
            }
            list.drain(..gone);
            self.used_up = true;
        }
        self.left = left.saturating_sub(total);
        if self.left == 0 {
            self.used_up = true;
        }
        gone > 0
    }

    /// While used up: how long until the allowance is full again (as of its last refill).
    fn full_in(&self) -> Option<Duration> {
        if !self.used_up {
            return None;
        }
        let missing = u128::from(SYNC_ALLOWANCE_BYTES.saturating_sub(self.left));
        let nanos = missing * SYNC_ALLOWANCE_WINDOW.as_nanos() / u128::from(SYNC_ALLOWANCE_BYTES);
        Some(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }
}

/// The newest message one app connection was sent, and when it was last answered.
#[derive(Debug, Default, Clone, Copy)]
struct SyncPoint {
    /// Timestamp of the newest message sent to this connection.
    newest: Option<f64>,
    /// When the connection's previous `some` answer was prepared.
    answered: Option<Instant>,
}

impl SyncPoint {
    /// The `timestamp` a `some` request for the messages after `since` is answered from at
    /// `now`: the newest message already sent when that is newer than `since` and the sync
    /// allowance is `used_up` (true) or the previous answer is less than [`SYNC_REPEAT_WINDOW`]
    /// old, else `since`.
    fn since(&self, now: Instant, since: f64, used_up: bool) -> (f64, bool) {
        let Some(newest) = self.newest.filter(|n| since < *n) else {
            return (since, false);
        };
        if used_up {
            return (newest, true);
        }
        let repeat = self
            .answered
            .is_some_and(|t| now.saturating_duration_since(t) < SYNC_REPEAT_WINDOW);
        (if repeat { newest } else { since }, false)
    }

    /// Remembers an answer (oldest first) prepared at `now`.
    fn record(&mut self, now: Instant, list: &[(f64, Bytes)]) {
        if let Some(&(ts, _)) = list.last() {
            self.newest = Some(self.newest.map_or(ts, |n| n.max(ts)));
        }
        self.answered = Some(now);
    }
}

/// The address a sync allowance is shared by: an IPv4 address, or an IPv6 /64 (as the
/// per-address app limits count, [`address_limits`]); IPv4-mapped IPv6 counts as IPv4.
fn allowance_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(std::net::Ipv6Addr::from(
            u128::from(v6) & !u128::from(u64::MAX),
        )),
        v4 @ IpAddr::V4(_) => v4,
    }
}

/// A map whose entries expire after `ttl`, with a size cap (oldest out first).
#[derive(Debug)]
struct TtlMap<V> {
    map: HashMap<String, (V, Instant)>,
    order: VecDeque<(String, Instant)>,
    ttl: Duration,
    cap: usize,
}

impl<V> TtlMap<V> {
    fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            ttl,
            cap,
        }
    }

    fn get(&self, k: &str, now: Instant) -> Option<&V> {
        self.map
            .get(k)
            .filter(|(_, at)| now.saturating_duration_since(*at) < self.ttl)
            .map(|(v, _)| v)
    }

    /// The live value of `k`, to change in place (its insertion time stays).
    fn get_mut(&mut self, k: &str, now: Instant) -> Option<&mut V> {
        let ttl = self.ttl;
        self.map
            .get_mut(k)
            .filter(|(_, at)| now.saturating_duration_since(*at) < ttl)
            .map(|(v, _)| v)
    }

    /// Removes `k`; its entry in the expiry queue goes when it comes up (or at the next
    /// compaction in [`Self::insert`]).
    fn remove(&mut self, k: &str) -> Option<V> {
        self.map.remove(k).map(|(v, _)| v)
    }

    /// Inserts `k` unless it is already live; a live key keeps its value and its insertion time.
    /// True if `k` was inserted.
    fn insert(&mut self, k: String, v: V, now: Instant) -> bool {
        self.prune(now);
        if self.map.contains_key(&k) {
            return false;
        }
        self.order.push_back((k.clone(), now));
        self.map.insert(k, (v, now));
        while self.map.len() > self.cap {
            let Some((k, at)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&k).is_some_and(|(_, t)| *t == at) {
                self.map.remove(&k);
            }
        }
        if self.order.len() > 2 * self.map.len() + 16 {
            let map = &self.map;
            self.order
                .retain(|(k, at)| map.get(k).is_some_and(|(_, t)| t == at));
        }
        true
    }

    /// Entries in the expiry queue (one per live key).
    #[cfg(test)]
    fn queued(&self) -> usize {
        self.order.len()
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn prune(&mut self, now: Instant) {
        while let Some((k, at)) = self.order.front() {
            if now.saturating_duration_since(*at) < self.ttl {
                break;
            }
            if self.map.get(k).is_some_and(|(_, t)| t == at) {
                self.map.remove(k);
            }
            self.order.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// the relay

/// Which topic a connection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A Hugin app (public-node or private-node topic).
    Client,
    /// Another relay (node-mesh topic).
    Node,
}

/// Who is on the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerMeta {
    pub role: Role,
    /// The remote DHT public key (authenticated by Noise): what bans are keyed on.
    pub remote_key: [u8; 32],
    /// The remote IP address as the transport reports it (Hyperswarm: the address of the
    /// UDP stream, which for a relayed connection is the relay's), `None` when unknown. Client
    /// connections are limited per address, IPv6 per /64, /56 and /48
    /// ([`RelayConfig::max_clients_per_ip`]), node connections per address or /64
    /// ([`MAX_NODES_PER_ADDRESS`]).
    pub remote_ip: Option<IpAddr>,
}

enum Outgoing {
    Json(Bytes),
    Reply(u64),
    /// Flush what is queued, then close.
    Close,
}

impl Outgoing {
    /// Bytes counted against [`OUTBOX_BYTES`].
    fn queued_len(&self) -> usize {
        match self {
            Self::Json(b) => b.len(),
            Self::Reply(_) | Self::Close => 0,
        }
    }
}

/// The packet could not be queued (count or byte limit, or the connection is gone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutboxFull;

/// The sending side of a connection's queue, bounded by [`OUTBOX`] packets and
/// [`OUTBOX_BYTES`] bytes. `write_loop` gives the bytes back as it dequeues.
#[derive(Debug, Clone)]
struct OutboxTx {
    tx: mpsc::Sender<Outgoing>,
    bytes: Arc<AtomicUsize>,
    /// [`OUTBOX_BYTES`] or [`NODE_OUTBOX_BYTES`].
    limit: usize,
}

impl OutboxTx {
    fn channel(limit: usize) -> (Self, mpsc::Receiver<Outgoing>) {
        let (tx, rx) = mpsc::channel(OUTBOX);
        (
            Self {
                tx,
                bytes: Arc::new(AtomicUsize::new(0)),
                limit,
            },
            rx,
        )
    }

    fn try_send(&self, item: Outgoing) -> Result<(), OutboxFull> {
        let len = item.queued_len();
        if len > 0 {
            let before = self.bytes.fetch_add(len, Ordering::AcqRel);
            if before > 0 && before.saturating_add(len) > self.limit {
                self.bytes.fetch_sub(len, Ordering::AcqRel);
                return Err(OutboxFull);
            }
        }
        if self.tx.try_send(item).is_err() {
            self.bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(OutboxFull);
        }
        Ok(())
    }

    /// `write_loop` took a packet of `len` queued bytes.
    fn dequeued(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::AcqRel);
    }

    /// Bytes queued right now.
    fn queued_bytes(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }
}

/// Bytes of the send budget held by a sync answer ([`RelayConfig::queue_budget`]); given back
/// as they are written, the rest when it is dropped.
#[derive(Debug)]
struct Reserved {
    counter: Arc<AtomicUsize>,
    left: usize,
}

impl Reserved {
    fn new(counter: Arc<AtomicUsize>, len: usize) -> Self {
        counter.fetch_add(len, Ordering::AcqRel);
        Self { counter, left: len }
    }

    /// Holds `len` bytes from now on.
    fn resize(&mut self, len: usize) {
        if len >= self.left {
            self.counter.fetch_add(len - self.left, Ordering::AcqRel);
        } else {
            self.counter.fetch_sub(self.left - len, Ordering::AcqRel);
        }
        self.left = len;
    }

    /// `len` bytes were written.
    fn release(&mut self, len: usize) {
        let len = len.min(self.left);
        self.left -= len;
        self.counter.fetch_sub(len, Ordering::AcqRel);
    }
}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.left, Ordering::AcqRel);
    }
}

/// One of the [`RelayConfig::sync_slots`]; dropping it gives the slot to the next waiting
/// request ([`Relay::end_app_sync`]).
#[derive(Debug)]
struct AppSyncSlot {
    relay: Option<Relay>,
}

impl Drop for AppSyncSlot {
    fn drop(&mut self) {
        if let Some(relay) = self.relay.take() {
            relay.end_app_sync();
        }
    }
}

/// History syncs of apps: how many hold a slot, and the requests waiting for one (oldest first);
/// the requests of connections whose sync allowance is used up ([`SyncAllowance`]) wait in
/// `later`, behind all others.
#[derive(Debug, Default)]
struct SyncQueue {
    running: usize,
    waiting: VecDeque<oneshot::Sender<AppSyncSlot>>,
    later: VecDeque<oneshot::Sender<AppSyncSlot>>,
}

/// A `some` answer on its way to the app, apart from the connection's queue: `write_loop`
/// writes it in turns ([`write_turn`]) once the queue is empty. It holds its sync slot, its share
/// of the budget and the connection's sync turn until it is written or dropped.
#[derive(Debug)]
struct SyncFrame {
    body: Bytes,
    reserved: Reserved,
    _slot: Option<AppSyncSlot>,
    _turn: Option<OwnedMutexGuard<()>>,
}

#[derive(Debug)]
struct PeerEntry {
    role: Role,
    remote_key: [u8; 32],
    /// [`PeerMeta::remote_ip`] in its canonical form (IPv4-mapped IPv6 as IPv4), counted per
    /// prefix ([`same_prefix`]).
    remote_ip: Option<IpAddr>,
    tx: OutboxTx,
    xkrd: Arc<AtomicBool>,
    job_subscriber: bool,
    stop: CancellationToken,
    /// Milliseconds since [`Inner::epoch`] of the last served request ([`IDLE_EVICT_AFTER`]).
    last_useful: Arc<AtomicU64>,
    /// A post, registration or gossiped message from this connection was accepted.
    proven: Arc<AtomicBool>,
    /// [`Conn::allowance_until`].
    allowance_until: Arc<AtomicU64>,
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(b) => write!(f, "Json({} bytes)", b.len()),
            Self::Reply(id) => write!(f, "Reply({id})"),
            Self::Close => f.write_str("Close"),
        }
    }
}

/// How long the previous block's jobs may still pay for a post after a new block arrived.
pub const PREVIOUS_BLOCK_GRACE: Duration = Duration::from_secs(15);

/// Records when the relay first saw tip `height`; returns that moment.
fn observe_tip(seen: &mut Option<(u64, Instant)>, height: u64, at: Instant) -> Instant {
    match *seen {
        Some((h, first)) if h == height => first,
        _ => {
            *seen = Some((height, at));
            at
        }
    }
}

/// A job dated at `issued` may pay for a post at tip `tip`: a job for the current block always,
/// one for the previous block (at most [`MAX_JOB_AGE`] behind) only within
/// [`PREVIOUS_BLOCK_GRACE`] of when the current tip was first seen, older ones never. A tip lower
/// than the job's height (a reorganisation) counts as current.
fn job_fresh(tip: u64, issued: u64, tip_seen_at: Instant, now: Instant) -> bool {
    match tip.saturating_sub(issued) {
        0 => true,
        age if age <= MAX_JOB_AGE => {
            now.saturating_duration_since(tip_seen_at) <= PREVIOUS_BLOCK_GRACE
        }
        _ => false,
    }
}

#[derive(Debug)]
struct State {
    /// The main-chain tip height and when this relay first saw it (the previous block's jobs
    /// stay valid for [`PREVIOUS_BLOCK_GRACE`] after that).
    tip_seen: Option<(u64, Instant)>,
    peers: HashMap<u64, PeerEntry>,
    bans: HashMap<[u8; 32], Instant>,
    /// `hash:sig` of accepted messages (hugin-node `acceptedPowAuthCache`).
    accepted_auth: TtlMap<()>,
    /// Share key (`job_id:nonce:result`) -> the message it paid for.
    share_bindings: TtlMap<Binding>,
    /// `prev_id` window of client posts.
    prev_ids: PrevIds,
    /// Last [`GOSSIP_PREV_ID_WINDOW`] distinct `prev_id`s of our pool jobs, newest first.
    gossip_prev_ids: VecDeque<[u8; 32]>,
    /// Gossip waiting for a `prev_id` ([`GOSSIP_PENDING_TTL`], [`GOSSIP_PENDING_MAX`]), oldest
    /// first.
    pending_gossip: VecDeque<PendingGossip>,
    /// The current pool job as handed to apps.
    job: Option<Arc<AppJob>>,
    /// The last [`VALID_JOBS`] pool jobs, newest first: which app shares are also pool shares,
    /// and how old each job is.
    pool_targets: VecDeque<KnownJob>,
    /// Accepted messages waiting for the next client push (client views).
    outbox: Vec<Value>,
    global_jobs: Window,
    /// Remote keys (hex) we served an xkrd sync to within [`SYNC_COOLDOWN`].
    sync_served: TtlMap<()>,
    /// Incoming xkrd syncs in progress: connection id -> start ([`MAX_CONCURRENT_SYNCS_IN`]).
    syncs_in: HashMap<u64, Instant>,
}

/// A pool job as handed to apps: the pool's job object with `target` replaced by the app
/// target ([`RelayConfig::app_difficulty`]); `job_id` and `blob` stay the pool's, so shares
/// that meet the pool target can be submitted as they are.
#[derive(Debug)]
struct AppJob {
    pool: Arc<PoolJob>,
    raw: Value,
}

impl AppJob {
    fn new(pool: Arc<PoolJob>, app_target: &str) -> Self {
        let mut raw = pool.raw.clone();
        if let Some(o) = raw.as_object_mut() {
            o.insert("target".into(), Value::String(app_target.to_owned()));
        }
        Self { pool, raw }
    }
}

/// A pool job the relay handed out, dated with the main-chain tip at the time it arrived.
#[derive(Debug)]
struct KnownJob {
    job_id: String,
    blob: Vec<u8>,
    /// The pool's own target.
    target: String,
    /// The node's main-chain tip when the job arrived (`None`: no tip feed then).
    tip: Option<ChainTip>,
}

impl KnownJob {
    fn is(&self, job_id: &str, blob: &[u8]) -> bool {
        self.job_id == job_id && self.blob == blob
    }
}

/// What a share paid for: the message hash and [`pow::content_digest`] of its signed content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Binding {
    hash: String,
    content: [u8; 32],
    /// The message is still being checked ([`Reservation`]).
    pending: bool,
}

/// A share key held for one message while its post, registration or gossip is checked
/// ([`Relay::reserve_share`]). [`Self::confirm`] turns it into the final binding; dropped
/// without that, the key is free again.
#[derive(Debug)]
struct Reservation {
    relay: Relay,
    key: String,
    /// The pending binding as stored.
    binding: Binding,
    confirmed: bool,
}

impl Reservation {
    /// The message was accepted: the share stays bound to it.
    fn confirm(mut self) {
        self.confirmed = true;
        let now = Instant::now();
        let mut st = lock(&self.relay.inner.state);
        match st.share_bindings.get_mut(&self.key, now) {
            Some(b) if *b == self.binding => b.pending = false,
            Some(_) => {}
            None => {
                let binding = Binding {
                    pending: false,
                    ..self.binding.clone()
                };
                st.share_bindings.insert(self.key.clone(), binding, now);
            }
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.confirmed {
            return;
        }
        let now = Instant::now();
        let mut st = lock(&self.relay.inner.state);
        if st
            .share_bindings
            .get(&self.key, now)
            .is_some_and(|b| *b == self.binding)
        {
            st.share_bindings.remove(&self.key);
        }
    }
}

/// A gossiped message waiting for its job's `prev_id`.
#[derive(Debug)]
struct PendingGossip {
    hash: String,
    prev_id: [u8; 32],
    message: Value,
    at: Instant,
}

/// How a share key relates to a message ([`Relay::reserve_share`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    /// No share is bound.
    Free,
    /// Bound to this message hash.
    SameHash,
    /// Bound to another hash with the same content.
    SameContent,
    /// Bound to another message.
    Taken,
    /// Held for a message that is still being checked.
    Pending,
}

#[derive(Debug)]
struct Inner {
    cfg: RelayConfig,
    pool: Mutex<MessagePool>,
    jobs: Arc<dyn JobSource>,
    state: Mutex<State>,
    stats: Arc<HuginStats>,
    shutdown: CancellationToken,
    next_peer: AtomicU64,
    /// Share hashing permits ([`RelayConfig::max_concurrent_hashes`]).
    hash_permits: Arc<Semaphore>,
    /// Reference point of the `last_useful` timestamps.
    epoch: Instant,
    /// History syncs of apps ([`RelayConfig::sync_slots`]).
    syncs: Mutex<SyncQueue>,
    /// Bytes of sync answers prepared and not yet written ([`Reserved`]).
    sync_reserved: Arc<AtomicUsize>,
    /// Write turns of sync answers ([`RelayConfig::sync_writers`]); the semaphore is fair, so
    /// an answer that used its turn waits behind the others.
    sync_turns: Arc<Semaphore>,
    /// Sync allowances of app addresses ([`allowance_key`]); an entry that is full again and
    /// has no app connection is dropped ([`Relay::clean`]).
    allowances: Mutex<HashMap<IpAddr, SyncAllowance>>,
    /// Share hashes computed.
    #[cfg(test)]
    hashed: AtomicU64,
}

/// The relay; cheap to clone.
#[derive(Debug, Clone)]
pub struct Relay {
    inner: Arc<Inner>,
}

/// Per-connection state (hugin-node keeps these in `WeakMap`s keyed by the connection).
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent per-connection flags
struct ConnState {
    last_accepted: Option<Instant>,
    /// A post or registration of this connection is being checked ([`PostTurn`]).
    post_in_flight: bool,
    /// Gossip allowance ([`GOSSIP_BURST`], [`GOSSIP_PER_SECOND`]).
    gossip: Bucket,
    pow_strikes: u32,
    job_window: Window,
    rate_strikes: u32,
    sync_id: Option<u64>,
    sync_received: usize,
    sync_requested: bool,
    /// Window of `some` requests ([`CLIENT_SYNC_REQUESTS_PER_WINDOW`]).
    sync_window: Window,
    /// The sync allowance of a connection without an address ([`Relay::with_allowance`]).
    sync_allowance: SyncAllowance,
    /// The newest message this connection was sent ([`SYNC_REPEAT_WINDOW`]).
    sync_point: SyncPoint,
    /// Number of the newest `some` request; an older one that has not reached the sync slots
    /// yet is not answered (the newer answer covers it).
    sync_latest: u64,
    /// An `xkrd_sync_request` from this node was already answered (one per connection).
    sync_served: bool,
    /// This connection holds one of the [`MAX_CONCURRENT_SYNCS_IN`] slots.
    sync_slot: bool,
}

#[derive(Debug)]
struct Conn {
    id: u64,
    meta: PeerMeta,
    tx: OutboxTx,
    xkrd: Arc<AtomicBool>,
    stop: CancellationToken,
    state: Mutex<ConnState>,
    last_useful: Arc<AtomicU64>,
    proven: Arc<AtomicBool>,
    /// The connection's sync answers ([`SyncFrame`]); `serve` hands the receiver to
    /// `write_loop`.
    sync_tx: mpsc::Sender<SyncFrame>,
    sync_rx: Mutex<Option<mpsc::Receiver<SyncFrame>>>,
    /// One `some` answer per connection at a time (held by its [`SyncFrame`]).
    sync_turn: Arc<tokio::sync::Mutex<()>>,
    /// Milliseconds since [`Inner::epoch`] until the sync allowance is full again (0: not
    /// used up), for [`HuginStats::apps_on_allowance`] of connections without an address.
    allowance_until: Arc<AtomicU64>,
}

/// The one post or registration of a connection being checked; dropping it lets the next in.
#[derive(Debug)]
struct PostTurn<'a>(&'a Conn);

impl Drop for PostTurn<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).post_in_flight = false;
    }
}

/// What reading one bare-rpc message asks for.
enum Read {
    Packet(Value),
    Ignore,
    Ban,
}

/// Result of checking a gossiped or synced message ([`Relay::check_gossip`]).
enum Gossip {
    /// Store `message` (its [`pow::canonical_message`]); its share is held by `reservation`
    /// (`None`: already bound to this message).
    Valid {
        message: Value,
        reservation: Option<Reservation>,
    },
    /// Not stored; the sender is not banned.
    Drop(&'static str),
    /// The job's `prev_id` is not among ours yet: wait for our next pool job.
    Defer([u8; 32]),
    /// Not stored; the sender is banned.
    Invalid(&'static str),
}

/// Result of [`Relay::check_client_pow`].
#[allow(clippy::large_enum_variant)] // short-lived, one per post
enum ClientPow {
    /// The share already paid for this message (same hash or same content).
    Paid,
    /// The share `pow.auth` signs, hashed and valid and held for this message, and the job
    /// blob it was mined on; `for_pool` when it also meets the pool job's own target (it is
    /// submitted).
    Fresh {
        share: ShareSubmit,
        blob: Vec<u8>,
        reservation: Reservation,
        for_pool: bool,
    },
}

/// A failed [`Relay::check_client_pow`].
#[derive(Debug, Clone, Copy)]
enum PowFail {
    /// The share is bound to another message (`share_reused`, no strike).
    Reused,
    /// Refused with this reason and the current job, without a strike: a job our source did
    /// not hand out recently, or more than [`MAX_JOB_AGE`] blocks behind the main-chain tip
    /// (`stale_job`).
    Refused(&'static str),
    /// A PoW failure (a strike).
    Invalid(&'static str),
}

impl Relay {
    /// Creates the relay and starts its background tasks (job updates, client pushes, cleanup);
    /// they stop with `shutdown`. Must be called inside a tokio runtime.
    pub fn new(
        cfg: RelayConfig,
        jobs: Arc<dyn JobSource>,
        stats: Arc<HuginStats>,
        shutdown: CancellationToken,
    ) -> std::io::Result<Self> {
        let pool = match &cfg.data_dir {
            Some(dir) => MessagePool::open(dir, cfg.store_limits, now_ms())?,
            None => MessagePool::new(cfg.store_limits),
        };
        stats.messages.store(pool.len() as u64, Ordering::Relaxed);
        let hash_permits = Arc::new(Semaphore::new(
            cfg.max_concurrent_hashes.clamp(1, Semaphore::MAX_PERMITS),
        ));
        let sync_turns = Arc::new(Semaphore::new(
            cfg.sync_writers.clamp(1, Semaphore::MAX_PERMITS),
        ));
        stats
            .apps_cap
            .store(cfg.max_clients as u64, Ordering::Relaxed);
        let relay = Self {
            inner: Arc::new(Inner {
                cfg,
                pool: Mutex::new(pool),
                jobs,
                state: Mutex::new(State {
                    tip_seen: None,
                    peers: HashMap::new(),
                    bans: HashMap::new(),
                    accepted_auth: TtlMap::new(AUTH_CACHE_TTL, AUTH_CACHE_CAP),
                    share_bindings: TtlMap::new(SHARE_BINDING_TTL, SHARE_BINDING_CAP),
                    prev_ids: PrevIds::default(),
                    gossip_prev_ids: VecDeque::new(),
                    pending_gossip: VecDeque::new(),
                    job: None,
                    pool_targets: VecDeque::new(),
                    outbox: Vec::new(),
                    global_jobs: Window::default(),
                    sync_served: TtlMap::new(SYNC_COOLDOWN, SYNC_KEYS_CAP),
                    syncs_in: HashMap::new(),
                }),
                stats,
                shutdown,
                next_peer: AtomicU64::new(1),
                hash_permits,
                epoch: Instant::now(),
                syncs: Mutex::new(SyncQueue::default()),
                sync_reserved: Arc::new(AtomicUsize::new(0)),
                sync_turns,
                allowances: Mutex::new(HashMap::new()),
                #[cfg(test)]
                hashed: AtomicU64::new(0),
            }),
        };
        tokio::spawn(relay.clone().job_loop());
        tokio::spawn(relay.clone().flush_loop());
        tokio::spawn(relay.clone().clean_loop());
        Ok(relay)
    }

    #[must_use]
    pub fn stats(&self) -> Arc<HuginStats> {
        self.inner.stats.clone()
    }

    #[must_use]
    pub fn config(&self) -> &RelayConfig {
        &self.inner.cfg
    }

    /// [`RelayConfig::app_difficulty`], at least 1.
    #[must_use]
    pub fn app_difficulty(&self) -> u64 {
        self.inner.cfg.app_difficulty.max(1)
    }

    /// The 4-byte target of the jobs handed to apps ([`pow::target_for_difficulty`], the same
    /// encoding as hugin-p2p `difficultyToTarget`).
    #[must_use]
    pub fn app_target(&self) -> String {
        pow::target_for_difficulty(self.app_difficulty())
    }

    /// Least job difficulty of gossiped and synced messages:
    /// [`RelayConfig::min_share_difficulty`] ([`GOSSIP_MIN_DIFFICULTY`]), at least 1, whatever
    /// our own app difficulty.
    #[must_use]
    pub fn gossip_floor(&self) -> u64 {
        self.inner.cfg.min_share_difficulty.max(1)
    }

    /// The pool's own target of a recent pool job.
    fn pool_target(&self, job_id: &str, blob: &[u8]) -> Option<String> {
        lock(&self.inner.state)
            .pool_targets
            .iter()
            .find(|j| j.is(job_id, blob))
            .map(|j| j.target.clone())
    }

    /// Whether a job may still pay for a post: it was issued for the current main-chain tip or
    /// the previous one (tip height − job height ≤ [`MAX_JOB_AGE`]). Without a tip feed every
    /// job may; with one, a job we did not date (not among our recent jobs, or handed out
    /// before the feed had a tip) may not.
    fn job_is_fresh(&self, job_id: &str, blob: &[u8]) -> bool {
        let Some(now) = self.inner.jobs.tip() else {
            return true;
        };
        let at = Instant::now();
        let mut st = lock(&self.inner.state);
        let seen = observe_tip(&mut st.tip_seen, now.height, at);
        st.pool_targets
            .iter()
            .find(|j| j.is(job_id, blob))
            .and_then(|j| j.tip)
            .is_some_and(|issued| job_fresh(now.height, issued.height, seen, at))
    }

    /// Messages in the pool.
    #[must_use]
    pub fn pool_len(&self) -> usize {
        lock(&self.inner.pool).len()
    }

    #[must_use]
    pub fn has_message(&self, hash: &str) -> bool {
        lock(&self.inner.pool).contains(hash)
    }

    #[must_use]
    pub fn is_banned(&self, key: &[u8; 32]) -> bool {
        lock(&self.inner.state)
            .bans
            .get(key)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Bans `key` for `dur` and closes its connections.
    pub fn ban(&self, key: [u8; 32], dur: Duration) {
        let mut st = lock(&self.inner.state);
        st.bans.insert(key, Instant::now() + dur);
        for p in st.peers.values().filter(|p| p.remote_key == key) {
            let _ = p.tx.try_send(Outgoing::Close);
            p.stop.cancel();
        }
        drop(st);
        HuginStats::inc(&self.inner.stats.bans);
    }

    // -----------------------------------------------------------------------------------------
    // background tasks

    async fn job_loop(self) {
        let mut rx = self.inner.jobs.jobs();
        loop {
            let job = rx.borrow_and_update().clone();
            self.inner
                .stats
                .pool_ready
                .store(self.inner.jobs.is_ready(), Ordering::Relaxed);
            match job {
                Some(job) => self.on_job(&job),
                // No job now (no p2pool, or the node is not synchronized): apps get
                // `job_pending` until the next one.
                None => lock(&self.inner.state).job = None,
            }
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                r = rx.changed() => if r.is_err() { return },
            }
        }
    }

    /// hugin-node `poolConnector.on('job')` + `flush_jobs`. Gossip waiting for a `prev_id` that
    /// is now in the window is checked again.
    fn on_job(&self, job: &Arc<PoolJob>) {
        let mut st = lock(&self.inner.state);
        if st.job.as_ref().is_some_and(|j| j.pool.raw == job.raw) {
            return;
        }
        let blob = hex::decode(&job.blob).unwrap_or_default();
        // A job seen before keeps the tip it was first dated with.
        let index = st
            .pool_targets
            .iter()
            .position(|j| j.is(&job.job_id, &blob));
        let seen = index.and_then(|i| st.pool_targets.remove(i));
        let current = self.inner.jobs.tip();
        if let Some(t) = current {
            observe_tip(&mut st.tip_seen, t.height, Instant::now());
        }
        let tip = match seen {
            Some(j) => j.tip,
            None => current,
        };
        st.pool_targets.push_front(KnownJob {
            job_id: job.job_id.clone(),
            blob,
            target: job.target.clone(),
            tip,
        });
        st.pool_targets.truncate(VALID_JOBS);
        let mut window_changed = false;
        if let Some(prev) = job.prev_id {
            st.prev_ids.push(prev);
            if st.gossip_prev_ids.front() != Some(&prev) {
                st.gossip_prev_ids.retain(|p| *p != prev);
                st.gossip_prev_ids.push_front(prev);
                st.gossip_prev_ids.truncate(GOSSIP_PREV_ID_WINDOW);
                window_changed = true;
            }
        }
        let mut ready = Vec::new();
        if window_changed {
            prune_pending(&mut st.pending_gossip, Instant::now());
            let window = st.gossip_prev_ids.clone();
            let (now_ready, waiting): (VecDeque<_>, VecDeque<_>) =
                std::mem::take(&mut st.pending_gossip)
                    .into_iter()
                    .partition(|g| window.contains(&g.prev_id));
            st.pending_gossip = waiting;
            ready = now_ready.into_iter().map(|g| g.message).collect();
        }
        let app_job = Arc::new(AppJob::new(job.clone(), &self.app_target()));
        let bytes = json_bytes(&protocol::job(&app_job.raw, None));
        st.job = Some(app_job);
        for p in st.peers.values().filter(|p| p.job_subscriber) {
            let _ = p.tx.try_send(Outgoing::Json(bytes.clone()));
        }
        drop(st);
        if !ready.is_empty() {
            let relay = self.clone();
            tokio::spawn(async move { relay.recheck_pending(ready).await });
        }
    }

    /// Checks gossip that waited for a `prev_id` again and stores what is valid now (hugin-node
    /// `recheck_pending_gossip`).
    async fn recheck_pending(&self, messages: Vec<Value>) {
        for m in messages {
            let hash = m.get("hash").and_then(Value::as_str).unwrap_or_default();
            if lock(&self.inner.pool).contains(hash) {
                continue;
            }
            if let Gossip::Valid {
                message,
                reservation,
            } = self.check_gossip(&m, true).await
            {
                self.store_gossip(&message, reservation, None, true);
            }
        }
    }

    /// Queues gossip whose job `prev_id` we have not seen yet (hugin-node `defer_gossip`).
    fn defer_gossip(&self, message: &Value, prev_id: [u8; 32]) {
        let Some(hash) = message.get("hash").and_then(Value::as_str) else {
            return;
        };
        let now = Instant::now();
        let mut st = lock(&self.inner.state);
        prune_pending(&mut st.pending_gossip, now);
        if st.pending_gossip.iter().any(|g| g.hash == hash) {
            return;
        }
        while st.pending_gossip.len() >= GOSSIP_PENDING_MAX {
            st.pending_gossip.pop_front();
        }
        st.pending_gossip.push_back(PendingGossip {
            hash: hash.to_owned(),
            prev_id,
            message: message.clone(),
            at: now,
        });
    }

    async fn flush_loop(self) {
        let mut tick = tokio::time::interval(self.inner.cfg.flush_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                _ = tick.tick() => {
                    self.flush_clients();
                    self.refresh_admission();
                }
            }
        }
    }

    /// hugin-node `flush_client_messages`: one `new-message` packet with everything accepted
    /// since the last flush, to every client.
    fn flush_clients(&self) {
        let mut st = lock(&self.inner.state);
        if st.outbox.is_empty() {
            return;
        }
        let messages = std::mem::take(&mut st.outbox);
        let bytes = json_bytes(&protocol::new_messages(&messages));
        for p in st.peers.values().filter(|p| p.role == Role::Client) {
            if p.tx.try_send(Outgoing::Json(bytes.clone())).is_err() {
                p.stop.cancel();
            }
        }
    }

    async fn clean_loop(self) {
        let mut tick = tokio::time::interval(CLEAN_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.inner.shutdown.cancelled() => return,
                _ = tick.tick() => self.clean(),
            }
        }
    }

    /// hugin-node `cleaner` plus our cache pruning.
    fn clean(&self) {
        let now = Instant::now();
        {
            let mut pool = lock(&self.inner.pool);
            let n = pool.evict_expired(now_ms());
            if n > 0 {
                tracing::debug!(evicted = n, left = pool.len(), "Hugin pool cleaned");
            }
            self.update_pool_stats(&pool);
        }
        let mut st = lock(&self.inner.state);
        st.bans.retain(|_, until| *until > now);
        st.accepted_auth.prune(now);
        st.share_bindings.prune(now);
        st.sync_served.prune(now);
        prune_pending(&mut st.pending_gossip, now);
        let connected: Vec<IpAddr> = st
            .peers
            .values()
            .filter(|p| p.role == Role::Client)
            .filter_map(|p| p.remote_ip.map(allowance_key))
            .collect();
        lock(&self.inner.allowances)
            .retain(|key, a| a.left_at(now) < SYNC_ALLOWANCE_BYTES || connected.contains(key));
    }

    /// Milliseconds since [`Inner::epoch`].
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.inner.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn update_pool_stats(&self, pool: &MessagePool) {
        self.inner
            .stats
            .messages
            .store(pool.len() as u64, Ordering::Relaxed);
        self.inner
            .stats
            .pool_bytes
            .store(pool.bytes() as u64, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------------------------------
    // connections

    /// Serves one connection until it closes, is banned, or the relay shuts down. Dropping the
    /// stream at the end closes it.
    pub async fn serve<S>(&self, stream: S, meta: PeerMeta)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let Some((conn, rx)) = self.register(meta) else {
            return;
        };
        let (rd, wr) = tokio::io::split(stream);
        let sync_rx = lock(&conn.sync_rx).take();
        let mut writer = tokio::spawn(write_loop(
            wr,
            rx,
            sync_rx,
            conn.clone(),
            self.inner.shutdown.clone(),
            self.inner.sync_turns.clone(),
        ));

        let hello = match meta.role {
            Role::Client => protocol::client_hello(&self.inner.cfg.node_address),
            Role::Node => protocol::node_hello(&self.inner.cfg.xkrd_version),
        };
        conn.send(&hello);

        let max_frame = match meta.role {
            Role::Client => HUGIN_INBOUND_LIMIT + 64,
            Role::Node => XKRD_INBOUND_LIMIT + 64,
        };
        let mut framed = FramedRead::new(rd, BareRpcCodec::new(max_frame));
        let mut tasks: JoinSet<()> = JoinSet::new();
        // The first packet must come within the hello timeout (no ban).
        let hello_deadline = tokio::time::sleep(self.inner.cfg.hello_timeout);
        tokio::pin!(hello_deadline);
        let mut greeted = false;
        loop {
            let item = tokio::select! {
                () = conn.stop.cancelled() => break,
                () = &mut hello_deadline, if !greeted => {
                    tracing::debug!(peer = %short(&meta.remote_key), role = ?meta.role, "no Hugin packet within the hello timeout; closing");
                    break;
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => continue,
                item = framed.next() => item,
            };
            let msg = match item {
                None => break,
                Some(Err(RpcCodecError::FrameTooLarge(n))) => {
                    tracing::debug!(peer = %short(&meta.remote_key), bytes = n, "Hugin packet too large; banning");
                    self.ban_conn(&conn, BAN);
                    break;
                }
                Some(Err(e)) => {
                    tracing::debug!(peer = %short(&meta.remote_key), error = %e, "Hugin connection error");
                    break;
                }
                Some(Ok(m)) => m,
            };
            let read = read_message(&conn, msg);
            if matches!(read, Read::Packet(_)) {
                greeted = true;
            }
            match read {
                Read::Ignore => {}
                Read::Ban => {
                    self.ban_conn(&conn, BAN);
                    break;
                }
                Read::Packet(v) if is_heavy(meta.role, &v) => {
                    while tasks.len() >= MAX_INFLIGHT {
                        tasks.join_next().await;
                    }
                    let relay = self.clone();
                    let c = conn.clone();
                    tasks.spawn(async move { relay.handle(&c, &v).await });
                }
                // Everything else in arrival order (a node's `version` before its sync request,
                // sync batches in sequence).
                Read::Packet(v) => self.handle(&conn, &v).await,
            }
        }
        tasks.abort_all();
        let _ = conn.tx.try_send(Outgoing::Close);
        self.unregister(&conn);
        if tokio::time::timeout(CLOSE_FLUSH, &mut writer)
            .await
            .is_err()
        {
            // The queue was full or the peer stopped reading: drop the stream now.
            writer.abort();
        }
    }

    fn register(&self, meta: PeerMeta) -> Option<(Arc<Conn>, mpsc::Receiver<Outgoing>)> {
        let now = Instant::now();
        let mut st = lock(&self.inner.state);
        if st.bans.get(&meta.remote_key).is_some_and(|u| *u > now) {
            tracing::debug!(peer = %short(&meta.remote_key), "banned Hugin peer refused");
            return None;
        }
        let remote_ip = meta.remote_ip.map(|ip| ip.to_canonical());
        if let Some(ip) = remote_ip
            && let Some((bits, max)) =
                address_limits(meta.role, self.inner.cfg.max_clients_per_ip, ip)
                    .into_iter()
                    .find(|&(bits, max)| {
                        st.peers
                            .values()
                            .filter(|p| {
                                p.role == meta.role
                                    && p.remote_ip
                                        .is_some_and(|other| same_prefix(ip, other, bits))
                            })
                            .count()
                            >= max
                    })
        {
            tracing::debug!(peer = %short(&meta.remote_key), role = ?meta.role, %ip, prefix = bits, max, "Hugin connections per address reached; closing");
            return None;
        }
        if meta.role == Role::Client && self.refresh_admission_with(&st) {
            HuginStats::inc(&self.inner.stats.refused_busy);
            tracing::debug!(peer = %short(&meta.remote_key), "Hugin relay busy with app syncs; new app connection closed");
            return None;
        }
        let (count, max) = match meta.role {
            Role::Client => (
                st.peers.values().filter(|p| p.role == Role::Client).count(),
                self.inner.cfg.max_clients,
            ),
            Role::Node => (
                st.peers.values().filter(|p| p.role == Role::Node).count(),
                self.inner.cfg.max_nodes,
            ),
        };
        if count >= max && !self.evict_idle(&mut st, meta.role) {
            tracing::debug!(peer = %short(&meta.remote_key), role = ?meta.role, "Hugin connection limit reached");
            return None;
        }
        let id = self.inner.next_peer.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = OutboxTx::channel(match meta.role {
            Role::Client => OUTBOX_BYTES,
            Role::Node => NODE_OUTBOX_BYTES,
        });
        let stop = self.inner.shutdown.child_token();
        let xkrd = Arc::new(AtomicBool::new(false));
        let last_useful = Arc::new(AtomicU64::new(self.elapsed_ms()));
        let proven = Arc::new(AtomicBool::new(false));
        let allowance_until = Arc::new(AtomicU64::new(0));
        let (sync_tx, sync_rx) = mpsc::channel(1);
        st.peers.insert(
            id,
            PeerEntry {
                role: meta.role,
                remote_key: meta.remote_key,
                remote_ip,
                tx: tx.clone(),
                xkrd: xkrd.clone(),
                job_subscriber: false,
                stop: stop.clone(),
                last_useful: last_useful.clone(),
                proven: proven.clone(),
                allowance_until: allowance_until.clone(),
            },
        );
        self.count_peers(&st);
        drop(st);
        tracing::debug!(peer = %short(&meta.remote_key), role = ?meta.role, "Hugin peer connected");
        let conn = Arc::new(Conn {
            id,
            meta,
            tx,
            xkrd,
            stop,
            state: Mutex::new(ConnState::default()),
            last_useful,
            proven,
            sync_tx,
            sync_rx: Mutex::new(Some(sync_rx)),
            sync_turn: Arc::new(tokio::sync::Mutex::new(())),
            allowance_until,
        });
        Some((conn, rx))
    }

    /// Closes one connection of `role` per [`IDLE_EVICT_AFTER`] and removes it from the table.
    /// True if one was evicted.
    fn evict_idle(&self, st: &mut State, role: Role) -> bool {
        let now = self.elapsed_ms();
        let min_idle =
            u64::try_from(self.inner.cfg.idle_evict_after.as_millis()).unwrap_or(u64::MAX);
        let proven_idle = min_idle.saturating_mul(u64::from(PROVEN_IDLE_FACTOR));
        let victim = st
            .peers
            .iter()
            .filter(|(_, p)| p.role == role)
            .filter_map(|(id, p)| {
                let idle = now.saturating_sub(p.last_useful.load(Ordering::Relaxed));
                let proven = p.proven.load(Ordering::Relaxed);
                let needed = if proven { proven_idle } else { min_idle };
                (idle >= needed).then_some((*id, proven, idle))
            })
            .min_by_key(|(_, proven, idle)| (*proven, Reverse(*idle)))
            .map(|(id, _, _)| id);
        let Some(id) = victim else {
            return false;
        };
        if let Some(p) = st.peers.remove(&id) {
            tracing::debug!(peer = %short(&p.remote_key), role = ?role, "idle Hugin peer evicted for a new connection");
            let _ = p.tx.try_send(Outgoing::Close);
            p.stop.cancel();
        }
        true
    }

    /// `conn` sent a request that was served; `proven` when it paid for an accepted message.
    /// An app connection without an accepted post or registration keeps the time it connected
    /// ([`IDLE_EVICT_AFTER`]).
    fn mark_useful(&self, conn: &Conn, proven: bool) {
        if !proven && conn.meta.role == Role::Client && !conn.proven.load(Ordering::Relaxed) {
            return;
        }
        conn.last_useful.store(self.elapsed_ms(), Ordering::Relaxed);
        if proven {
            conn.proven.store(true, Ordering::Relaxed);
        }
    }

    fn unregister(&self, conn: &Conn) {
        self.release_sync_slot(conn);
        let mut st = lock(&self.inner.state);
        st.peers.remove(&conn.id);
        self.count_peers(&st);
        drop(st);
        tracing::debug!(peer = %short(&conn.meta.remote_key), role = ?conn.meta.role, "Hugin peer gone");
    }

    fn count_peers(&self, st: &State) {
        let (mut c, mut n, mut x) = (0u64, 0u64, 0u64);
        for p in st.peers.values() {
            match p.role {
                Role::Client => c += 1,
                Role::Node => {
                    n += 1;
                    if p.xkrd.load(Ordering::Relaxed) {
                        x += 1;
                    }
                }
            }
        }
        let s = &self.inner.stats;
        s.clients.store(c, Ordering::Relaxed);
        s.nodes.store(n, Ordering::Relaxed);
        s.xkrd_nodes.store(x, Ordering::Relaxed);
    }

    fn ban_conn(&self, conn: &Conn, dur: Duration) {
        tracing::info!(peer = %short(&conn.meta.remote_key), role = ?conn.meta.role, secs = dur.as_secs(), "Hugin peer banned");
        {
            let mut st = lock(&self.inner.state);
            st.bans.insert(conn.meta.remote_key, Instant::now() + dur);
        }
        HuginStats::inc(&self.inner.stats.bans);
        conn.close();
    }

    async fn handle(&self, conn: &Arc<Conn>, data: &Value) {
        match conn.meta.role {
            Role::Client => self.client_packet(conn, data).await,
            Role::Node => self.node_packet(conn, data).await,
        }
    }

    // -----------------------------------------------------------------------------------------
    // clients (hugin-node `client_message`)

    async fn client_packet(&self, conn: &Arc<Conn>, data: &Value) {
        if data.get("request").is_some() {
            self.client_request(conn, data).await;
            return;
        }
        if let Some(ty) = data.get("type") {
            match ty.as_str() {
                Some("job_request") => self.job_request(conn, data),
                Some("pow_tag") => conn.send(&protocol::success(id_of(data), false)),
                Some("register") => self.register_packet(conn, data).await,
                Some("post") => self.post(conn, data).await,
                _ => conn.send(&protocol::failure("unknown_type", id_of(data))),
            }
            return;
        }
        if data.get("register").is_some() {
            self.register_packet(conn, data).await;
        }
    }

    /// `{ request: true, type: 'some', timestamp }` (hugin-node `onrequest`). The answer is a
    /// history sync: it waits for the connection's previous answer (a newer request of the same
    /// connection replaces one that is still waiting there), then for a sync slot (in arrival
    /// order) and for room in the send budget, and is then sent apart from the connection's
    /// queue ([`SyncFrame`]). Its size counts against the sync allowance of the connection's
    /// address ([`Self::with_allowance`]), which also
    /// decides where the request waits for a slot and which messages it gets
    /// ([`SyncAllowance`], [`SYNC_REPEAT_WINDOW`]).
    async fn client_request(&self, conn: &Conn, data: &Value) {
        // hugin-node bans when `timestamp` is not a number; hugin-desktop sends none on its
        // first sync, so a missing or null timestamp counts as 0 here.
        let since = match data.get("timestamp") {
            None | Some(Value::Null) => Some(0.0),
            Some(v) => v.as_f64(),
        };
        let ty = data.get("type").and_then(Value::as_str);
        let (Some(since), Some("some")) = (since, ty.filter(|t| pow::js_len(t) <= 10)) else {
            tracing::debug!(peer = %short(&conn.meta.remote_key), "invalid Hugin request; banning");
            self.ban_conn(conn, BAN);
            return;
        };
        let allowed = lock(&conn.state)
            .sync_window
            .allow(Instant::now(), CLIENT_SYNC_REQUESTS_PER_WINDOW);
        if !allowed {
            conn.send(&protocol::failure("rate_limit", id_of(data)));
            return;
        }
        self.mark_useful(conn, false);
        let seq = {
            let mut cs = lock(&conn.state);
            cs.sync_latest += 1;
            cs.sync_latest
        };
        // The connection's previous answer first; while this one waits for it, a newer request
        // takes its place.
        let turn = loop {
            if lock(&conn.state).sync_latest != seq {
                return;
            }
            if let Ok(turn) = conn.sync_turn.clone().try_lock_owned() {
                break turn;
            }
            tokio::time::sleep(BUDGET_POLL).await;
        };
        let later = self.with_allowance(conn, |a| a.used_up_at(Instant::now()));
        let slot = self.app_sync_slot(later).await;
        let now = Instant::now();
        let used_up = self.with_allowance(conn, |a| a.used_up_at(now));
        let (since, moved, keep_all) = {
            let cs = lock(&conn.state);
            let (since, moved) = cs.sync_point.since(now, since, used_up);
            (since, moved, used_up && cs.sync_point.newest.is_some())
        };
        let mut list =
            lock(&self.inner.pool).since_capped_dated(since, MAX_SYNC_MESSAGES, SOME_MAX_BYTES);
        let (cut, full_in) =
            self.with_allowance(conn, |a| (a.charge(now, &mut list, keep_all), a.full_in()));
        lock(&conn.state).sync_point.record(now, &list);
        conn.allowance_until.store(
            full_in.map_or(0, |d| {
                let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
                self.elapsed_ms().saturating_add(ms).max(1)
            }),
            Ordering::Relaxed,
        );
        if cut || moved {
            HuginStats::inc(&self.inner.stats.syncs_limited);
        }
        let list: Vec<Bytes> = list.into_iter().map(|(_, m)| m).collect();
        // The answer's size before it is built: the messages, their commas and the wrapper.
        let estimate = list.iter().map(|m| m.len() + 1).sum::<usize>() + 64;
        let Some(mut reserved) = self.reserve_sync_bytes(conn, estimate).await else {
            return;
        };
        let body = protocol::some_response_raw(&list, id_of(data));
        drop(list);
        reserved.resize(body.len());
        let frame = SyncFrame {
            body,
            reserved,
            _slot: Some(slot),
            _turn: Some(turn),
        };
        // Fails only when the connection is gone; dropping the frame frees its slot and bytes.
        let _ = conn.sync_tx.try_send(frame);
    }

    /// Runs `f` on the sync allowance of `conn`: the one of its address ([`allowance_key`]), or
    /// its own when it has no address.
    fn with_allowance<R>(&self, conn: &Conn, f: impl FnOnce(&mut SyncAllowance) -> R) -> R {
        if let Some(ip) = conn.meta.remote_ip {
            f(lock(&self.inner.allowances)
                .entry(allowance_key(ip))
                .or_default())
        } else {
            f(&mut lock(&conn.state).sync_allowance)
        }
    }

    /// One of the [`RelayConfig::sync_slots`], in arrival order; with `later` (the sync
    /// allowance is used up) behind all requests without. A request whose connection closes
    /// while it waits is dropped with its task and skipped.
    async fn app_sync_slot(&self, later: bool) -> AppSyncSlot {
        loop {
            let waiting = {
                let mut q = lock(&self.inner.syncs);
                q.waiting.retain(|w| !w.is_closed());
                q.later.retain(|w| !w.is_closed());
                let free = q.running < self.inner.cfg.sync_slots.max(1)
                    && q.waiting.is_empty()
                    && (!later || q.later.is_empty());
                if free {
                    q.running += 1;
                    None
                } else {
                    let (tx, rx) = oneshot::channel();
                    if later {
                        q.later.push_back(tx);
                    } else {
                        q.waiting.push_back(tx);
                    }
                    Some(rx)
                }
            };
            self.refresh_admission();
            match waiting {
                None => {
                    return AppSyncSlot {
                        relay: Some(self.clone()),
                    };
                }
                Some(rx) => {
                    if let Ok(slot) = rx.await {
                        return slot;
                    }
                }
            }
        }
    }

    /// A sync slot was given back: the oldest waiting request whose connection is still open
    /// gets it, the requests in [`SyncQueue::later`] only when no other waits.
    fn end_app_sync(&self) {
        {
            let mut q = lock(&self.inner.syncs);
            q.running = q.running.saturating_sub(1);
            while q.running < self.inner.cfg.sync_slots.max(1) {
                let Some(next) = q.waiting.pop_front().or_else(|| q.later.pop_front()) else {
                    break;
                };
                q.running += 1;
                let slot = AppSyncSlot {
                    relay: Some(self.clone()),
                };
                if let Err(mut unused) = next.send(slot) {
                    // The request was dropped (its connection closed).
                    unused.relay = None;
                    q.running -= 1;
                }
            }
        }
        self.refresh_admission();
    }

    /// Bytes of control packets and notifications queued for all app connections.
    fn app_queue_bytes(st: &State) -> usize {
        st.peers
            .values()
            .filter(|p| p.role == Role::Client)
            .map(|p| p.tx.queued_bytes())
            .sum()
    }

    /// Holds `len` bytes of the send budget for a sync answer, waiting until they fit: the
    /// bytes queued for all apps plus `len` within [`RelayConfig::queue_budget`], or nothing
    /// queued at all. `None` when the connection closed meanwhile.
    async fn reserve_sync_bytes(&self, conn: &Conn, len: usize) -> Option<Reserved> {
        loop {
            if conn.stop.is_cancelled() {
                return None;
            }
            let queued = Self::app_queue_bytes(&lock(&self.inner.state));
            {
                // One reservation at a time, so two answers cannot both take the last room.
                let _q = lock(&self.inner.syncs);
                let total = queued.saturating_add(self.inner.sync_reserved.load(Ordering::Acquire));
                if total == 0 || total.saturating_add(len) <= self.inner.cfg.queue_budget {
                    return Some(Reserved::new(self.inner.sync_reserved.clone(), len));
                }
            }
            tokio::time::sleep(BUDGET_POLL).await;
        }
    }

    /// [`Self::refresh_admission_with`] with the state lock taken here.
    fn refresh_admission(&self) -> bool {
        let st = lock(&self.inner.state);
        self.refresh_admission_with(&st)
    }

    /// Updates the sync and queue counters in the stats and whether new app connections are
    /// paused ([`admission_paused`]); true while they are. Runs on every new app connection,
    /// every sync slot change and every client push ([`RelayConfig::flush_interval`]).
    fn refresh_admission_with(&self, st: &State) -> bool {
        let (running, queued) = {
            let mut q = lock(&self.inner.syncs);
            q.waiting.retain(|w| !w.is_closed());
            q.later.retain(|w| !w.is_closed());
            (q.running, q.waiting.len() + q.later.len())
        };
        let on_allowance = {
            let now_ms = self.elapsed_ms();
            let now = Instant::now();
            let allowances = lock(&self.inner.allowances);
            st.peers
                .values()
                .filter(|p| p.role == Role::Client)
                .filter(|p| match p.remote_ip {
                    Some(ip) => allowances
                        .get(&allowance_key(ip))
                        .is_some_and(|a| a.is_used_up(now)),
                    None => p.allowance_until.load(Ordering::Relaxed) > now_ms,
                })
                .count()
        };
        let bytes = Self::app_queue_bytes(st)
            .saturating_add(self.inner.sync_reserved.load(Ordering::Acquire));
        let s = &self.inner.stats;
        s.syncs_running.store(running as u64, Ordering::Relaxed);
        s.syncs_queued.store(queued as u64, Ordering::Relaxed);
        s.queued_bytes.store(bytes as u64, Ordering::Relaxed);
        s.apps_on_allowance
            .store(on_allowance as u64, Ordering::Relaxed);
        let was = s.admission_paused.load(Ordering::Relaxed);
        let paused = admission_paused(
            was,
            queued,
            bytes,
            self.inner.cfg.sync_slots.max(1),
            self.inner.cfg.queue_budget,
        );
        if paused != was {
            s.admission_paused.store(paused, Ordering::Relaxed);
            if paused {
                tracing::debug!(
                    syncs_queued = queued,
                    queued_bytes = bytes,
                    "new Hugin app connections paused"
                );
            } else {
                tracing::debug!(
                    syncs_queued = queued,
                    queued_bytes = bytes,
                    "new Hugin app connections resumed"
                );
            }
        }
        paused
    }

    /// hugin-node `job_request` with `rate_limit_request`.
    fn job_request(&self, conn: &Conn, data: &Value) {
        let now = Instant::now();
        let client_ok = lock(&conn.state)
            .job_window
            .allow(now, CLIENT_JOB_REQUESTS_PER_WINDOW);
        let mut st = lock(&self.inner.state);
        // Only requests within the client's own limit use up the global window, and only the
        // client's own excess earns strikes (hugin-node counts both, so one busy client could
        // use up the window and get every other client struck and timed out).
        let global_ok = client_ok && st.global_jobs.allow(now, GLOBAL_JOB_REQUESTS_PER_WINDOW);
        if !global_ok {
            drop(st);
            conn.send(&protocol::failure("rate_limit", id_of(data)));
            if client_ok {
                return;
            }
            let strikes = {
                let mut cs = lock(&conn.state);
                cs.rate_strikes += 1;
                cs.rate_strikes
            };
            if strikes >= RATE_STRIKES {
                self.ban_conn(conn, TIMEOUT_BAN);
            }
            return;
        }
        if let Some(p) = st.peers.get_mut(&conn.id) {
            p.job_subscriber = true;
        }
        let job = st.job.clone();
        drop(st);
        self.mark_useful(conn, false);
        if let Some(job) = job {
            conn.send(&protocol::job(&job.raw, Some(id_of(data))));
        } else {
            conn.send(&protocol::job_pending(id_of(data)));
        }
    }

    /// Cooldown + duplicate checks shared by posts and registrations. A connection has one post
    /// or registration checked at a time: while one is, the next is answered `cooldown` like one
    /// within [`POST_COOLDOWN`] of the last accepted. `Some` is this packet's turn, held until
    /// it is answered.
    fn precheck<'c>(
        &self,
        conn: &'c Conn,
        hash: Option<&Value>,
        pow: Option<&Value>,
        id: &Value,
    ) -> Option<PostTurn<'c>> {
        let now = Instant::now();
        let cooling = {
            let mut cs = lock(&conn.state);
            let cooling = cs.post_in_flight
                || cs
                    .last_accepted
                    .is_some_and(|t| now.saturating_duration_since(t) < POST_COOLDOWN);
            if !cooling {
                cs.post_in_flight = true;
            }
            cooling
        };
        if cooling {
            conn.send(&protocol::failure("cooldown", id));
            return None;
        }
        let turn = PostTurn(conn);
        if let Some(key) = pow::auth_cache_key(hash, pow)
            && lock(&self.inner.state)
                .accepted_auth
                .get(&key, now)
                .is_some()
        {
            conn.send(&protocol::success(id, true));
            return None;
        }
        Some(turn)
    }

    /// A PoW failure from a client: answer, count a strike, ban on the second.
    fn pow_strike(&self, conn: &Conn, reason: &str, id: &Value) {
        HuginStats::inc(&self.inner.stats.posts_rejected);
        conn.send(&serde_json::json!({ "reason": reason, "success": false, "id": id }));
        let strikes = {
            let mut cs = lock(&conn.state);
            cs.pow_strikes += 1;
            cs.pow_strikes
        };
        if strikes >= POW_STRIKES {
            self.ban_conn(conn, BAN);
        }
    }

    /// hugin-node `cheap_pow` / `cheap_pow_register` (shapes, freshness, signature), then the
    /// share `pow.auth` signs ([`pow::auth_share`]) is looked up in the share bindings and, if
    /// it paid for nothing yet, held for this message ([`Self::reserve_share`]) and hashed
    /// here. Before the signature, the job (id and blob) must be
    /// one our source handed out recently ([`JobSource::job_known`]), issued for the current
    /// main-chain tip or the previous one ([`Self::job_is_fresh`]), and its target at least as
    /// hard as our app target ([`RelayConfig::app_difficulty`]); otherwise the post is answered
    /// `stale_job` with the current job. A valid share is a pool share as well when it also
    /// meets the pool's target of that job.
    async fn check_client_pow(
        &self,
        hash: Option<&Value>,
        timestamp: Option<&Value>,
        pow: &Value,
        context: &str,
        digest: &[u8; 32],
    ) -> Result<ClientPow, PowFail> {
        const POW_INVALID: &str = "Invalid PoW shares.";
        if !pow::pow_version_is_current(Some(pow)) {
            return Err(PowFail::Invalid(POW_INVALID));
        }
        if pow
            .get("shares")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            return Err(PowFail::Invalid(POW_INVALID));
        }
        let prev_ids = lock(&self.inner.state).prev_ids;
        let validated = validate_pow_payload(hash, pow, Some(&prev_ids))
            .map_err(|_| PowFail::Invalid(POW_INVALID))?;
        if !self
            .inner
            .jobs
            .job_known(&validated.job.job_id, &validated.job.blob)
            || !self.job_is_fresh(&validated.job.job_id, &validated.job.blob)
        {
            return Err(PowFail::Refused("stale_job"));
        }
        if pow::share_difficulty(&validated.job.target).is_none_or(|d| d < self.app_difficulty()) {
            return Err(PowFail::Refused("stale_job"));
        }
        verify_pow_auth(hash, timestamp, pow, context).map_err(PowFail::Invalid)?;
        let share = pow::auth_share(pow)
            .ok()
            .and_then(|s| validate_share_fast(s, &validated.job.job_id).ok())
            .ok_or(PowFail::Invalid(POW_INVALID))?;
        let hash_str = hash.and_then(Value::as_str).unwrap_or_default();
        let reservation = match self.reserve_share(share.key(), hash_str, digest) {
            Ok(r) => r,
            Err(Claim::SameHash | Claim::SameContent) => return Ok(ClientPow::Paid),
            Err(_) => return Err(PowFail::Reused),
        };
        let (nonce, result) = (share.nonce.to_owned(), share.result.to_owned());
        if !self
            .hash_share(validated.job.clone(), nonce.clone(), result.clone())
            .await
        {
            return Err(PowFail::Invalid("pow_verify_failed"));
        }
        let for_pool = self
            .pool_target(&validated.job.job_id, &validated.job.blob)
            .is_some_and(|t| pow::result_meets_target(&result, &t));
        Ok(ClientPow::Fresh {
            share: ShareSubmit {
                job_id: validated.job.job_id,
                nonce,
                result,
            },
            blob: validated.job.blob,
            reservation,
            for_pool,
        })
    }

    /// One CN-Turtle share hash on the blocking pool, after taking one of the
    /// relay-wide [`RelayConfig::max_concurrent_hashes`] permits (FIFO wait). The permit moves
    /// into the blocking task, so it is held until the hash is done even if the waiting packet
    /// is cancelled (its connection closed).
    async fn hash_share(&self, job: PowJob, nonce: String, result: String) -> bool {
        let Ok(permit) = self.inner.hash_permits.clone().acquire_owned().await else {
            return false;
        };
        #[cfg(test)]
        self.inner.hashed.fetch_add(1, Ordering::Relaxed);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            pow::verify_share(&job, &nonce, &result)
        })
        .await
        .unwrap_or(false)
    }

    /// How the share `key` relates to the message `hash` with content digest `digest`; when it
    /// is free, it is held for this message in the same step ([`Reservation`]), so a share
    /// pays for one message also when several are checked at once.
    fn reserve_share(
        &self,
        key: String,
        hash: &str,
        digest: &[u8; 32],
    ) -> Result<Reservation, Claim> {
        let now = Instant::now();
        let mut st = lock(&self.inner.state);
        let claim = st.share_bindings.get(&key, now).map_or(Claim::Free, |b| {
            if b.pending {
                Claim::Pending
            } else if b.hash == hash {
                Claim::SameHash
            } else if b.content == *digest {
                Claim::SameContent
            } else {
                Claim::Taken
            }
        });
        if claim != Claim::Free {
            return Err(claim);
        }
        let binding = Binding {
            hash: hash.to_owned(),
            content: *digest,
            pending: true,
        };
        st.share_bindings.insert(key.clone(), binding.clone(), now);
        drop(st);
        Ok(Reservation {
            relay: self.clone(),
            key,
            binding,
            confirmed: false,
        })
    }

    /// hugin-node `submit_message_shares_with_reauth` for the one verified share, mined on
    /// `blob`, which pays for the post (counted in `shares_accepted`) when it returns `Ok`.
    /// Only a share that also meets the pool's target (`for_pool`) is submitted, and only when
    /// the source submits at all (a fee address is set). A share the pool refuses (stale,
    /// duplicate, too easy after a new block, ...) still pays for the post while its job is
    /// fresh ([`Self::job_is_fresh`]): the app did its work. It is counted in
    /// `shares_rejected_by_pool`; once the job is older, the post is answered `stale_job`.
    /// Every other share was checked here against the app target and is not submitted.
    async fn submit_share(
        &self,
        peer: &[u8; 32],
        share: &ShareSubmit,
        blob: &[u8],
        for_pool: bool,
    ) -> Result<(), PowFail> {
        if for_pool
            && self.inner.jobs.submits_shares()
            && let Err(rejects) = self.submit_to_pool(share, blob).await
        {
            if !self.job_is_fresh(&share.job_id, blob) {
                tracing::debug!(peer = %short(peer), ?rejects, "Hugin share: pool reject, job too old");
                return Err(PowFail::Refused("stale_job"));
            }
            tracing::debug!(peer = %short(peer), ?rejects, "Hugin share: pool reject, post accepted");
        }
        HuginStats::inc(&self.inner.stats.shares_accepted);
        Ok(())
    }

    /// One share to the pool, again once after a re-login when the pool asks for one.
    async fn submit_to_pool(
        &self,
        share: &ShareSubmit,
        blob: &[u8],
    ) -> Result<(), Vec<(String, Option<String>)>> {
        let mut retried = false;
        loop {
            HuginStats::inc(&self.inner.stats.shares_submitted);
            let outcome = self.inner.jobs.submit_for(share.clone(), blob).await;
            let auth_issue = outcome.is_auth_reject();
            let reject = match outcome {
                SubmitOutcome::Accepted => return Ok(()),
                // hugin-node counts this as accepted; the pool already has the share from
                // someone else, so it pays for nothing here.
                SubmitOutcome::Duplicate => ("duplicate_share".to_owned(), None),
                SubmitOutcome::Rejected { reason, error } => (reason, error),
            };
            HuginStats::inc(&self.inner.stats.shares_rejected_by_pool);
            if retried || !auth_issue {
                return Err(vec![reject]);
            }
            retried = true;
            let deadline = Instant::now() + RELOGIN_WAIT;
            while !self.inner.jobs.is_ready() && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if !self.inner.jobs.is_ready() {
                return Err(vec![reject]);
            }
        }
    }

    fn send_current_job(&self, conn: &Conn) {
        let job = lock(&self.inner.state).job.clone();
        if let Some(job) = job {
            conn.send(&protocol::job(&job.raw, None));
        }
    }

    /// A post or registration whose share already paid for it, or which failed (`fail`).
    fn answer_claimed(&self, conn: &Conn, id: &Value, fail: Option<PowFail>) {
        match fail {
            None => conn.send(&protocol::success(id, true)),
            Some(PowFail::Reused) => {
                HuginStats::inc(&self.inner.stats.posts_rejected);
                conn.send(
                    &serde_json::json!({ "reason": "share_reused", "success": false, "id": id }),
                );
            }
            Some(PowFail::Refused(reason)) => {
                HuginStats::inc(&self.inner.stats.posts_rejected);
                conn.send(&serde_json::json!({ "reason": reason, "success": false, "id": id }));
                self.send_current_job(conn);
            }
            Some(PowFail::Invalid(reason)) => self.pow_strike(conn, reason, id),
        }
    }

    /// hugin-node `client_post`. The message is stored and gossiped in its
    /// [`pow::canonical_message`] form.
    async fn post(&self, conn: &Arc<Conn>, data: &Value) {
        let message = data.get("message").filter(|m| m.is_object());
        let id = message.and_then(|m| m.get("id")).unwrap_or(&NULL);
        let hash = message.and_then(|m| m.get("hash"));
        let pow_v = message.and_then(|m| m.get("pow"));
        let Some(_turn) = self.precheck(conn, hash, pow_v, id) else {
            return;
        };
        let has_shares = pow_v
            .and_then(|p| p.get("shares"))
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let (Some(message), true) = (message, has_shares) else {
            HuginStats::inc(&self.inner.stats.posts_rejected);
            conn.send(&serde_json::json!({ "reason": "no_share", "success": false, "id": id }));
            self.ban_conn(conn, TIMEOUT_BAN);
            return;
        };
        if !check_message(message, now_ms(), MAX_MESSAGE_PAST_MS) {
            self.pow_strike(conn, "Wrong message format.", id);
            return;
        }
        let Some(canonical) = pow::canonical_message(message) else {
            self.pow_strike(conn, "Wrong message format.", id);
            return;
        };
        let hash_str = hash.and_then(Value::as_str).unwrap_or_default().to_owned();
        let dm = pow::message_kind(&canonical) == "dm";
        if dm && lock(&self.inner.pool).contains(&hash_str) {
            conn.send(&protocol::success(id, true));
            return;
        }
        let context = pow::auth_context_message(&canonical);
        let digest = pow::content_digest("msg", &context);
        let (share, blob, reservation, for_pool) = match self
            .check_client_pow(
                canonical.get("hash"),
                canonical.get("timestamp"),
                &canonical["pow"],
                &context,
                &digest,
            )
            .await
        {
            Ok(ClientPow::Fresh {
                share,
                blob,
                reservation,
                for_pool,
            }) => (share, blob, reservation, for_pool),
            Ok(ClientPow::Paid) => return self.answer_claimed(conn, id, None),
            Err(fail) => return self.answer_claimed(conn, id, Some(fail)),
        };
        if let Err(fail) = self
            .submit_share(&conn.meta.remote_key, &share, &blob, for_pool)
            .await
        {
            return self.answer_claimed(conn, id, Some(fail));
        }
        reservation.confirm();

        if dm {
            let added = lock(&self.inner.pool).insert(canonical.clone(), now_ms());
            match added {
                Insert::Added => {}
                Insert::Duplicate => {
                    conn.send(&protocol::success(id, true));
                    return;
                }
                Insert::Expired | Insert::Invalid => {
                    HuginStats::inc(&self.inner.stats.posts_rejected);
                    conn.send(&protocol::failure("Wrong message format.", id));
                    return;
                }
            }
        }
        {
            let mut cs = lock(&conn.state);
            cs.last_accepted = Some(Instant::now());
            cs.pow_strikes = 0;
        }
        self.mark_useful(conn, true);
        if let Some(key) = pow::auth_cache_key(hash, pow_v) {
            lock(&self.inner.state)
                .accepted_auth
                .insert(key, (), Instant::now());
        }
        HuginStats::inc(&self.inner.stats.posts_accepted);
        conn.send(&protocol::success(id, false));
        tracing::debug!(peer = %short(&conn.meta.remote_key), hash = %hash_str, dm, "Hugin message accepted");

        if dm {
            self.update_pool_stats(&lock(&self.inner.pool));
            self.gossip(&canonical, None, true);
        }
        if message.get("push").is_some() {
            self.notify_nodes(&serde_json::json!({ "type": "post", "message": canonical }));
        }
    }

    /// hugin-node `client_register`.
    async fn register_packet(&self, conn: &Arc<Conn>, data: &Value) {
        let id = match data.get("id") {
            Some(v) if v.is_number() => v,
            _ => data.get("timestamp").unwrap_or(&NULL),
        };
        let pow_v = data.get("pow");
        let Some(_turn) = self.precheck(conn, data.get("hash"), pow_v, id) else {
            return;
        };
        let well_formed = data.get("register") == Some(&Value::Bool(true))
            && data
                .get("data")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
        if !well_formed {
            self.pow_strike(conn, "Wrong message format.", id);
            return;
        }
        let Some(pow_v) = pow_v else {
            self.pow_strike(conn, "Invalid PoW shares.", id);
            return;
        };
        let context = pow::auth_context_register(data);
        let digest = pow::content_digest("reg", &context);
        let (share, blob, reservation, for_pool) = match self
            .check_client_pow(
                data.get("hash"),
                data.get("timestamp"),
                pow_v,
                &context,
                &digest,
            )
            .await
        {
            Ok(ClientPow::Fresh {
                share,
                blob,
                reservation,
                for_pool,
            }) => (share, blob, reservation, for_pool),
            Ok(ClientPow::Paid) => return self.answer_claimed(conn, id, None),
            Err(fail) => return self.answer_claimed(conn, id, Some(fail)),
        };
        if let Err(fail) = self
            .submit_share(&conn.meta.remote_key, &share, &blob, for_pool)
            .await
        {
            return self.answer_claimed(conn, id, Some(fail));
        }
        reservation.confirm();
        {
            let mut cs = lock(&conn.state);
            cs.last_accepted = Some(Instant::now());
            cs.pow_strikes = 0;
        }
        self.mark_useful(conn, true);
        if let Some(key) = pow::auth_cache_key(data.get("hash"), Some(pow_v)) {
            lock(&self.inner.state)
                .accepted_auth
                .insert(key, (), Instant::now());
        }
        HuginStats::inc(&self.inner.stats.registers_accepted);
        conn.send(&protocol::success(id, false));
        self.notify_nodes(data);
    }

    // -----------------------------------------------------------------------------------------
    // nodes (hugin-node `node_message`, plus our sync)

    async fn node_packet(&self, conn: &Arc<Conn>, data: &Value) {
        if data.get("signal").is_some() {
            self.on_signal(conn, data).await;
            return;
        }
        if data.get("push").is_some() {
            // For the push-notification cluster; hugin-nodes ignore it too.
            return;
        }
        match data.get("type").and_then(Value::as_str) {
            Some("xkrd_sync_request") if conn.xkrd.load(Ordering::Relaxed) => {
                self.serve_sync(conn, data);
                return;
            }
            Some("xkrd_sync") if conn.xkrd.load(Ordering::Relaxed) => {
                self.on_sync_batch(conn, data).await;
                return;
            }
            _ => {}
        }
        if data.get("version").is_some() {
            self.mark_useful(conn, false);
            let xkrd = data
                .get("xkrd")
                .and_then(|x| x.get("sync"))
                .and_then(Value::as_u64)
                .is_some_and(|v| v >= protocol::XKRD_SYNC_VERSION);
            tracing::debug!(
                peer = %short(&conn.meta.remote_key),
                version = %data["version"],
                xkrd,
                "Hugin node version"
            );
            if xkrd && !conn.xkrd.swap(true, Ordering::Relaxed) {
                self.count_peers(&lock(&self.inner.state));
                let first = {
                    let mut cs = lock(&conn.state);
                    let first = !cs.sync_requested;
                    cs.sync_requested = true;
                    first
                };
                if first && self.inner.cfg.sync_on_connect && self.take_sync_slot(conn) {
                    let id = u64::from(rand_u32()) + 1;
                    lock(&conn.state).sync_id = Some(id);
                    let since = (now_ms() - self.inner.cfg.store_limits.ttl_ms) as i64;
                    conn.send(&protocol::sync_request(id, since));
                }
            }
        }
    }

    /// hugin-node `check_gossip` for a message from another node, in its order:
    ///
    /// * shape ([`validate_pow_payload`] without the client freshness window): ban;
    /// * `pow.version` other than 2: drop;
    /// * job target: zero or unparsable ([`pow::share_difficulty`]): ban; difficulty below
    ///   [`Self::gossip_floor`] ([`RelayConfig::min_share_difficulty`]; the target is the app
    ///   target of the relay that took the post): drop;
    /// * `pow.auth` signature: ban;
    /// * with `prev_id_window`, the job's `prev_id` must be zero or among our last
    ///   [`GOSSIP_PREV_ID_WINDOW`] pool `prev_id`s: otherwise [`Gossip::Defer`]. The hashing
    ///   blob of an XKR job (p2pool, or any XKR pool) is the merge-mining parent header, whose
    ///   `prev_id` is zero, so relays on different p2pool nodes, or without one, take each
    ///   other's gossip;
    /// * the share `pow.auth` signs ([`pow::auth_share`]) must match the job: ban otherwise;
    /// * that share bound to another message, or held for one being checked: drop;
    /// * that share, and only that one, is hashed: not valid, ban. The other shares of the
    ///   message are not looked at; the message is stored with the signed share only.
    ///
    /// A valid message is returned in its [`pow::canonical_message`] form (drop if it is too
    /// large), with its share held for it ([`Self::reserve_share`]).
    async fn check_gossip(&self, message: &Value, prev_id_window: bool) -> Gossip {
        let Some(pow_v) = message.get("pow").filter(|p| p.is_object()) else {
            return Gossip::Invalid("missing_pow");
        };
        let hash = message.get("hash");
        let validated = match validate_pow_payload(hash, pow_v, None) {
            Ok(v) => v,
            Err(r) => return Gossip::Invalid(r),
        };
        if !pow::pow_version_is_current(Some(pow_v)) {
            return Gossip::Drop("wrong_pow_version");
        }
        match pow::share_difficulty(&validated.job.target) {
            None => return Gossip::Invalid("no_target"),
            Some(d) if d < self.gossip_floor() => {
                return Gossip::Drop("low_difficulty");
            }
            Some(_) => {}
        }
        let context = pow::auth_context_message(message);
        if let Err(r) = verify_pow_auth(hash, message.get("timestamp"), pow_v, &context) {
            return Gossip::Invalid(r);
        }
        let digest = pow::content_digest("msg", &context);
        if prev_id_window
            && validated.prev_id != [0u8; 32]
            && !lock(&self.inner.state)
                .gossip_prev_ids
                .contains(&validated.prev_id)
        {
            return Gossip::Defer(validated.prev_id);
        }
        let Some(share) = pow::auth_share(pow_v)
            .ok()
            .and_then(|s| validate_share_fast(s, &validated.job.job_id).ok())
        else {
            return Gossip::Invalid("no_matching_share");
        };
        let hash_str = hash.and_then(Value::as_str).unwrap_or_default();
        let reservation = match self.reserve_share(share.key(), hash_str, &digest) {
            Ok(r) => Some(r),
            Err(Claim::SameHash) => None,
            Err(Claim::SameContent) => return Gossip::Drop("duplicate_content"),
            Err(Claim::Pending) => return Gossip::Drop("share_pending"),
            Err(Claim::Free | Claim::Taken) => return Gossip::Drop("share_reused"),
        };
        if !self
            .hash_share(
                validated.job.clone(),
                share.nonce.to_owned(),
                share.result.to_owned(),
            )
            .await
        {
            return Gossip::Invalid("pow_verify_failed");
        }
        let Some(canonical) = pow::canonical_message(message) else {
            return Gossip::Drop("message_too_large");
        };
        Gossip::Valid {
            message: canonical,
            reservation,
        }
    }

    /// Stores a checked gossiped or synced message and keeps its share bound to it; with
    /// `forward`, a new DM is pushed to our clients and re-gossiped (except to `from`) while its
    /// timestamp is within [`MAX_MESSAGE_PAST_MS`]. True if it was added.
    fn store_gossip(
        &self,
        message: &Value,
        reservation: Option<Reservation>,
        from: Option<&Conn>,
        forward: bool,
    ) -> bool {
        if let Some(r) = reservation {
            r.confirm();
        }
        let added = lock(&self.inner.pool).insert(message.clone(), now_ms()) == Insert::Added;
        if added {
            self.update_pool_stats(&lock(&self.inner.pool));
            if let Some(c) = from {
                self.mark_useful(c, true);
            }
            if forward
                && self.inner.cfg.forward_gossip
                && pow::message_kind(message) == "dm"
                && check_message(message, now_ms(), MAX_MESSAGE_PAST_MS)
            {
                self.gossip(message, from.map(|c| c.id), true);
            }
        }
        added
    }

    /// `{ signal: true, message }`. Each node connection may send [`GOSSIP_BURST`] at once and
    /// [`GOSSIP_PER_SECOND`] after that; signals over that are dropped before they are read.
    async fn on_signal(&self, conn: &Arc<Conn>, data: &Value) {
        let allowed =
            lock(&conn.state)
                .gossip
                .take(Instant::now(), GOSSIP_BURST, GOSSIP_PER_SECOND);
        if !allowed {
            tracing::debug!(peer = %short(&conn.meta.remote_key), "Hugin gossip over the rate; dropped");
            return;
        }
        let Some(message) = data.get("message").filter(|m| m.is_object()) else {
            return;
        };
        if !check_message(message, now_ms(), MAX_MESSAGE_PAST_MS) {
            return;
        }
        let hash = message
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if lock(&self.inner.pool).contains(&hash) {
            return;
        }
        HuginStats::inc(&self.inner.stats.gossip_received);
        match self.check_gossip(message, true).await {
            Gossip::Invalid(reason) => {
                tracing::info!(peer = %short(&conn.meta.remote_key), reason, "invalid Hugin gossip; banning node");
                self.ban_conn(conn, BAN);
            }
            Gossip::Drop(reason) => {
                tracing::debug!(peer = %short(&conn.meta.remote_key), hash = %hash, reason, "Hugin gossip dropped");
            }
            Gossip::Defer(prev_id) => self.defer_gossip(message, prev_id),
            Gossip::Valid {
                message,
                reservation,
            } => {
                self.store_gossip(&message, reservation, Some(conn.as_ref()), true);
            }
        }
    }

    /// hugin-node `gossip`: queue for the client push and signal every other node.
    fn gossip(&self, message: &Value, except: Option<u64>, to_clients: bool) {
        let bytes = json_bytes(&protocol::signal(message));
        let mut st = lock(&self.inner.state);
        if to_clients {
            st.outbox.push(crate::store::client_view(message));
        }
        let mut sent = 0u64;
        for (id, p) in &st.peers {
            if p.role != Role::Node || Some(*id) == except {
                continue;
            }
            if p.tx.try_send(Outgoing::Json(bytes.clone())).is_ok() {
                sent += 1;
            }
        }
        st.outbox.truncate(10_000);
        drop(st);
        self.inner
            .stats
            .gossip_sent
            .fetch_add(sent, Ordering::Relaxed);
    }

    /// hugin-node `network.notify`: `{ push: true, message: packet }` to every node.
    fn notify_nodes(&self, packet: &Value) {
        let bytes = json_bytes(&protocol::push(packet));
        let st = lock(&self.inner.state);
        for p in st.peers.values().filter(|p| p.role == Role::Node) {
            let _ = p.tx.try_send(Outgoing::Json(bytes.clone()));
        }
    }

    /// One of the [`MAX_CONCURRENT_SYNCS_IN`] slots for receiving a sync from `conn`.
    fn take_sync_slot(&self, conn: &Conn) -> bool {
        let now = Instant::now();
        let mut st = lock(&self.inner.state);
        st.syncs_in
            .retain(|_, since| now.saturating_duration_since(*since) < SYNC_SLOT_TIMEOUT);
        if st.syncs_in.len() >= MAX_CONCURRENT_SYNCS_IN {
            tracing::debug!(peer = %short(&conn.meta.remote_key), "Hugin sync not requested: enough syncs in progress");
            return false;
        }
        st.syncs_in.insert(conn.id, now);
        drop(st);
        lock(&conn.state).sync_slot = true;
        true
    }

    /// Ends `conn`'s incoming sync (done, over the message limit, or the connection closed).
    fn release_sync_slot(&self, conn: &Conn) {
        let held = {
            let mut cs = lock(&conn.state);
            cs.sync_id = None;
            std::mem::replace(&mut cs.sync_slot, false)
        };
        if held {
            lock(&self.inner.state).syncs_in.remove(&conn.id);
        }
    }

    /// Answers an `xkrd_sync_request` with our pool since `since` (full messages, in batches).
    fn serve_sync(&self, conn: &Arc<Conn>, data: &Value) {
        // One full pool dump per connection: each costs up to SYNC_MAX_MESSAGES clones and
        // serializations, and a peer could otherwise repeat the request in a loop.
        if std::mem::replace(&mut lock(&conn.state).sync_served, true) {
            tracing::debug!(peer = %short(&conn.meta.remote_key), "repeated xkrd sync request ignored");
            return;
        }
        // And once per remote key per cooldown (reconnect loops), with a relay-wide
        // cap on distinct keys per cooldown (keys are free).
        {
            let now = Instant::now();
            let key = hex::encode(conn.meta.remote_key);
            let mut st = lock(&self.inner.state);
            st.sync_served.prune(now);
            if st.sync_served.get(&key, now).is_some()
                || st.sync_served.len() >= MAX_SYNC_SERVES_PER_COOLDOWN
            {
                drop(st);
                tracing::debug!(peer = %short(&conn.meta.remote_key), "xkrd sync request refused (cooldown)");
                conn.send(&protocol::sync_batch(id_of(data), &[], true));
                return;
            }
            st.sync_served.insert(key, (), now);
        }
        let floor = now_ms() - self.inner.cfg.store_limits.ttl_ms;
        let since = data
            .get("since")
            .and_then(Value::as_f64)
            .unwrap_or(floor)
            .max(floor);
        let id = id_of(data).clone();
        let all = lock(&self.inner.pool).since_full(since, SYNC_MAX_MESSAGES);
        self.mark_useful(conn, false);
        let mut batch: Vec<Bytes> = Vec::new();
        let mut packets: Vec<Bytes> = Vec::new();
        let mut bytes = 0usize;
        let total = all.len();
        for (_, m) in all {
            let size = m.len();
            if !batch.is_empty()
                && (batch.len() >= SYNC_BATCH_MESSAGES || bytes + size > SYNC_BATCH_BYTES)
            {
                packets.push(protocol::sync_batch_raw(
                    &id,
                    &std::mem::take(&mut batch),
                    false,
                ));
                bytes = 0;
            }
            bytes += size;
            batch.push(m);
        }
        packets.push(protocol::sync_batch_raw(&id, &batch, true));
        send_paced(conn, packets);
        tracing::debug!(peer = %short(&conn.meta.remote_key), messages = total, "serving a Hugin sync");
    }

    /// One `xkrd_sync` batch: each new DM is checked like gossip (with the pool's age limit,
    /// 24 h by default, and without the `prev_id` window) and stored; synced messages are not
    /// re-gossiped.
    async fn on_sync_batch(&self, conn: &Arc<Conn>, data: &Value) {
        let expected = lock(&conn.state).sync_id;
        if expected.is_none() || data.get("id").and_then(Value::as_u64) != expected {
            return;
        }
        self.mark_useful(conn, false);
        let messages = data
            .get("messages")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let max_past = self.inner.cfg.store_limits.ttl_ms;
        let mut added = 0u64;
        for m in messages {
            {
                let mut cs = lock(&conn.state);
                cs.sync_received += 1;
                if cs.sync_received > SYNC_MAX_MESSAGES {
                    drop(cs);
                    self.release_sync_slot(conn);
                    return;
                }
            }
            if !check_message(m, now_ms(), max_past) {
                continue;
            }
            let hash = m.get("hash").and_then(Value::as_str).unwrap_or_default();
            if pow::message_kind(m) != "dm" || lock(&self.inner.pool).contains(hash) {
                continue;
            }
            match self.check_gossip(m, false).await {
                Gossip::Invalid(reason) => {
                    tracing::info!(peer = %short(&conn.meta.remote_key), reason, "invalid message in a Hugin sync; banning node");
                    self.ban_conn(conn, BAN);
                    return;
                }
                Gossip::Drop(_) | Gossip::Defer(_) => {}
                Gossip::Valid {
                    message,
                    reservation,
                } => {
                    if self.store_gossip(&message, reservation, Some(conn.as_ref()), false) {
                        added += 1;
                    }
                }
            }
        }
        self.inner
            .stats
            .synced_in
            .fetch_add(added, Ordering::Relaxed);
        self.update_pool_stats(&lock(&self.inner.pool));
        if data.get("done") == Some(&Value::Bool(true)) {
            self.release_sync_slot(conn);
            let received = lock(&conn.state).sync_received;
            tracing::info!(peer = %short(&conn.meta.remote_key), received, pool = self.pool_len(), "Hugin sync from an xkrd node done");
        }
    }
}

/// One bare-rpc message (hugin-node `setup_rpc`): size limit, JSON, falsy values are bans;
/// every request gets the fixed reply.
fn read_message(conn: &Conn, msg: RpcMessage) -> Read {
    let RpcMessage::Request {
        id,
        stream: 0,
        data: Some(data),
        ..
    } = msg
    else {
        // Replies to our requests, streams: nothing to do.
        return Read::Ignore;
    };
    let xkrd = conn.meta.role == Role::Node && conn.xkrd.load(Ordering::Relaxed);
    let limit = if xkrd {
        XKRD_INBOUND_LIMIT
    } else {
        HUGIN_INBOUND_LIMIT
    };
    if data.len() > limit {
        return Read::Ban;
    }
    // Claiming xkrd sync lifts the limit to 1 MiB only for the sync batches we asked for; outside
    // our own sync, anything above twice the Hugin limit (room for the `signal` wrapper around a
    // maximal client post) is dropped unparsed. No ban: a node's batch may still be in flight after
    // we stopped its sync.
    if xkrd && data.len() > 2 * HUGIN_INBOUND_LIMIT && lock(&conn.state).sync_id.is_none() {
        return Read::Ignore;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&data) else {
        return Read::Ban;
    };
    if !truthy(Some(&value)) {
        return Read::Ban;
    }
    if id != 0 {
        conn.reply(id);
    }
    if value.is_object() {
        Read::Packet(value)
    } else {
        Read::Ignore
    }
}

/// Queues `packets` on `conn` in order: what fits now at once, the rest from a task that waits
/// for the queue to drain (at most [`SYNC_SEND_WAIT`] per packet, then the rest is dropped).
fn send_paced(conn: &Arc<Conn>, mut packets: Vec<Bytes>) {
    let mut sent = 0;
    for p in &packets {
        if !conn.has_room(p.len()) {
            break;
        }
        if !conn.send_bytes(p.clone()) {
            return;
        }
        sent += 1;
    }
    if sent == packets.len() {
        return;
    }
    let rest = packets.split_off(sent);
    let conn = conn.clone();
    tokio::spawn(async move {
        for p in rest {
            let deadline = Instant::now() + SYNC_SEND_WAIT;
            while !conn.has_room(p.len()) {
                if conn.stop.is_cancelled() || Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if !conn.send_bytes(p) {
                return;
            }
        }
    });
}

impl Conn {
    /// Whether a packet of `len` bytes fits into the queue now.
    fn has_room(&self, len: usize) -> bool {
        let queued = self.tx.queued_bytes();
        queued == 0 || queued.saturating_add(len) <= self.tx.limit
    }

    /// Queues a JSON packet; a full queue closes the connection (slow peer).
    fn send(&self, v: &Value) {
        self.send_bytes(json_bytes(v));
    }

    fn send_bytes(&self, b: Bytes) -> bool {
        if self.tx.try_send(Outgoing::Json(b)).is_ok() {
            true
        } else {
            self.stop.cancel();
            false
        }
    }

    fn reply(&self, id: u64) {
        if self.tx.try_send(Outgoing::Reply(id)).is_err() {
            self.stop.cancel();
        }
    }

    /// Flushes what is queued and closes.
    fn close(&self) {
        let _ = self.tx.try_send(Outgoing::Close);
        self.stop.cancel();
    }
}

/// What `write_loop` does next.
enum WriteStep {
    Queue(Option<Outgoing>),
    Sync(Option<SyncFrame>),
    Turn,
}

/// The next sync answer of a connection; never ready without a receiver.
async fn next_sync(rx: &mut Option<mpsc::Receiver<SyncFrame>>) -> Option<SyncFrame> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// A sync answer being written: the frame, its bare-rpc header and the bytes written so far.
struct SyncWrite {
    frame: SyncFrame,
    head: Bytes,
    written: usize,
    /// When the last byte of it was written (or it started).
    progress_at: Instant,
}

/// Writes queued packets as bare-rpc requests (ids 1, 2, ...) and replies, and the
/// connection's sync answers ([`SyncFrame`]): the queue first, then an answer in turns of
/// [`SYNC_CHUNK`] bytes shared with every other connection's answers (`turns`). An answer is one
/// bare-rpc packet, so once its first byte is out the queue waits until it is complete.
async fn write_loop<W>(
    wr: W,
    mut rx: mpsc::Receiver<Outgoing>,
    mut sync_rx: Option<mpsc::Receiver<SyncFrame>>,
    conn: Arc<Conn>,
    shutdown: CancellationToken,
    turns: Arc<Semaphore>,
) where
    W: AsyncWrite + Send + Unpin + 'static,
{
    let mut sink = FramedWrite::new(wr, BareRpcCodec::new(usize::MAX));
    let mut next_id = 0u64;
    let mut sending: Option<SyncWrite> = None;
    loop {
        let started = sending.as_ref().is_some_and(|w| w.written > 0);
        let idle = sending.is_none();
        let (step, permit) = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            item = rx.recv(), if !started => (WriteStep::Queue(item), None),
            frame = next_sync(&mut sync_rx), if idle => (WriteStep::Sync(frame), None),
            permit = turns.clone().acquire_owned(), if !idle => match permit {
                Ok(p) => (WriteStep::Turn, Some(p)),
                Err(_) => break,
            },
        };
        match step {
            WriteStep::Queue(item) => {
                let msg = match item {
                    None | Some(Outgoing::Close) => break,
                    Some(Outgoing::Reply(id)) => RpcMessage::reply_ok(id),
                    Some(Outgoing::Json(bytes)) => {
                        conn.tx.dequeued(bytes.len());
                        // hugin-node bans node peers that send more than 9000 bytes.
                        if conn.meta.role == Role::Node
                            && !conn.xkrd.load(Ordering::Relaxed)
                            && bytes.len() > HUGIN_INBOUND_LIMIT
                        {
                            tracing::debug!(
                                bytes = bytes.len(),
                                "not sending an oversized packet to a hugin-node"
                            );
                            continue;
                        }
                        next_id += 1;
                        RpcMessage::packet(next_id, bytes)
                    }
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            WriteStep::Sync(None) => sync_rx = None,
            WriteStep::Sync(Some(frame)) => {
                next_id += 1;
                let head = protocol::packet_header(next_id, frame.body.len()).freeze();
                sending = Some(SyncWrite {
                    frame,
                    head,
                    written: 0,
                    progress_at: Instant::now(),
                });
            }
            WriteStep::Turn => {
                let Some(w) = sending.as_mut() else {
                    continue;
                };
                // The sink flushed its buffer with the last packet, so raw bytes go out in order.
                match write_turn(sink.get_mut(), w).await {
                    Err(_) => break,
                    Ok(true) => {
                        if sink.get_mut().flush().await.is_err() {
                            break;
                        }
                        sending = None;
                    }
                    Ok(false) if w.progress_at.elapsed() >= SYNC_SEND_WAIT => {
                        tracing::debug!(peer = %short(&conn.meta.remote_key), "Hugin app does not read its sync answer; closing");
                        break;
                    }
                    Ok(false) => {}
                }
                // The turn ends here: the next waiting answer goes first.
                drop(permit);
            }
        }
    }
    let _ = sink.close().await;
    conn.stop.cancel();
}

/// One turn of a sync answer: up to [`SYNC_CHUNK`] more bytes (the header first), for at most
/// [`SYNC_TURN`]; the written bytes go back to the budget. True when the answer is complete.
async fn write_turn<W>(wr: &mut W, w: &mut SyncWrite) -> std::io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let head = w.head.len();
    let total = head + w.frame.body.len();
    let end = w.written.saturating_add(SYNC_CHUNK).min(total);
    let turn = tokio::time::sleep(SYNC_TURN);
    tokio::pin!(turn);
    while w.written < end {
        let buf = if w.written < head {
            w.head.slice(w.written..)
        } else {
            w.frame.body.slice(w.written - head..end - head)
        };
        let n = tokio::select! {
            biased;
            r = wr.write(&buf) => r?,
            () = &mut turn => break,
        };
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let body_before = w.written.saturating_sub(head);
        w.written += n;
        w.frame
            .reserved
            .release(w.written.saturating_sub(head) - body_before);
        w.progress_at = Instant::now();
    }
    Ok(w.written >= total)
}

/// Whether new app connections are paused, given whether they are now (`paused`), the `some`
/// requests waiting for a slot (`queued`), the bytes queued for all apps (`bytes`), the sync
/// slots and the send budget. They are paused when more requests wait than there are slots or
/// the bytes reach [`ADMISSION_PAUSE_PERCENT`] of the budget, and resumed once at most half the
/// slots' worth of requests wait and the bytes are below [`ADMISSION_RESUME_PERCENT`].
#[must_use]
pub fn admission_paused(
    paused: bool,
    queued: usize,
    bytes: usize,
    slots: usize,
    budget: usize,
) -> bool {
    let share = |percent: usize| budget.saturating_mul(percent) / 100;
    if paused {
        queued > slots / 2 || bytes >= share(ADMISSION_RESUME_PERCENT)
    } else {
        queued > slots || bytes >= share(ADMISSION_PAUSE_PERCENT)
    }
}

/// Packets that wait for a pool, a sync slot or the send budget, or hash a share, run
/// concurrently; the rest inline.
fn is_heavy(role: Role, v: &Value) -> bool {
    match role {
        Role::Client => {
            v.get("request").is_some()
                || matches!(
                    v.get("type").and_then(Value::as_str),
                    Some("post" | "register")
                )
                || (v.get("type").is_none() && v.get("register").is_some())
        }
        Role::Node => v.get("signal").is_some(),
    }
}

static NULL: Value = Value::Null;

/// `data.id` (or null), the id hugin-node echoes in answers.
fn id_of(data: &Value) -> &Value {
    data.get("id").unwrap_or(&NULL)
}

fn rand_u32() -> u32 {
    xkr_crypto::os_rng().next_u32()
}

/// Drops pending gossip older than [`GOSSIP_PENDING_TTL`] (the queue is oldest first).
fn prune_pending(queue: &mut VecDeque<PendingGossip>, now: Instant) {
    while queue
        .front()
        .is_some_and(|g| now.saturating_duration_since(g.at) > GOSSIP_PENDING_TTL)
    {
        queue.pop_front();
    }
}

/// The per-address limits of a new `role` connection from `ip` (in its canonical form), as
/// `(prefix length in bits, most connections)`. Apps: `per_ip` per IPv4 address or IPv6 /64
/// (one host or home network, as xkrd counts its XKR peers), and per IPv6 /56 and /48
/// [`MAX_CLIENTS_PER_56`] and [`MAX_CLIENTS_PER_48`] (at least `per_ip`); none when `per_ip` is
/// 0. Nodes: [`MAX_NODES_PER_ADDRESS`] per IPv4 address or IPv6 /64.
fn address_limits(role: Role, per_ip: usize, ip: IpAddr) -> Vec<(u8, usize)> {
    let host = if ip.is_ipv4() { 32 } else { 64 };
    match role {
        Role::Node => vec![(host, MAX_NODES_PER_ADDRESS)],
        Role::Client if per_ip == 0 => Vec::new(),
        Role::Client if ip.is_ipv4() => vec![(host, per_ip)],
        Role::Client => vec![
            (host, per_ip),
            (56, MAX_CLIENTS_PER_56.max(per_ip)),
            (48, MAX_CLIENTS_PER_48.max(per_ip)),
        ],
    }
}

/// Whether `a` and `b` share their first `bits` bits (an IPv4 and an IPv6 address never do).
fn same_prefix(a: IpAddr, b: IpAddr, bits: u8) -> bool {
    fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
        let full = usize::from(bits / 8).min(a.len());
        let rest = bits % 8;
        a[..full] == b[..full]
            && (rest == 0 || full >= a.len() || (a[full] ^ b[full]) & (0xff_u8 << (8 - rest)) == 0)
    }
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => prefix_eq(&a.octets(), &b.octets(), bits),
        (IpAddr::V6(a), IpAddr::V6(b)) => prefix_eq(&a.octets(), &b.octets(), bits),
        _ => false,
    }
}

fn short(key: &[u8; 32]) -> String {
    hex::encode(&key[..6])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::pool::ManualJobSource;

    fn relay_with(f: impl FnOnce(&mut RelayConfig)) -> Relay {
        let mut cfg = RelayConfig::new("SEKR".into());
        f(&mut cfg);
        Relay::new(
            cfg,
            Arc::new(ManualJobSource::default()),
            Arc::new(HuginStats::default()),
            CancellationToken::new(),
        )
        .unwrap()
    }

    fn conn(relay: &Relay, role: Role, key: u8) -> (Arc<Conn>, mpsc::Receiver<Outgoing>) {
        relay
            .register(PeerMeta {
                role,
                remote_key: [key; 32],
                remote_ip: None,
            })
            .expect("registered")
    }

    /// Registers a connection from `ip`; `None` if it was refused.
    fn conn_from(
        relay: &Relay,
        role: Role,
        key: u8,
        ip: Option<&str>,
    ) -> Option<(Arc<Conn>, mpsc::Receiver<Outgoing>)> {
        relay.register(PeerMeta {
            role,
            remote_key: [key; 32],
            remote_ip: ip.map(|s| s.parse().unwrap()),
        })
    }

    /// Queued JSON packets, without waiting.
    fn drain(rx: &mut mpsc::Receiver<Outgoing>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let Outgoing::Json(b) = item {
                out.push(serde_json::from_slice(&b).unwrap_or(Value::Null));
            }
        }
        out
    }

    /// The outbox is bounded by bytes as well as by packets.
    #[test]
    fn outbox_is_bounded_by_bytes() {
        let (tx, mut rx) = OutboxTx::channel(OUTBOX_BYTES);
        let one_mib = Bytes::from(vec![b'x'; 1024 * 1024]);
        assert!(tx.try_send(Outgoing::Json(one_mib.clone())).is_ok());
        assert!(tx.try_send(Outgoing::Json(one_mib.clone())).is_ok());
        // 3 MiB would exceed OUTBOX_BYTES (2 MiB)
        assert_eq!(
            tx.try_send(Outgoing::Json(one_mib.clone())),
            Err(OutboxFull)
        );
        assert_eq!(tx.queued_bytes(), 2 * 1024 * 1024);
        // replies and close carry no bytes and still fit
        assert!(tx.try_send(Outgoing::Reply(1)).is_ok());
        // the writer takes one packet: room again
        if let Ok(Outgoing::Json(b)) = rx.try_recv() {
            tx.dequeued(b.len());
        }
        assert!(tx.try_send(Outgoing::Json(one_mib)).is_ok());
        // a single packet larger than the limit is accepted into an empty queue
        let (tx2, _rx2) = OutboxTx::channel(OUTBOX_BYTES);
        let huge = Bytes::from(vec![b'y'; OUTBOX_BYTES + 1]);
        assert!(tx2.try_send(Outgoing::Json(huge.clone())).is_ok());
        assert_eq!(
            tx2.try_send(Outgoing::Json(Bytes::from_static(b"{}"))),
            Err(OutboxFull)
        );
    }

    /// Worst case of the default limits: the send budget (sync answers being sent), the pool,
    /// the relay queues and each app's own queue of control packets stay below 900 MiB.
    #[test]
    fn memory_bound_of_the_default_limits() {
        let cfg = RelayConfig::new(String::new());
        assert_eq!(cfg.max_clients, 200);
        let mib = 1024 * 1024;
        let worst = cfg.queue_budget
            + StoreLimits::default().max_bytes
            + cfg.max_nodes * NODE_OUTBOX_BYTES
            + cfg.max_clients * OUTBOX_BYTES;
        assert!(worst <= 900 * mib, "{}", worst / mib);
        // one `some` answer always fits into the budget
        assert!(SOME_MAX_BYTES + 64 * 1024 <= cfg.queue_budget);
        // admission resumes below the share where it pauses, and pauses before the budget is full
        const { assert!(ADMISSION_RESUME_PERCENT < ADMISSION_PAUSE_PERCENT) };
        const { assert!(ADMISSION_PAUSE_PERCENT < 100) };
    }

    /// Share hashes wait for one of `max_concurrent_hashes` permits.
    #[tokio::test]
    async fn share_hashing_waits_for_a_permit() {
        let relay = relay_with(|c| c.max_concurrent_hashes = 1);
        assert_eq!(relay.inner.hash_permits.available_permits(), 1);
        let held = relay
            .inner
            .hash_permits
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let job = PowJob {
            job_id: "j".into(),
            blob: vec![0; 76],
            target: "ffffffff".into(),
        };
        let waiting = tokio::time::timeout(
            Duration::from_millis(200),
            relay.hash_share(job.clone(), "00".into(), "00".into()),
        )
        .await;
        assert!(waiting.is_err(), "hashed without a permit");
        drop(held);
        // a malformed share fails fast, and only after taking the permit
        assert!(!relay.hash_share(job, "00".into(), "00".into()).await);
        assert_eq!(relay.inner.hash_permits.available_permits(), 1);
        let zero = relay_with(|c| c.max_concurrent_hashes = 0);
        assert_eq!(zero.inner.hash_permits.available_permits(), 1);
    }

    /// Refused requests do not use up the global job window, and a refusal caused
    /// only by the global window gives the client no strike.
    #[tokio::test]
    async fn global_job_window_counts_only_admitted_requests() {
        let relay = relay_with(|_| {});
        let (a, _rx_a) = conn(&relay, Role::Client, 1);
        for i in 0..15 {
            relay.job_request(&a, &json!({ "type": "job_request", "id": i }));
        }
        assert_eq!(
            lock(&relay.inner.state).global_jobs.count,
            CLIENT_JOB_REQUESTS_PER_WINDOW
        );
        lock(&relay.inner.state).global_jobs.count = GLOBAL_JOB_REQUESTS_PER_WINDOW;
        let (b, mut rx_b) = conn(&relay, Role::Client, 2);
        relay.job_request(&b, &json!({ "type": "job_request", "id": 1 }));
        assert_eq!(drain(&mut rx_b)[0]["reason"], "rate_limit");
        assert_eq!(lock(&b.state).rate_strikes, 0);
        assert!(!relay.is_banned(&[2; 32]));
    }

    fn signed_like_message(hash: &str) -> Value {
        json!({
            "hash": hash,
            "timestamp": now_ms(),
            "cipher": "ab",
            "pow": { "version": 2, "shares": [] },
        })
    }

    /// One served sync per remote key per cooldown, across reconnects.
    #[tokio::test]
    async fn xkrd_sync_is_served_once_per_key_per_cooldown() {
        let relay = relay_with(|_| {});
        lock(&relay.inner.pool).insert(signed_like_message(&"ab".repeat(32)), now_ms());
        let request = json!({ "type": "xkrd_sync_request", "id": 5, "since": 0 });

        let (first, mut rx1) = conn(&relay, Role::Node, 7);
        relay.serve_sync(&first, &request);
        let batches = drain(&mut rx1);
        assert_eq!(
            batches.last().unwrap()["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        relay.unregister(&first);

        // the same key on a new connection gets an empty, final batch
        let (again, mut rx2) = conn(&relay, Role::Node, 7);
        relay.serve_sync(&again, &request);
        let batches = drain(&mut rx2);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0]["done"], true);
        assert_eq!(
            batches[0]["messages"].as_array().unwrap(),
            &Vec::<serde_json::Value>::new()
        );

        // a relay-wide cap on distinct keys per cooldown
        for k in 0..MAX_SYNC_SERVES_PER_COOLDOWN {
            let (c, _rx) = conn(&relay, Role::Node, 100 + k as u8);
            relay.serve_sync(&c, &request);
            relay.unregister(&c);
        }
        assert_eq!(
            lock(&relay.inner.state).sync_served.len(),
            MAX_SYNC_SERVES_PER_COOLDOWN
        );
    }

    /// At most `MAX_CONCURRENT_SYNCS_IN` incoming syncs; closing frees the slot.
    #[tokio::test]
    async fn incoming_syncs_are_capped() {
        let relay = relay_with(|_| {});
        let conns: Vec<_> = (0..=MAX_CONCURRENT_SYNCS_IN)
            .map(|k| conn(&relay, Role::Node, 30 + k as u8))
            .collect();
        for (c, _) in &conns[..MAX_CONCURRENT_SYNCS_IN] {
            assert!(relay.take_sync_slot(c));
        }
        let (last, _) = &conns[MAX_CONCURRENT_SYNCS_IN];
        assert!(!relay.take_sync_slot(last));
        relay.unregister(&conns[0].0);
        assert!(relay.take_sync_slot(last));
    }

    const VECTORS: &str = include_str!("../../../docs/test-vectors/hugin-relay.json");

    /// The job of the CN-Turtle vector (target ffffffff) and its one valid share.
    fn vector_job_and_share() -> (Value, Value) {
        let v: Value = serde_json::from_str(VECTORS).unwrap();
        let p = &v["pow"];
        let share =
            json!({ "job_id": p["job"]["job_id"], "nonce": p["nonce"], "result": p["result"] });
        (p["job"].clone(), share)
    }

    /// A message signed for `shares[auth]` with the one-time key from `seed`.
    fn signed_message(
        hash: &str,
        cipher: &str,
        kind: &str,
        shares: &[Value],
        auth: usize,
        seed: [u8; 32],
    ) -> Value {
        let (job, _) = vector_job_and_share();
        let ts = now_ms() as u64;
        let share = &shares[auth];
        let payload =
            pow::pow_sig_payload(hash, &ts.to_string(), share, &format!("{kind}:{cipher}"));
        let (public, sig) = pow::ed25519_sign(&seed, payload.as_bytes());
        json!({
            "cipher": cipher,
            "timestamp": ts,
            "hash": hash,
            "id": ts,
            "push": true,
            "viewtag": "abc",
            "kind": kind,
            "pow": {
                "version": 2,
                "job": job,
                "shares": shares,
                "auth": { "pub": hex::encode(public), "sig": hex::encode(sig), "nonce": share["nonce"] },
            },
        })
    }

    fn seed(i: u32) -> [u8; 32] {
        let mut s = [7u8; 32];
        s[..4].copy_from_slice(&i.to_le_bytes());
        s
    }

    /// A relay with the vector job as its current pool job, and an app difficulty and minimum
    /// gossip difficulty of 1 (the vector share meets difficulty 1 only).
    fn relay_with_job() -> (Relay, Arc<ManualJobSource>) {
        relay_with_pool_job("ffffffff", 1)
    }

    /// A relay whose current pool job is the vector job with the pool target `pool_target`,
    /// with app difficulty `app_difficulty` and a minimum gossip difficulty of 1.
    fn relay_with_pool_job(
        pool_target: &str,
        app_difficulty: u64,
    ) -> (Relay, Arc<ManualJobSource>) {
        let jobs = Arc::new(ManualJobSource::default());
        (relay_on(jobs.clone(), pool_target, app_difficulty), jobs)
    }

    /// [`relay_with_pool_job`] on any job source.
    fn relay_on(jobs: Arc<dyn JobSource>, pool_target: &str, app_difficulty: u64) -> Relay {
        let mut cfg = RelayConfig::new("SEKR".into());
        cfg.min_share_difficulty = 1;
        cfg.app_difficulty = app_difficulty;
        let relay = Relay::new(
            cfg,
            jobs,
            Arc::new(HuginStats::default()),
            CancellationToken::new(),
        )
        .unwrap();
        let (mut job, _) = vector_job_and_share();
        job["target"] = json!(pool_target);
        relay.on_job(&Arc::new(PoolJob::from_value(job).unwrap()));
        relay
    }

    #[test]
    fn ttl_map_reinsert_keeps_one_queue_entry() {
        let now = Instant::now();
        let mut m: TtlMap<String> = TtlMap::new(Duration::from_hours(1), 100);
        assert!(m.insert("k".into(), "a".into(), now));
        for _ in 0..10_000 {
            assert!(!m.insert("k".into(), "b".into(), now));
        }
        assert_eq!(m.len(), 1);
        assert_eq!(m.queued(), 1);
        assert_eq!(m.get("k", now).map(String::as_str), Some("a"));
        for i in 0..1_000 {
            m.insert(format!("x{i}"), String::new(), now);
            m.insert(format!("x{i}"), String::new(), now);
            assert!(m.len() <= 100);
            assert!(m.queued() <= 2 * 100 + 16);
        }
        // an expired key can be inserted again
        let later = now + Duration::from_hours(2);
        assert!(m.insert("k".into(), "c".into(), later));
        assert_eq!(m.get("k", later).map(String::as_str), Some("c"));
        assert_eq!(m.len(), 1);
    }

    /// Posting the same message again with new auth keys is answered as a duplicate: no second
    /// hash or pool submit, no second push notification, one share binding.
    #[tokio::test]
    async fn repost_with_new_auth_keys_is_a_duplicate() {
        let (relay, jobs) = relay_with_job();
        let (node, mut node_rx) = conn(&relay, Role::Node, 50);
        let (client, mut rx) = conn(&relay, Role::Client, 51);
        let (_, share) = vector_job_and_share();
        let hash = "ab".repeat(32);
        let first = signed_message(
            &hash,
            "c0ffee",
            "call",
            std::slice::from_ref(&share),
            0,
            seed(0),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": first }))
            .await;
        let answers = drain(&mut rx);
        assert_eq!(answers.last().unwrap()["success"], true, "{answers:?}");
        assert_eq!(jobs.submitted().len(), 1);
        for i in 1..1_000 {
            lock(&client.state).last_accepted = None;
            let m = signed_message(
                &hash,
                "c0ffee",
                "call",
                std::slice::from_ref(&share),
                0,
                seed(i),
            );
            relay
                .post(&client, &json!({ "type": "post", "message": m }))
                .await;
            let a = drain(&mut rx);
            assert_eq!(a.last().unwrap()["duplicate"], true, "{a:?}");
        }
        assert_eq!(jobs.submitted().len(), 1);
        let st = lock(&relay.inner.state);
        assert_eq!(st.share_bindings.len(), 1);
        assert_eq!(st.share_bindings.queued(), 1);
        drop(st);
        let pushes = drain(&mut node_rx)
            .into_iter()
            .filter(|v| v["push"] == true)
            .count();
        assert_eq!(pushes, 1);
        assert!(!relay.is_banned(&[51; 32]));
        drop(node);
    }

    /// A share bound to another message is answered `share_reused` without a strike; the same
    /// content under another hash is a duplicate.
    #[tokio::test]
    async fn share_reuse_is_answered_without_a_strike() {
        let (relay, jobs) = relay_with_job();
        let (_, share) = vector_job_and_share();
        let (client, mut rx) = conn(&relay, Role::Client, 52);
        let m = signed_message(
            &"11".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(1),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["success"], true);
        for i in 0..3 {
            lock(&client.state).last_accepted = None;
            let other = signed_message(
                &format!("{:064x}", 100 + i),
                "bb",
                "dm",
                std::slice::from_ref(&share),
                0,
                seed(10 + i),
            );
            relay
                .post(&client, &json!({ "type": "post", "message": other }))
                .await;
            assert_eq!(drain(&mut rx).last().unwrap()["reason"], "share_reused");
        }
        assert_eq!(lock(&client.state).pow_strikes, 0);
        assert!(!relay.is_banned(&[52; 32]));
        // same content, other hash and key
        let again = signed_message(
            &"22".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(20),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": again }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["duplicate"], true);
        assert_eq!(jobs.submitted().len(), 1);
        assert_eq!(relay.pool_len(), 1);
    }

    /// Only the share `pow.auth` signs is hashed, submitted, stored and gossiped.
    #[tokio::test]
    async fn only_the_signed_share_is_kept() {
        let (relay, jobs) = relay_with_job();
        let (_node, mut node_rx) = conn(&relay, Role::Node, 53);
        let (client, mut rx) = conn(&relay, Role::Client, 54);
        let (_, good) = vector_job_and_share();
        let bogus =
            json!({ "job_id": good["job_id"], "nonce": "ffffffff", "result": "00".repeat(32) });
        let m = signed_message(
            &"33".repeat(32),
            "aa",
            "dm",
            &[bogus.clone(), good.clone()],
            1,
            seed(2),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["success"], true);
        assert_eq!(jobs.submitted().len(), 1);
        assert_eq!(jobs.submitted()[0].nonce, good["nonce"].as_str().unwrap());
        let gossip: Vec<Value> = drain(&mut node_rx)
            .into_iter()
            .filter(|v| v["signal"] == true)
            .collect();
        assert_eq!(gossip.len(), 1);
        assert_eq!(gossip[0]["message"]["pow"]["shares"], json!([good]));
        // signed for the bogus share: refused
        lock(&client.state).last_accepted = None;
        let bad = signed_message(&"44".repeat(32), "bb", "dm", &[bogus, good], 0, seed(3));
        relay
            .post(&client, &json!({ "type": "post", "message": bad }))
            .await;
        assert_eq!(
            drain(&mut rx).last().unwrap()["reason"],
            "pow_verify_failed"
        );
        assert_eq!(relay.pool_len(), 1);
    }

    /// Synced messages are stored with the known fields only; a message too large in that form
    /// is skipped without a ban.
    #[tokio::test]
    async fn synced_messages_keep_only_known_fields() {
        let (relay, _jobs) = relay_with_job();
        let (node, _rx) = conn(&relay, Role::Node, 55);
        lock(&node.state).sync_id = Some(9);
        let (_, share) = vector_job_and_share();
        let mut padded = signed_message(
            &"55".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(4),
        );
        padded["x"] = json!(vec![0u8; 20_000]);
        padded["pow"]["y"] = json!("z".repeat(10_000));
        padded["pow"]["job"]["extra"] = json!(vec![1u8; 5_000]);
        let big_cipher = "ab".repeat(4_500);
        let big = signed_message(
            &"66".repeat(32),
            &big_cipher,
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(5),
        );
        let batch =
            json!({ "type": "xkrd_sync", "id": 9, "messages": [big, padded], "done": true });
        relay.on_sync_batch(&node, &batch).await;
        assert!(!relay.is_banned(&[55; 32]));
        assert!(relay.has_message(&"55".repeat(32)));
        assert!(!relay.has_message(&"66".repeat(32)));
        let pool = lock(&relay.inner.pool);
        let stored: Value =
            serde_json::from_slice(&pool.get(&"55".repeat(32)).unwrap().full).unwrap();
        assert!(stored.get("x").is_none());
        assert!(stored["pow"].get("y").is_none());
        assert!(stored["pow"]["job"].get("extra").is_none());
        assert!(pool.bytes() < 3 * pow::MAX_MESSAGE_BYTES);
    }

    /// Connections without an accepted message are evicted first, those with one only after
    /// `PROVEN_IDLE_FACTOR` times the idle time. An app without an accepted post or
    /// registration keeps the time it connected, however many syncs and job requests it sends;
    /// a node's served requests count.
    #[tokio::test]
    async fn eviction_counts_paid_activity_of_apps() {
        let relay = relay_with(|c| c.idle_evict_after = Duration::from_millis(20));
        let (quiet, _a) = conn(&relay, Role::Client, 60);
        let (proven_peer, _b) = conn(&relay, Role::Client, 61);
        relay.mark_useful(&proven_peer, true);
        let (reader, _c) = conn(&relay, Role::Client, 62);
        let (node, _d) = conn(&relay, Role::Node, 63);
        tokio::time::sleep(Duration::from_millis(50)).await;
        relay.mark_useful(&reader, false);
        relay.mark_useful(&node, false);
        {
            let mut st = lock(&relay.inner.state);
            assert!(relay.evict_idle(&mut st, Role::Client));
            assert!(relay.evict_idle(&mut st, Role::Client));
            assert!(!st.peers.contains_key(&quiet.id));
            assert!(!st.peers.contains_key(&reader.id));
            assert!(st.peers.contains_key(&proven_peer.id));
            // the proven app is not idle for 6 x 20 ms yet, the node not for 20 ms
            assert!(!relay.evict_idle(&mut st, Role::Client));
            assert!(!relay.evict_idle(&mut st, Role::Node));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        relay.mark_useful(&node, false);
        let mut st = lock(&relay.inner.state);
        assert!(relay.evict_idle(&mut st, Role::Client));
        assert!(!st.peers.contains_key(&proven_peer.id));
        assert!(!relay.evict_idle(&mut st, Role::Node));
        assert!(st.peers.contains_key(&node.id));
    }

    /// A sync larger than a relay connection's queue is sent as the queue drains, in order.
    #[tokio::test]
    async fn large_syncs_wait_for_the_queue() {
        let relay = relay_with(|c| c.store_limits.max_bytes = 64 * 1024 * 1024);
        let cipher = "ab".repeat(4_000);
        let n = 1_000;
        for i in 0..n {
            let mut m = signed_like_message(&format!("{i:064x}"));
            m["cipher"] = json!(cipher);
            lock(&relay.inner.pool).insert(m, now_ms());
        }
        assert_eq!(relay.pool_len(), n);
        let (c, mut rx) = conn(&relay, Role::Node, 11);
        relay.serve_sync(
            &c,
            &json!({ "type": "xkrd_sync_request", "id": 3, "since": 0 }),
        );
        assert!(c.tx.queued_bytes() <= NODE_OUTBOX_BYTES);
        let mut received = 0usize;
        let mut done = false;
        while !done {
            let item = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("the rest of the sync follows")
                .expect("queue open");
            if let Outgoing::Json(b) = item {
                c.tx.dequeued(b.len());
                let v: Value = serde_json::from_slice(&b).unwrap();
                received += v["messages"].as_array().unwrap().len();
                done = v["done"] == true;
            }
        }
        assert_eq!(received, n);
        assert!(!c.stop.is_cancelled());
    }

    /// A node that claimed xkrd sync may send up to 1 MiB only while we receive its
    /// sync; outside that, large packets are dropped unparsed (no ban).
    #[tokio::test]
    async fn large_node_packets_only_during_our_sync() {
        let relay = relay_with(|_| {});
        let (c, _rx) = conn(&relay, Role::Node, 9);
        c.xkrd.store(true, Ordering::Relaxed);
        let big =
            serde_json::to_vec(&json!({ "type": "xkrd_sync", "pad": "x".repeat(40_000) })).unwrap();
        let packet = || RpcMessage::packet(0, big.clone());
        assert!(matches!(read_message(&c, packet()), Read::Ignore));
        lock(&c.state).sync_id = Some(1);
        assert!(matches!(read_message(&c, packet()), Read::Packet(_)));
        let huge = vec![b' '; XKRD_INBOUND_LIMIT + 1];
        assert!(matches!(
            read_message(&c, RpcMessage::packet(0, huge)),
            Read::Ban
        ));
    }

    /// Jobs reach apps with the app target; `job_id` and `blob` stay the pool's.
    #[tokio::test]
    async fn apps_get_the_app_target() {
        let (relay, _jobs) = relay_with_pool_job("37894100", DEFAULT_APP_DIFFICULTY);
        assert_eq!(relay.app_target(), "14ae4701");
        let (client, mut rx) = conn(&relay, Role::Client, 60);
        relay.job_request(&client, &json!({ "type": "job_request", "id": 1 }));
        let answers = drain(&mut rx);
        let job = &answers.last().unwrap()["job"];
        let (vector, _) = vector_job_and_share();
        assert_eq!(job["target"], "14ae4701", "{answers:?}");
        assert_eq!(job["job_id"], vector["job_id"]);
        assert_eq!(job["blob"], vector["blob"]);
        assert_eq!(
            pow::share_difficulty("14ae4701"),
            Some(DEFAULT_APP_DIFFICULTY)
        );
    }

    /// An app share that meets the app target pays for the post; it goes to the pool only when
    /// it also meets the pool job's target.
    #[tokio::test]
    async fn only_shares_that_meet_the_pool_target_are_submitted() {
        let (_, share) = vector_job_and_share();
        // the vector share meets difficulty 1, not the pool's (about 2^32)
        let (relay, jobs) = relay_with_pool_job("01000000", 1);
        let (client, mut rx) = conn(&relay, Role::Client, 61);
        let m = signed_message(
            &"61".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(61),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["success"], true);
        assert!(relay.has_message(&"61".repeat(32)));
        assert!(jobs.submitted().is_empty());
        let s = relay.stats().snapshot();
        assert_eq!(
            (
                s.shares_accepted,
                s.shares_submitted,
                s.shares_rejected_by_pool
            ),
            (1, 0, 0)
        );

        // the same share against a pool target it meets: submitted
        let (relay, jobs) = relay_with_job();
        let (client, mut rx) = conn(&relay, Role::Client, 62);
        let m = signed_message(
            &"62".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(62),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["success"], true);
        assert_eq!(jobs.submitted().len(), 1);
        let s = relay.stats().snapshot();
        assert_eq!(
            (
                s.shares_accepted,
                s.shares_submitted,
                s.shares_rejected_by_pool
            ),
            (1, 1, 0)
        );

        // refused by the pool, no tip feed (the job is not dated): the post is accepted
        let (relay, jobs) = relay_with_job();
        jobs.set_outcome(SubmitOutcome::rejected(
            "pool",
            Some("Low difficulty share".into()),
        ));
        let (client, mut rx) = conn(&relay, Role::Client, 63);
        let m = signed_message(
            &"63".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(63),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(drain(&mut rx).last().unwrap()["success"], true);
        assert!(relay.has_message(&"63".repeat(32)));
        let s = relay.stats().snapshot();
        assert_eq!(
            (
                s.shares_accepted,
                s.shares_submitted,
                s.shares_rejected_by_pool
            ),
            (1, 1, 1)
        );
    }

    #[test]
    fn previous_block_jobs_have_a_grace_period() {
        let t0 = Instant::now();
        let mut seen = None;
        assert_eq!(observe_tip(&mut seen, 101, t0), t0);
        assert_eq!(observe_tip(&mut seen, 101, t0 + Duration::from_secs(5)), t0);
        assert!(job_fresh(101, 101, t0, t0 + Duration::from_secs(60)));
        assert!(job_fresh(101, 100, t0, t0 + Duration::from_secs(15)));
        assert!(!job_fresh(101, 100, t0, t0 + Duration::from_secs(16)));
        assert!(!job_fresh(102, 100, t0, t0));
        assert!(job_fresh(100, 101, t0, t0 + Duration::from_secs(60)));
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(observe_tip(&mut seen, 102, t1), t1);
    }

    #[allow(clippy::unnecessary_wraps)]
    fn tip(height: u64) -> Option<ChainTip> {
        Some(ChainTip {
            height,
            hash: [u8::try_from(height % 256).unwrap(); 32],
        })
    }

    /// A relay on `jobs` whose vector job was dated at main-chain height 100; then the tip is
    /// at `now`. Returns the answers to one post with the vector share.
    async fn post_at_tip(
        jobs: &Arc<ManualJobSource>,
        source: Arc<dyn JobSource>,
        now: u64,
        key: u8,
    ) -> (Relay, Vec<Value>) {
        let (job, share) = vector_job_and_share();
        jobs.set_tip(tip(100));
        // the source's current job as well, so the relay keeps it as the current job
        jobs.set_job(PoolJob::from_value(job).unwrap());
        let relay = relay_on(source, "ffffffff", 1);
        jobs.set_tip(tip(now));
        let (client, mut rx) = conn(&relay, Role::Client, key);
        let hash = format!("{key:02x}").repeat(32);
        let m = signed_message(
            &hash,
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(u32::from(key)),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        assert_eq!(lock(&client.state).pow_strikes, 0);
        (relay, drain(&mut rx))
    }

    /// A job pays for posts while it was issued for the current main-chain tip or the previous
    /// one; two blocks behind, the post is answered `stale_job` and the current job, without a
    /// strike and without a submit.
    #[tokio::test]
    async fn jobs_older_than_one_block_are_stale() {
        for (now, key) in [(100, 70u8), (101, 71)] {
            let jobs = Arc::new(ManualJobSource::default());
            let (relay, answers) = post_at_tip(&jobs, jobs.clone(), now, key).await;
            assert_eq!(answers.last().unwrap()["success"], true, "{answers:?}");
            assert!(relay.has_message(&format!("{key:02x}").repeat(32)));
            assert_eq!(jobs.submitted().len(), 1);
        }
        let jobs = Arc::new(ManualJobSource::default());
        let (relay, answers) = post_at_tip(&jobs, jobs.clone(), 102, 72).await;
        assert_eq!(answers[0]["reason"], "stale_job", "{answers:?}");
        assert!(answers[1].get("job").is_some(), "{answers:?}");
        assert!(!relay.has_message(&"48".repeat(32)));
        assert!(jobs.submitted().is_empty());
        assert_eq!(relay.stats().snapshot().shares_accepted, 0);
    }

    /// A share the pool refuses still pays for the post while the job is at most one block
    /// old (counted in `shares_rejected_by_pool`).
    #[tokio::test]
    async fn pool_refusals_within_one_block_pay_for_the_post() {
        let jobs = Arc::new(ManualJobSource::default());
        jobs.set_outcome(SubmitOutcome::rejected("stale", None));
        let (relay, answers) = post_at_tip(&jobs, jobs.clone(), 101, 73).await;
        assert_eq!(answers.last().unwrap()["success"], true, "{answers:?}");
        assert!(relay.has_message(&"49".repeat(32)));
        let s = relay.stats().snapshot();
        assert_eq!(
            (
                s.shares_accepted,
                s.shares_submitted,
                s.shares_rejected_by_pool
            ),
            (1, 1, 1)
        );

        // two blocks behind: refused before anything reaches the pool
        let jobs = Arc::new(ManualJobSource::default());
        jobs.set_outcome(SubmitOutcome::rejected("stale", None));
        let (relay, answers) = post_at_tip(&jobs, jobs.clone(), 102, 74).await;
        assert_eq!(answers[0]["reason"], "stale_job", "{answers:?}");
        assert!(!relay.has_message(&"4a".repeat(32)));
        assert!(jobs.submitted().is_empty());
    }

    /// A job source whose tip moves two blocks on while the pool refuses the share.
    #[derive(Debug)]
    struct TipMovesOnSubmit(Arc<ManualJobSource>);

    impl JobSource for TipMovesOnSubmit {
        fn jobs(&self) -> tokio::sync::watch::Receiver<Option<Arc<PoolJob>>> {
            self.0.jobs()
        }

        fn submit(&self, share: ShareSubmit) -> futures::future::BoxFuture<'static, SubmitOutcome> {
            let now = self.0.tip().map_or(0, |t| t.height);
            self.0.set_tip(tip(now + 2));
            self.0.submit(share)
        }

        fn is_ready(&self) -> bool {
            true
        }

        fn tip(&self) -> Option<ChainTip> {
            self.0.tip()
        }
    }

    /// The pool refuses the share and the job is two blocks old by then: `stale_job`, the
    /// share counted as refused, the post not stored.
    #[tokio::test]
    async fn pool_refusals_outside_the_window_are_stale() {
        let jobs = Arc::new(ManualJobSource::default());
        jobs.set_outcome(SubmitOutcome::rejected("stale", None));
        let source = Arc::new(TipMovesOnSubmit(jobs.clone()));
        let (relay, answers) = post_at_tip(&jobs, source, 100, 75).await;
        assert_eq!(answers[0]["reason"], "stale_job", "{answers:?}");
        assert!(answers[1].get("job").is_some(), "{answers:?}");
        assert!(!relay.has_message(&"4b".repeat(32)));
        assert_eq!(jobs.submitted().len(), 1);
        let s = relay.stats().snapshot();
        assert_eq!(
            (
                s.shares_accepted,
                s.shares_submitted,
                s.shares_rejected_by_pool
            ),
            (0, 1, 1)
        );
        // the share is free again
        assert_eq!(lock(&relay.inner.state).share_bindings.len(), 0);
    }

    /// A post whose job target is easier than the app target is answered `stale_job` with the
    /// current job (no strike, nothing hashed or submitted).
    #[tokio::test]
    async fn jobs_easier_than_the_app_target_are_refused() {
        let (_, share) = vector_job_and_share();
        let (relay, jobs) = relay_with_pool_job("ffffffff", DEFAULT_APP_DIFFICULTY);
        let (client, mut rx) = conn(&relay, Role::Client, 64);
        // the message carries the vector job with target ffffffff (difficulty 1)
        let m = signed_message(
            &"64".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(64),
        );
        relay
            .post(&client, &json!({ "type": "post", "message": m }))
            .await;
        let answers = drain(&mut rx);
        assert_eq!(answers[0]["reason"], "stale_job", "{answers:?}");
        assert_eq!(answers[1]["job"]["target"], "14ae4701", "{answers:?}");
        assert_eq!(lock(&client.state).pow_strikes, 0);
        assert!(jobs.submitted().is_empty());
        assert!(!relay.has_message(&"64".repeat(32)));
    }

    /// Gossip floor: the configured one (the same default on every relay), whatever our own
    /// app difficulty.
    #[tokio::test]
    async fn gossip_floor_is_the_same_for_every_app_difficulty() {
        assert_eq!(GOSSIP_MIN_DIFFICULTY, 200);
        assert_eq!(
            GOSSIP_MIN_DIFFICULTY,
            MIN_SHARE_DIFFICULTY.max(DEFAULT_APP_DIFFICULTY)
        );
        assert_eq!(relay_with(|_| {}).gossip_floor(), GOSSIP_MIN_DIFFICULTY);
        for app in [0, 20, 5000] {
            assert_eq!(
                relay_with(|c| c.app_difficulty = app).gossip_floor(),
                GOSSIP_MIN_DIFFICULTY
            );
        }
        assert_eq!(relay_with(|c| c.min_share_difficulty = 0).gossip_floor(), 1);
    }

    /// At most `max_clients_per_ip` app connections per address (IPv6: per /64); other
    /// addresses, unknown addresses and node connections are not limited by it; 0 turns it off.
    #[tokio::test]
    async fn app_connections_are_capped_per_address() {
        let relay = relay_with(|c| c.max_clients = 64);
        assert_eq!(
            relay.config().max_clients_per_ip,
            DEFAULT_MAX_CLIENTS_PER_IP
        );
        let mut held = Vec::new();
        for k in 0..4 {
            held.push(
                conn_from(&relay, Role::Client, k, Some("203.0.113.7")).expect("under the cap"),
            );
        }
        assert!(conn_from(&relay, Role::Client, 4, Some("203.0.113.7")).is_none());
        // the same address as IPv4-mapped IPv6
        assert!(conn_from(&relay, Role::Client, 5, Some("::ffff:203.0.113.7")).is_none());
        held.push(conn_from(&relay, Role::Client, 6, Some("203.0.113.8")).expect("other address"));
        // IPv6: one /64 is one address
        for k in 40..44 {
            let ip = format!("2001:db8::{k}");
            held.push(conn_from(&relay, Role::Client, k, Some(&ip)).expect("IPv6"));
        }
        assert!(conn_from(&relay, Role::Client, 44, Some("2001:db8::ffff:1")).is_none());
        held.push(conn_from(&relay, Role::Client, 7, Some("2001:db8:0:1::1")).expect("other /64"));
        for k in 10..20 {
            held.push(conn_from(&relay, Role::Client, k, None).expect("unknown address"));
        }
        held.push(conn_from(&relay, Role::Node, 30, Some("203.0.113.7")).expect("node"));
        assert_eq!(relay.stats().snapshot().clients, 20);
        // one goes away: room for one more from that address
        let (gone, _) = held.remove(0);
        relay.unregister(&gone);
        held.push(conn_from(&relay, Role::Client, 8, Some("203.0.113.7")).expect("room again"));
        assert!(conn_from(&relay, Role::Client, 9, Some("203.0.113.7")).is_none());

        // the global cap still counts connections without an address
        let relay = relay_with(|c| c.max_clients = 2);
        let _a = conn_from(&relay, Role::Client, 1, None).unwrap();
        let _b = conn_from(&relay, Role::Client, 2, None).unwrap();
        assert!(conn_from(&relay, Role::Client, 3, None).is_none());

        let relay = relay_with(|c| c.max_clients_per_ip = 0);
        let all: Vec<_> = (0..8)
            .map(|k| conn_from(&relay, Role::Client, k, Some("203.0.113.7")).expect("no cap"))
            .collect();
        assert_eq!(all.len(), 8);
    }

    /// A share of the vector job for `nonce` (every hash meets its target ffffffff).
    fn mined_share(nonce: u32) -> Value {
        let (job, _) = vector_job_and_share();
        let blob = hex::decode(job["blob"].as_str().unwrap()).unwrap();
        let hashing = pow::insert_nonce(&blob, &nonce.to_be_bytes()).unwrap();
        let result = hex::encode(xkr_crypto::cn_turtle_lite_slow_hash_v2(&hashing).0);
        json!({ "job_id": job["job_id"], "nonce": format!("{nonce:08x}"), "result": result })
    }

    /// A share of the vector job whose result is wrong.
    fn bogus_share(nonce: u32) -> Value {
        let (job, _) = vector_job_and_share();
        let result = "00".repeat(32);
        json!({ "job_id": job["job_id"], "nonce": format!("{nonce:08x}"), "result": result })
    }

    /// The binding key of `share` on the vector job.
    fn share_key(share: &Value) -> String {
        let (job, _) = vector_job_and_share();
        validate_share_fast(share, job["job_id"].as_str().unwrap())
            .unwrap()
            .key()
    }

    /// Gossip: only the share `pow.auth` signs is hashed, and the message is stored with that
    /// share alone. The same share for other messages is dropped without a hash and without a
    /// ban; a signed share that does not hash to its result bans the node, whatever the other
    /// shares, and is not left bound.
    #[tokio::test]
    async fn gossip_pays_with_the_signed_share() {
        let (relay, _jobs) = relay_with_job();
        let (node, _rx) = conn(&relay, Role::Node, 80);
        let good = mined_share(500);
        let mut shares: Vec<Value> = (0..4).map(|i| bogus_share(600 + i)).collect();
        shares.push(good.clone());
        let m = signed_message(&"81".repeat(32), "aa", "dm", &shares, 4, seed(80));
        relay
            .on_signal(&node, &json!({ "signal": true, "message": m }))
            .await;
        assert!(relay.has_message(&"81".repeat(32)));
        assert_eq!(relay.inner.hashed.load(Ordering::Relaxed), 1);
        let stored: Value =
            serde_json::from_slice(&lock(&relay.inner.pool).get(&"81".repeat(32)).unwrap().full)
                .unwrap();
        assert_eq!(stored["pow"]["shares"], json!([good]));

        for i in 0..50u32 {
            let mut shares = vec![good.clone()];
            shares.extend((0..4).map(|k| bogus_share(700 + i * 4 + k)));
            let m = signed_message(
                &format!("{:064x}", 0x900 + i),
                &format!("{:04x}", 0xbb00 + i),
                "dm",
                &shares,
                0,
                seed(100 + i),
            );
            relay
                .on_signal(&node, &json!({ "signal": true, "message": m }))
                .await;
        }
        assert_eq!(relay.inner.hashed.load(Ordering::Relaxed), 1);
        assert!(!relay.is_banned(&[80; 32]));
        assert_eq!(relay.pool_len(), 1);

        let (other, _rx2) = conn(&relay, Role::Node, 81);
        let wrong = bogus_share(800);
        let m = signed_message(
            &"82".repeat(32),
            "cc",
            "dm",
            &[mined_share(501), wrong.clone()],
            1,
            seed(81),
        );
        relay
            .on_signal(&other, &json!({ "signal": true, "message": m }))
            .await;
        assert!(relay.is_banned(&[81; 32]));
        assert!(!relay.has_message(&"82".repeat(32)));
        assert_eq!(relay.inner.hashed.load(Ordering::Relaxed), 2);
        let st = lock(&relay.inner.state);
        assert!(
            st.share_bindings
                .get(&share_key(&wrong), Instant::now())
                .is_none()
        );
        assert_eq!(st.share_bindings.len(), 1);
    }

    /// Eight apps post eight messages paid with one share at the same time: one is accepted,
    /// the others are answered `share_reused`; the share is bound once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_share_one_post_under_concurrency() {
        let (relay, jobs) = relay_with_job();
        let share = mined_share(510);
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for i in 0..8u8 {
            let (relay, share, barrier) = (relay.clone(), share.clone(), barrier.clone());
            tasks.push(tokio::spawn(async move {
                let (client, mut rx) = conn(&relay, Role::Client, 90 + i);
                let m = signed_message(
                    &format!("{:064x}", 0xa00 + u32::from(i)),
                    &format!("{:02x}", 0x10 + i),
                    "dm",
                    std::slice::from_ref(&share),
                    0,
                    seed(200 + u32::from(i)),
                );
                let packet = json!({ "type": "post", "message": m });
                barrier.wait().await;
                relay.post(&client, &packet).await;
                drain(&mut rx)
                    .into_iter()
                    .rfind(|v| v.get("success").is_some())
                    .unwrap()
            }));
        }
        let mut answers = Vec::new();
        for t in tasks {
            answers.push(t.await.unwrap());
        }
        let accepted = answers.iter().filter(|a| a["success"] == true).count();
        let reused = answers
            .iter()
            .filter(|a| a["reason"] == "share_reused")
            .count();
        assert_eq!((accepted, reused), (1, 7), "{answers:?}");
        assert_eq!(relay.pool_len(), 1);
        assert_eq!(jobs.submitted().len(), 1);
        assert_eq!(lock(&relay.inner.state).share_bindings.len(), 1);
    }

    /// One connection sends eight posts with eight shares at once: one is checked and
    /// accepted, the others are answered `cooldown`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn posts_of_one_connection_take_turns() {
        let (relay, jobs) = relay_with_job();
        let (client, mut rx) = conn(&relay, Role::Client, 98);
        let posts: Vec<Value> = (0..8u32)
            .map(|i| {
                let m = signed_message(
                    &format!("{:064x}", 0xb00 + i),
                    &format!("{:02x}", 0x20 + i),
                    "dm",
                    &[mined_share(520 + i)],
                    0,
                    seed(300 + i),
                );
                json!({ "type": "post", "message": m })
            })
            .collect();
        futures::future::join_all(posts.iter().map(|p| relay.post(&client, p))).await;
        let answers: Vec<Value> = drain(&mut rx)
            .into_iter()
            .filter(|v| v.get("success").is_some())
            .collect();
        assert_eq!(answers.len(), 8, "{answers:?}");
        let accepted = answers.iter().filter(|a| a["success"] == true).count();
        let cooling = answers.iter().filter(|a| a["reason"] == "cooldown").count();
        assert_eq!((accepted, cooling), (1, 7), "{answers:?}");
        assert_eq!(jobs.submitted().len(), 1);
        assert!(!lock(&client.state).post_in_flight);
    }

    /// The gossip bucket: `GOSSIP_BURST` at once, then `GOSSIP_PER_SECOND`, never more than the
    /// burst.
    #[test]
    fn gossip_bucket_refills() {
        let t0 = Instant::now();
        let mut b = Bucket::default();
        for _ in 0..GOSSIP_BURST {
            assert!(b.take(t0, GOSSIP_BURST, GOSSIP_PER_SECOND));
        }
        assert!(!b.take(t0, GOSSIP_BURST, GOSSIP_PER_SECOND));
        let t1 = t0 + Duration::from_secs(1);
        let refilled = (0..2 * GOSSIP_PER_SECOND)
            .filter(|_| b.take(t1, GOSSIP_BURST, GOSSIP_PER_SECOND))
            .count();
        assert_eq!(refilled, usize::try_from(GOSSIP_PER_SECOND).unwrap());
        let t2 = t0 + Duration::from_hours(1);
        let full = (0..2 * GOSSIP_BURST)
            .filter(|_| b.take(t2, GOSSIP_BURST, GOSSIP_PER_SECOND))
            .count();
        assert_eq!(full, usize::try_from(GOSSIP_BURST).unwrap());
    }

    /// A node connection that sends more than `GOSSIP_BURST` signals at once has the rest
    /// dropped unread, without a ban; another connection is not affected.
    #[tokio::test]
    async fn gossip_rate_per_connection() {
        let (relay, _jobs) = relay_with_job();
        let (busy, _rx) = conn(&relay, Role::Node, 85);
        let filler = json!({ "signal": true, "message": { "hash": "00" } });
        for _ in 0..GOSSIP_BURST + 100 {
            relay.on_signal(&busy, &filler).await;
        }
        let (_, share) = vector_job_and_share();
        let m = signed_message(
            &"86".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(86),
        );
        let packet = json!({ "signal": true, "message": m });
        relay.on_signal(&busy, &packet).await;
        assert!(!relay.has_message(&"86".repeat(32)));
        assert_eq!(relay.stats().snapshot().gossip_received, 0);
        assert!(!relay.is_banned(&[85; 32]));
        let (calm, _rx2) = conn(&relay, Role::Node, 87);
        relay.on_signal(&calm, &packet).await;
        assert!(relay.has_message(&"86".repeat(32)));
    }

    /// A stored gossiped or synced message is re-gossiped only while its timestamp is within
    /// `MAX_MESSAGE_PAST_MS`.
    #[tokio::test]
    async fn only_recent_messages_are_regossiped() {
        let (relay, _jobs) = relay_with_job();
        let (_peer, mut peer_rx) = conn(&relay, Role::Node, 88);
        let (_, share) = vector_job_and_share();
        let recent = signed_message(
            &"8b".repeat(32),
            "aa",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(88),
        );
        let mut old = signed_message(
            &"8a".repeat(32),
            "ab",
            "dm",
            std::slice::from_ref(&share),
            0,
            seed(89),
        );
        old["timestamp"] = json!(now_ms() as u64 - 2 * 60 * 60 * 1000);
        assert!(relay.store_gossip(&old, None, None, true));
        assert!(relay.store_gossip(&recent, None, None, true));
        let signals: Vec<Value> = drain(&mut peer_rx)
            .into_iter()
            .filter(|v| v["signal"] == true)
            .collect();
        assert_eq!(signals.len(), 1, "{signals:?}");
        assert_eq!(signals[0]["message"]["hash"], "8b".repeat(32));
    }

    /// IPv6 apps are limited per /64, per /56 and per /48; a higher per-address limit raises
    /// the wider ones. Nodes are limited per IPv4 address or IPv6 /64.
    #[tokio::test]
    async fn address_caps_per_prefix() {
        let relay = relay_with(|c| {
            c.max_clients = 64;
            c.max_nodes = 64;
        });
        let mut held = Vec::new();
        let mut key = 0u8;
        let mut add = |role: Role, ip: &str| {
            key += 1;
            conn_from(&relay, role, key, Some(ip))
        };
        // one /56: eight apps in four /64s fill it
        for net in 0..4 {
            for host in 1..=2 {
                let ip = format!("2001:db8:0:1{net:02x}::{host}");
                held.push(add(Role::Client, &ip).expect("under the caps"));
            }
        }
        assert!(add(Role::Client, "2001:db8:0:1aa::1").is_none());
        // another /56 of the same /48: eight more fill the /48
        for net in 0..8 {
            let ip = format!("2001:db8:0:2{net:02x}::1");
            held.push(add(Role::Client, &ip).expect("under the caps"));
        }
        assert!(add(Role::Client, "2001:db8:0:300::1").is_none());
        held.push(add(Role::Client, "2001:db8:1::1").expect("another /48"));
        // nodes: four per IPv4 address, four per /64
        for _ in 0..MAX_NODES_PER_ADDRESS {
            held.push(add(Role::Node, "198.51.100.1").expect("node"));
        }
        assert!(add(Role::Node, "198.51.100.1").is_none());
        assert!(add(Role::Node, "::ffff:198.51.100.1").is_none());
        held.push(add(Role::Node, "198.51.100.2").expect("other address"));
        for host in 1..=MAX_NODES_PER_ADDRESS {
            held.push(add(Role::Node, &format!("2001:db8:5::{host}")).expect("node"));
        }
        assert!(add(Role::Node, "2001:db8:5::ff").is_none());
        held.push(add(Role::Node, "2001:db8:5:1::1").expect("other /64"));
        held.push(
            relay
                .register(PeerMeta {
                    role: Role::Node,
                    remote_key: [0xfe; 32],
                    remote_ip: None,
                })
                .expect("unknown address"),
        );

        // a per-address limit of 10 raises the /56 limit to 10
        let relay = relay_with(|c| {
            c.max_clients = 64;
            c.max_clients_per_ip = 10;
        });
        let wide: Vec<_> = (0..10)
            .map(|k| conn_from(&relay, Role::Client, k, Some(&format!("2001:db8::{k}"))))
            .collect();
        assert!(wide.iter().all(Option::is_some));
        assert!(conn_from(&relay, Role::Client, 10, Some("2001:db8:0:1::1")).is_none());

        // 0: no app limits per address
        let relay = relay_with(|c| c.max_clients_per_ip = 0);
        let all: Vec<_> = (0..20)
            .map(|k| conn_from(&relay, Role::Client, k, Some(&format!("2001:db8::{k}"))))
            .collect();
        assert!(all.iter().all(Option::is_some));
    }

    /// Waits up to 5 s for `f`.
    async fn wait_for(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The sync answer waiting in `c`'s sync lane, if any.
    fn sync_frame(c: &Conn) -> Option<SyncFrame> {
        let mut rx = lock(&c.sync_rx);
        rx.as_mut().and_then(|r| r.try_recv().ok())
    }

    /// Waits up to 5 s for `c`'s next sync answer.
    async fn take_frame(c: &Conn) -> SyncFrame {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = sync_frame(c) {
                return frame;
            }
            assert!(Instant::now() < deadline, "no sync answer");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// `c`'s `some` request, handled in a task as `serve` does.
    fn request_some(relay: &Relay, c: &Arc<Conn>, id: u32) -> tokio::task::JoinHandle<()> {
        request_since(relay, c, id, 0.0)
    }

    /// [`request_some`] for the messages after `since`.
    fn request_since(
        relay: &Relay,
        c: &Arc<Conn>,
        id: u32,
        since: f64,
    ) -> tokio::task::JoinHandle<()> {
        let relay = relay.clone();
        let c = c.clone();
        let request = json!({ "request": true, "type": "some", "timestamp": since, "id": id });
        tokio::spawn(async move { relay.client_request(&c, &request).await })
    }

    /// The hashes of the messages in a sync answer.
    fn answer_hashes(frame: &SyncFrame) -> Vec<String> {
        let v: Value = serde_json::from_slice(&frame.body).unwrap();
        v["response"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["hash"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// Message `i` of a test pool: timestamp `base + i`, a cipher of `cipher_len` characters.
    fn put_message(relay: &Relay, i: u32, base: f64, cipher_len: usize) {
        let mut m = signed_like_message(&format!("{i:064x}"));
        m["timestamp"] = json!(base + f64::from(i));
        m["cipher"] = json!("a".repeat(cipher_len));
        assert_eq!(lock(&relay.inner.pool).insert(m, now_ms()), Insert::Added);
    }

    /// Sync answers take the slots in arrival order; a request whose connection closed while it
    /// waited is skipped, and a slot comes back when its answer is written or dropped.
    #[tokio::test]
    async fn app_syncs_take_slots_in_arrival_order() {
        let relay = relay_with(|c| c.sync_slots = 1);
        lock(&relay.inner.pool).insert(signed_like_message(&"ab".repeat(32)), now_ms());
        let stats = relay.stats();
        let (a, _rx_a) = conn(&relay, Role::Client, 1);
        let (b, _rx_b) = conn(&relay, Role::Client, 2);
        let (gone, _rx_gone) = conn(&relay, Role::Client, 3);
        let (c, _rx_c) = conn(&relay, Role::Client, 4);
        let _task_a = request_some(&relay, &a, 1);
        let first = take_frame(&a).await;
        assert_eq!(stats.snapshot().syncs_running, 1);
        let _task_b = request_some(&relay, &b, 2);
        wait_for("b waits", || stats.snapshot().syncs_queued == 1).await;
        let closing = request_some(&relay, &gone, 3);
        wait_for("gone waits", || stats.snapshot().syncs_queued == 2).await;
        let _task_c = request_some(&relay, &c, 4);
        wait_for("c waits", || stats.snapshot().syncs_queued == 3).await;
        // the second waiting request's connection closes, and its task with it
        closing.abort();
        let _ = closing.await;
        relay.refresh_admission();
        assert_eq!(stats.snapshot().syncs_queued, 2);
        // each answer holds its slot until it is written (dropped here)
        drop(first);
        let second = take_frame(&b).await;
        assert!(sync_frame(&c).is_none());
        assert_eq!(stats.snapshot().syncs_queued, 1);
        drop(second);
        let third = take_frame(&c).await;
        let answer: Value = serde_json::from_slice(&third.body).unwrap();
        assert_eq!(answer["id"], 4);
        assert_eq!(answer["response"].as_array().unwrap().len(), 1);
        assert!(sync_frame(&gone).is_none());
        drop(third);
        let s = stats.snapshot();
        assert_eq!((s.syncs_running, s.syncs_queued), (0, 0));
        assert_eq!(relay.inner.sync_reserved.load(Ordering::Acquire), 0);
    }

    /// While a connection's sync request waits for its previous answer, a newer request of the
    /// same connection takes its place.
    #[tokio::test]
    async fn a_newer_sync_request_replaces_a_waiting_one() {
        let relay = relay_with(|c| c.sync_slots = 1);
        let held = relay.app_sync_slot(false).await;
        let (b, _rx_b) = conn(&relay, Role::Client, 2);
        let _first = request_some(&relay, &b, 1);
        wait_for("the first waits for a slot", || {
            relay.stats().snapshot().syncs_queued == 1
        })
        .await;
        let second = request_some(&relay, &b, 2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _third = request_some(&relay, &b, 3);
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the second request ends")
            .unwrap();
        drop(held);
        let answer = take_frame(&b).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&answer.body).unwrap()["id"],
            1
        );
        drop(answer);
        let answer = take_frame(&b).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&answer.body).unwrap()["id"],
            3
        );
        drop(answer);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(sync_frame(&b).is_none());
    }

    /// The sync allowance: a larger answer than what is left keeps its newest messages that
    /// fit, a used-up allowance answers only the messages after the newest one sent, and the
    /// allowance refills over time.
    #[test]
    fn sync_allowance_cuts_a_large_answer_and_refills() {
        const MIB: u64 = 1024 * 1024;
        let answer = |n: u32, from: f64| -> Vec<(f64, Bytes)> {
            (0..n)
                .map(|i| (from + f64::from(i), Bytes::from(vec![b'x'; 1024 * 1024])))
                .collect()
        };
        let t0 = Instant::now();
        let mut a = SyncAllowance::default();
        let mut point = SyncPoint::default();
        assert_eq!(a.left_at(t0), SYNC_ALLOWANCE_BYTES);
        // three answers of 4 MiB fit
        for k in 0..3u32 {
            let mut list = answer(4, f64::from(k * 4 + 1));
            assert!(!a.charge(t0, &mut list, false));
            assert_eq!(list.len(), 4);
            point.record(t0, &list);
        }
        assert_eq!(a.left_at(t0), 4 * MIB);
        assert!(!a.used_up_at(t0));
        // 6 MiB with 4 MiB left: the 4 newest messages stay
        let mut list = answer(6, 13.0);
        assert!(a.charge(t0, &mut list, false));
        point.record(t0, &list);
        let kept: Vec<f64> = list.iter().map(|(ts, _)| *ts).collect();
        assert_eq!(kept, [15.0, 16.0, 17.0, 18.0]);
        assert!(a.used_up_at(t0));
        assert!(a.is_used_up(t0));
        assert_eq!(a.full_in(), Some(SYNC_ALLOWANCE_WINDOW));
        // used up: only the messages after the newest one sent, also after the repeat window;
        // those answers stay whole
        let t1 = t0 + SYNC_ALLOWANCE_WINDOW / 2;
        assert_eq!(a.left_at(t1), 8 * MIB);
        let used_up = a.used_up_at(t1);
        assert!(used_up);
        assert_eq!(point.since(t1, 0.0, used_up), (18.0, true));
        assert_eq!(point.since(t1, 20.0, used_up), (20.0, false));
        let mut newer = answer(2, 19.0);
        assert!(!a.charge(t1, &mut newer, true));
        assert_eq!(newer.len(), 2);
        assert_eq!(a.left_at(t1), 6 * MIB);
        // full again: the whole range again
        let t2 = t1 + SYNC_ALLOWANCE_WINDOW;
        assert!(!a.used_up_at(t2));
        assert!(!a.is_used_up(t2));
        assert_eq!(point.since(t2, 0.0, false), (0.0, false));
        assert_eq!(a.left_at(t2), SYNC_ALLOWANCE_BYTES);
        assert_eq!(a.full_in(), None);
        // a single message larger than what is left still goes out, and uses the allowance up
        let mut b = SyncAllowance {
            left: MIB / 2,
            at: Some(t0),
            ..SyncAllowance::default()
        };
        let mut one = answer(1, 1.0);
        assert!(!b.charge(t0, &mut one, false));
        assert_eq!(one.len(), 1);
        assert!(b.used_up_at(t0));
    }

    /// A `some` request within the repeat window gets only the messages after the newest one
    /// the connection was already sent; after the window, the whole range again.
    #[tokio::test]
    async fn repeat_sync_requests_get_only_newer_messages() {
        let relay = relay_with(|_| {});
        let base = (now_ms() - 60_000.0).floor();
        for i in 1..=3 {
            put_message(&relay, i, base, 2);
        }
        let (a, _rx) = conn(&relay, Role::Client, 1);
        let _t = request_some(&relay, &a, 1);
        let frame = take_frame(&a).await;
        assert_eq!(answer_hashes(&frame).len(), 3);
        drop(frame);
        put_message(&relay, 4, base, 2);
        let _t = request_some(&relay, &a, 2);
        let frame = take_frame(&a).await;
        assert_eq!(answer_hashes(&frame), [format!("{:064x}", 4)]);
        drop(frame);
        put_message(&relay, 5, base, 2);
        let _t = request_since(&relay, &a, 3, base + 1.0);
        let frame = take_frame(&a).await;
        assert_eq!(answer_hashes(&frame), [format!("{:064x}", 5)]);
        drop(frame);
        // the previous answer is older than the repeat window
        lock(&a.state).sync_point.answered =
            Instant::now().checked_sub(SYNC_REPEAT_WINDOW + Duration::from_secs(1));
        let _t = request_some(&relay, &a, 4);
        let frame = take_frame(&a).await;
        assert_eq!(answer_hashes(&frame).len(), 5);
        drop(frame);
        assert_eq!(relay.stats().snapshot().syncs_limited, 0);
    }

    /// Requests of connections whose sync allowance is used up wait for a slot behind all
    /// others, and those connections are counted as on allowance.
    #[tokio::test]
    async fn used_up_allowances_wait_behind_others_for_a_slot() {
        let relay = relay_with(|c| c.sync_slots = 1);
        put_message(&relay, 1, (now_ms() - 60_000.0).floor(), 2);
        let stats = relay.stats();
        let held = relay.app_sync_slot(false).await;
        let (a, _rx_a) = conn(&relay, Role::Client, 1);
        let (b, _rx_b) = conn(&relay, Role::Client, 2);
        lock(&a.state).sync_allowance = SyncAllowance {
            left: 0,
            at: Some(Instant::now()),
            used_up: true,
        };
        let _task_a = request_some(&relay, &a, 1);
        wait_for("a waits", || stats.snapshot().syncs_queued == 1).await;
        let _task_b = request_some(&relay, &b, 2);
        wait_for("b waits", || stats.snapshot().syncs_queued == 2).await;
        drop(held);
        let first = take_frame(&b).await;
        assert!(sync_frame(&a).is_none(), "a waits behind b");
        drop(first);
        let second = take_frame(&a).await;
        assert_eq!(answer_hashes(&second).len(), 1);
        drop(second);
        relay.refresh_admission();
        let s = stats.snapshot();
        assert_eq!((s.syncs_running, s.syncs_queued), (0, 0));
        assert_eq!(s.apps_on_allowance, 1);
        assert_eq!(s.syncs_limited, 0);
    }

    /// An app that syncs the whole pool once and then only new messages is never limited by
    /// its sync allowance.
    #[tokio::test]
    async fn a_full_sync_then_new_messages_stays_within_the_allowance() {
        let relay = relay_with(|_| {});
        let base = (now_ms() - 600_000.0).floor();
        for i in 0..520 {
            put_message(&relay, i, base, 8_000);
        }
        let full = lock(&relay.inner.pool)
            .since_capped(0.0, MAX_SYNC_MESSAGES, SOME_MAX_BYTES)
            .len();
        let (a, _rx) = conn(&relay, Role::Client, 1);
        let _t = request_some(&relay, &a, 1);
        let frame = take_frame(&a).await;
        assert_eq!(answer_hashes(&frame).len(), full);
        drop(frame);
        for i in 520..524u32 {
            put_message(&relay, i, base, 8_000);
            let newest = base + f64::from(i) - 1.0;
            let _t = request_since(&relay, &a, i, newest);
            let frame = take_frame(&a).await;
            assert_eq!(answer_hashes(&frame), [format!("{i:064x}")]);
            drop(frame);
        }
        relay.refresh_admission();
        let s = relay.stats().snapshot();
        assert_eq!((s.syncs_limited, s.apps_on_allowance), (0, 0));
        assert!(!lock(&a.state).sync_allowance.used_up_at(Instant::now()));
    }

    /// The app connections of one address (an IPv4 address, an IPv6 /64) share a sync
    /// allowance: a reconnect keeps a used-up one, another address has its own, and an entry
    /// that is full again without a connection is dropped.
    #[tokio::test]
    async fn the_sync_allowance_belongs_to_the_address() {
        let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
        assert_eq!(
            allowance_key(ip("2001:db8::1")),
            allowance_key(ip("2001:db8::2:3"))
        );
        assert_ne!(
            allowance_key(ip("2001:db8::1")),
            allowance_key(ip("2001:db8:0:1::1"))
        );
        assert_eq!(allowance_key(ip("::ffff:10.0.0.1")), ip("10.0.0.1"));

        let relay = relay_with(|_| {});
        let base = (now_ms() - 60_000.0).floor();
        for i in 1..=6 {
            put_message(&relay, i, base, 8_000);
        }
        let used_up = SyncAllowance {
            left: 0,
            at: Some(Instant::now()),
            used_up: true,
        };
        lock(&relay.inner.allowances).insert(ip("10.0.0.1"), used_up);
        let (first, _rx1) = conn_from(&relay, Role::Client, 1, Some("10.0.0.1")).unwrap();
        let _t = request_some(&relay, &first, 1);
        let frame = take_frame(&first).await;
        assert!(answer_hashes(&frame).len() < 6, "cut to what is left");
        drop(frame);
        relay.unregister(&first);
        drop(first);
        // a new connection from the same address: still used up
        let (again, _rx2) = conn_from(&relay, Role::Client, 2, Some("10.0.0.1")).unwrap();
        let _t = request_some(&relay, &again, 2);
        let frame = take_frame(&again).await;
        assert!(
            answer_hashes(&frame).len() < 6,
            "the address keeps its allowance"
        );
        drop(frame);
        // another address: its own, full allowance
        let (other, _rx3) = conn_from(&relay, Role::Client, 3, Some("10.0.0.2")).unwrap();
        let _t = request_some(&relay, &other, 3);
        let frame = take_frame(&other).await;
        assert_eq!(answer_hashes(&frame).len(), 6);
        drop(frame);
        relay.refresh_admission();
        let s = relay.stats().snapshot();
        assert_eq!(s.apps_on_allowance, 1);
        assert!(s.syncs_limited >= 2);
        // an address without connections is dropped once its allowance is full again
        let old = Instant::now().checked_sub(SYNC_ALLOWANCE_WINDOW + Duration::from_secs(1));
        if let Some(old) = old {
            lock(&relay.inner.allowances).insert(
                ip("10.0.0.9"),
                SyncAllowance {
                    left: 0,
                    at: Some(old),
                    used_up: true,
                },
            );
            relay.clean();
            let map = lock(&relay.inner.allowances);
            assert!(!map.contains_key(&ip("10.0.0.9")));
            assert!(map.contains_key(&ip("10.0.0.1")));
        }
    }

    /// A sync answer starts only when it fits into the send budget, and gives its bytes back as
    /// it is written.
    #[tokio::test]
    async fn budget_holds_a_large_answer_until_the_queue_drains() {
        let relay = relay_with(|c| c.queue_budget = 8 * 1024);
        let mut m = signed_like_message(&"cd".repeat(32));
        m["cipher"] = json!("ab".repeat(1_500));
        lock(&relay.inner.pool).insert(m, now_ms());
        let (a, mut rx_a) = conn(&relay, Role::Client, 1);
        let (b, _rx_b) = conn(&relay, Role::Client, 2);
        // 6 KiB of notifications wait in a's queue
        assert!(a.send_bytes(Bytes::from(vec![b' '; 6 * 1024])));
        let _task = request_some(&relay, &b, 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(sync_frame(&b).is_none(), "the answer waits for the budget");
        assert_eq!(relay.inner.sync_reserved.load(Ordering::Acquire), 0);
        assert_eq!(relay.stats().snapshot().syncs_running, 1);
        // the writer takes a's packet: the answer fits now
        if let Ok(Outgoing::Json(p)) = rx_a.try_recv() {
            a.tx.dequeued(p.len());
        }
        let mut frame = take_frame(&b).await;
        let len = frame.body.len();
        assert!(len > 3_000, "{len}");
        assert_eq!(relay.inner.sync_reserved.load(Ordering::Acquire), len);
        frame.reserved.release(1_000);
        assert_eq!(
            relay.inner.sync_reserved.load(Ordering::Acquire),
            len - 1_000
        );
        drop(frame);
        assert_eq!(relay.inner.sync_reserved.load(Ordering::Acquire), 0);
        assert!(!a.stop.is_cancelled());
    }

    /// New app connections pause when the budget is mostly used or more syncs wait than there
    /// are slots, and resume below the lower share; connected apps and relays go on.
    #[tokio::test]
    async fn admission_pauses_and_resumes_with_hysteresis() {
        // the rule with 8 slots and a budget of 1000 bytes
        assert!(!admission_paused(false, 8, 799, 8, 1000));
        assert!(admission_paused(false, 9, 0, 8, 1000));
        assert!(admission_paused(false, 0, 800, 8, 1000));
        assert!(admission_paused(true, 0, 700, 8, 1000));
        assert!(admission_paused(true, 5, 0, 8, 1000));
        assert!(!admission_paused(true, 4, 599, 8, 1000));

        let relay = relay_with(|c| c.queue_budget = 1000);
        let stats = relay.stats();
        assert_eq!(stats.snapshot().apps_cap, DEFAULT_MAX_CLIENTS as u64);
        let (a, mut rx_a) = conn(&relay, Role::Client, 1);
        let mut held = Reserved::new(relay.inner.sync_reserved.clone(), 800);
        assert!(
            conn_from(&relay, Role::Client, 2, None).is_none(),
            "paused at 80 %"
        );
        let s = stats.snapshot();
        assert!(s.admission_paused);
        assert_eq!(s.refused_busy, 1);
        assert_eq!(s.queued_bytes, 800);
        assert_eq!(s.clients, 1);
        // relays and the connected app go on
        let _node = conn_from(&relay, Role::Node, 3, None).expect("relays are not paused");
        a.send(&json!({ "type": "new-message", "messages": [] }));
        match rx_a.try_recv() {
            Ok(Outgoing::Json(p)) => a.tx.dequeued(p.len()),
            other => panic!("{other:?}"),
        }
        assert!(!a.stop.is_cancelled());
        // still paused at 70 %, resumed below 60 %
        held.resize(700);
        assert!(conn_from(&relay, Role::Client, 4, None).is_none());
        held.resize(500);
        let _b = conn_from(&relay, Role::Client, 5, None).expect("resumed");
        let s = stats.snapshot();
        assert!(!s.admission_paused);
        assert_eq!(s.refused_busy, 2);
        assert_eq!(s.queued_bytes, 500);
        drop(held);
        relay.refresh_admission();
        assert_eq!(stats.snapshot().queued_bytes, 0);
    }

    /// A writer for `c` on an in-memory stream of `buffer` bytes; returns the app's end.
    fn spawn_writer(
        relay: &Relay,
        c: &Arc<Conn>,
        rx: mpsc::Receiver<Outgoing>,
        buffer: usize,
    ) -> tokio::io::DuplexStream {
        let (ours, theirs) = tokio::io::duplex(buffer);
        let sync_rx = lock(&c.sync_rx).take();
        tokio::spawn(write_loop(
            ours,
            rx,
            sync_rx,
            c.clone(),
            relay.inner.shutdown.clone(),
            relay.inner.sync_turns.clone(),
        ));
        theirs
    }

    /// A sync answer of `len` bytes for `c`, handed over as `client_request` does.
    fn queue_answer(relay: &Relay, c: &Conn, len: usize) -> Bytes {
        let body = Bytes::from(vec![b'x'; len]);
        let frame = SyncFrame {
            body: body.clone(),
            reserved: Reserved::new(relay.inner.sync_reserved.clone(), len),
            _slot: None,
            _turn: None,
        };
        assert!(c.sync_tx.try_send(frame).is_ok());
        body
    }

    /// The next packet the app reads, within `wait`.
    async fn next_packet(
        reader: &mut FramedRead<tokio::io::DuplexStream, BareRpcCodec>,
        wait: Duration,
    ) -> Bytes {
        match tokio::time::timeout(wait, reader.next())
            .await
            .expect("a packet in time")
            .expect("stream open")
            .expect("a valid frame")
        {
            RpcMessage::Request { data: Some(d), .. } => d,
            other => panic!("{other:?}"),
        }
    }

    /// Notifications go out at once while sync answers fill the budget and wait for their
    /// turn; the answers follow complete.
    #[tokio::test]
    async fn notifications_go_first_while_syncs_fill_the_budget() {
        let relay = relay_with(|c| {
            c.sync_writers = 1;
            c.queue_budget = 2 * 1024 * 1024;
        });
        // the only turn is taken: no answer can start
        let turn = relay
            .inner
            .sync_turns
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let mut apps = Vec::new();
        for k in 1..=2u8 {
            let (c, rx) = conn(&relay, Role::Client, k);
            let stream = spawn_writer(&relay, &c, rx, 2 * 1024 * 1024);
            let body = queue_answer(&relay, &c, 1024 * 1024);
            apps.push((
                c,
                FramedRead::new(stream, BareRpcCodec::new(usize::MAX)),
                body,
            ));
        }
        assert!(relay.refresh_admission(), "the budget is full");
        lock(&relay.inner.state)
            .outbox
            .push(json!({ "hash": "n1" }));
        relay.flush_clients();
        for (_, reader, _) in &mut apps {
            let packet = next_packet(reader, Duration::from_secs(1)).await;
            let first: Value = serde_json::from_slice(&packet).unwrap();
            assert_eq!(first["type"], "new-message");
            assert_eq!(first["messages"][0]["hash"], "n1");
        }
        drop(turn);
        for (c, reader, body) in &mut apps {
            assert_eq!(next_packet(reader, Duration::from_secs(5)).await, *body);
            assert!(!c.stop.is_cancelled());
        }
        wait_for("the budget is given back", || {
            relay.inner.sync_reserved.load(Ordering::Acquire) == 0
        })
        .await;
        assert!(!relay.refresh_admission());
    }

    /// Two sync answers written at the same time take turns of `SYNC_CHUNK` bytes.
    #[tokio::test]
    async fn two_syncs_progress_in_turns() {
        use tokio::io::AsyncReadExt;

        let relay = relay_with(|c| c.sync_writers = 1);
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let len = 8 * SYNC_CHUNK;
        let mut readers = Vec::new();
        for k in 1..=2u8 {
            let (c, rx) = conn(&relay, Role::Client, k);
            let mut stream = spawn_writer(&relay, &c, rx, 16 * 1024);
            queue_answer(&relay, &c, len);
            let log = log.clone();
            readers.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 8 * 1024];
                let mut total = 0;
                while total < len {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                    lock(&log).push(k);
                }
                total
            }));
        }
        for r in readers {
            assert!(r.await.unwrap() >= len);
        }
        let log = lock(&log).clone();
        let switches = log.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(switches >= 4, "{switches} switches");
    }
}
