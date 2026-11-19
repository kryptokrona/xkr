//! The clients topic (`xkr/<network>/clients/v1`; `docs/hyper-transport.md`, "Clients
//! topic"): programs without the chain (the chainless xkrd modes, later wallet tools) use this
//! node over Hyperswarm.
//!
//! [`crate::hyper`] announces the topic (server mode, once the node is synchronized; with
//! `--private` only the private clients topic of the invite) and hands every connection on it
//! to [`ClientsServer`], which admits it ([`ClientsServer::admit`]: `--max-clients`, per
//! address, bans) and serves it with the `xkr-hyper-rpc` protocol:
//!
//! * service `node`: the allowlisted JSON-RPC methods ([`NODE_METHODS`]) answered by the node's
//!   own dispatcher in-process (`xkr_rpc::call`, no HTTP), and a `tip` push on every new
//!   main-chain tip;
//! * service `stratum`: listed while the built-in p2pool's stratum server runs. The methods
//!   [`STRATUM_OPEN`], [`STRATUM_MESSAGE`] and [`STRATUM_CLOSE`] carry p2pool stratum messages
//!   (`login`, `submit`, `keepalived`, `getjob`) into a session of the node's own
//!   [`StratumServer`](xkr_p2pool::stratum::StratumServer), the same session a TCP miner gets
//!   (its line limit, login timeout, share checks and score); the session's lines (answers and
//!   `job` notifications) come back as [`STRATUM_MESSAGE`] pushes, its end as a
//!   [`STRATUM_CLOSED`] push. The methods are also answered on a `node` connection of a node
//!   that runs p2pool: Hyperswarm keeps one connection per client key on a topic, so a
//!   chainless xkrd uses the connection it already has for the chain.
//!
//! A connection that breaks the protocol or keeps calling past its limits is closed and its DHT key is
//! refused for [`BAN`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _,
};
use tokio::sync::{Semaphore, mpsc};
use xkr_consensus::Network;
use xkr_hyper_rpc::{
    Accepted, Handler, Hello, HelloReply, Push, RefuseReason, Refused, RpcError, ServeEnd,
    ServerLimits, Service, Tip, VERSION, code,
};
use xkr_p2pool::stratum::{STRATUM_BUF_SIZE, StratumHandle, StreamEnd};
use xkr_rpc::{ClientStats, RpcState};

use crate::hyper::{self, Key};

/// Default of `--max-clients`: as many as Hugin app connections (`--hugin-max-clients`).
pub const DEFAULT_MAX_CLIENTS: usize = xkr_hugin::relay::DEFAULT_MAX_CLIENTS;

/// The JSON-RPC methods of service `node`. `getinfo` is `/json_rpc` `get_info` (the `/getinfo`
/// answer); `get_block_blob` is served here only ([`block_blob`]).
pub const NODE_METHODS: [&str; 8] = [
    "get_miner_data",
    "get_block_headers_range",
    "getblockheaderbyheight",
    "getblockheaderbyhash",
    "submitblock",
    "getinfo",
    "getstatehash",
    GET_BLOCK_BLOB,
];

/// `get_block_blob {"height": h}` -> `{"height", "hash", "blob"}`: the main-chain block at `h`
/// as stored (header, parent block, miner transaction and transaction hashes; no
/// transactions), hex. A client hashes it to check the block's id and proof of work.
pub const GET_BLOCK_BLOB: &str = "get_block_blob";

/// The HTTP paths of service `wallet` (method [`WALLET_HTTP`]): what the wallet tools and
/// wallet-backend-js call on a daemon.
pub const WALLET_PATHS: [&str; 16] = [
    "/getinfo",
    "/info",
    "/getheight",
    "/height",
    "/fee",
    "/feeinfo",
    "/getwalletsyncdata",
    "/get_global_indexes_for_range",
    "/get_transactions_status",
    "/sendrawtransaction",
    "/getrandom_outs",
    "/get_pool_changes_lite",
    "/queryblockslite",
    "/gettransactions",
    "/get_o_indexes",
    "/json_rpc",
];

/// The `/json_rpc` methods of service `wallet`.
pub const WALLET_JSON_RPC: [&str; 6] = [
    "get_block_headers_range",
    "getblockcount",
    "getblockheaderbyheight",
    "getblockheaderbyhash",
    "getlastblockheader",
    "get_info",
];

/// `http {"method": "GET"|"POST", "path": "/…", "body": {…}}` -> `{"status": n, "body": …}`:
/// one daemon HTTP request of service `wallet`, answered as the node's HTTP server answers it
/// (a path in [`WALLET_PATHS`], at most one query string, `/json_rpc` with a method in
/// [`WALLET_JSON_RPC`]).
pub const WALLET_HTTP: &str = "http";

/// Where a client connection came in: the topic decides the services offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    /// The public clients topic: `node`, and `stratum` while p2pool runs.
    Clients,
    /// The public wallets topic: `wallet`.
    Wallets,
    /// The private clients topic of the node's invite: every service.
    Private,
}

/// Client connections from one IP address (IPv6: one /64).
pub const MAX_CLIENTS_PER_IP: usize = 8;

/// Calls of all clients that read storage at once.
const MAX_BLOCKING_CALLS: usize = 16;

/// How long a misbehaving client's key is refused.
pub const BAN: Duration = Duration::from_mins(10);

/// `stratum_open {}` -> `{"session": n}`: starts the connection's stratum session (a session
/// of this connection that is still open ends first).
pub const STRATUM_OPEN: &str = "stratum_open";
/// `stratum {"session": n, "message": {...}}` -> `null`: one stratum message (a JSON object,
/// shorter than a p2pool stratum line) into session `n`. Also the name of the pushes
/// `{"session": n, "message": {...}}` that carry the session's answers and jobs.
pub const STRATUM_MESSAGE: &str = "stratum";
/// `stratum_close {"session": n}` -> `null`: ends session `n`.
pub const STRATUM_CLOSE: &str = "stratum_close";
/// Push `{"session": n}`: session `n` ended (the client closed it, a new one was opened, or the
/// stratum server closed it).
pub const STRATUM_CLOSED: &str = "stratum_closed";
/// Messages of a stratum session waiting to be read by the stratum server.
const STRATUM_QUEUE: usize = 32;
/// Session lines waiting to be pushed.
const STRATUM_PUSHES: usize = 32;
/// Buffer between a session and its pumps.
const STRATUM_PIPE: usize = 16 * 1024;
/// Longest line taken from the stratum server.
const STRATUM_SERVER_LINE: u64 = 64 * 1024;

/// The built-in p2pool's stratum server while it runs ([`crate::p2pool::PoolLink`]).
pub trait StratumSource: Send + Sync + std::fmt::Debug {
    /// Its sessions, `None` while it does not run.
    fn stratum(&self) -> Option<StratumHandle>;
}

/// Why a client connection was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAdmitted {
    /// Its key is banned.
    Banned,
    /// `--max-clients` connections are open.
    Full,
    /// [`MAX_CLIENTS_PER_IP`] connections from its address are open.
    PerIp,
}

#[derive(Debug, Default)]
struct Book {
    /// Key -> end of its ban.
    bans: HashMap<Key, Instant>,
    /// Open connections per address key.
    per_ip: HashMap<IpAddr, usize>,
}

/// The server side of the clients topic; lives across bridge restarts.
#[derive(Debug)]
pub struct ClientsServer {
    rpc: Arc<RpcState>,
    stats: Arc<ClientStats>,
    network: Network,
    max_clients: usize,
    limits: ServerLimits,
    /// The built-in p2pool's stratum server (`None`: p2pool off).
    p2pool: Option<Arc<dyn StratumSource>>,
    blocking: Arc<Semaphore>,
    book: Mutex<Book>,
}

/// An admitted connection's place; dropping it frees the place.
#[derive(Debug)]
pub struct Slot {
    server: Arc<ClientsServer>,
    ip: Option<IpAddr>,
    addr: Option<SocketAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut book = self.server.book();
        self.server
            .stats
            .connections
            .fetch_sub(1, Ordering::Relaxed);
        if let Some(ip) = self.ip
            && let Some(n) = book.per_ip.get_mut(&ip)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                book.per_ip.remove(&ip);
            }
        }
    }
}

impl ClientsServer {
    /// The server over the node's RPC state; `p2pool` is the built-in p2pool's stratum server
    /// (service `stratum` is listed while it runs).
    #[must_use]
    pub fn new(
        rpc: Arc<RpcState>,
        stats: Arc<ClientStats>,
        network: Network,
        max_clients: usize,
        p2pool: Option<Arc<dyn StratumSource>>,
    ) -> Arc<Self> {
        Self::with_limits(
            rpc,
            stats,
            network,
            max_clients,
            p2pool,
            ServerLimits::default(),
        )
    }

    /// [`ClientsServer::new`] with other per-connection limits.
    #[must_use]
    pub fn with_limits(
        rpc: Arc<RpcState>,
        stats: Arc<ClientStats>,
        network: Network,
        max_clients: usize,
        p2pool: Option<Arc<dyn StratumSource>>,
        limits: ServerLimits,
    ) -> Arc<Self> {
        Arc::new(Self {
            rpc,
            stats,
            network,
            max_clients,
            limits,
            p2pool,
            blocking: Arc::new(Semaphore::new(MAX_BLOCKING_CALLS)),
            book: Mutex::new(Book::default()),
        })
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes a place for a connection from `key` at `addr`, unless the key is banned, every
    /// place is taken or its address has [`MAX_CLIENTS_PER_IP`] already.
    pub fn admit(
        self: &Arc<Self>,
        key: &Key,
        addr: Option<SocketAddr>,
        now: Instant,
    ) -> Result<Slot, NotAdmitted> {
        let mut book = self.book();
        if let Some(until) = book.bans.get(key) {
            if *until > now {
                return Err(NotAdmitted::Banned);
            }
            book.bans.remove(key);
        }
        if self.stats.connections.load(Ordering::Relaxed) >= self.max_clients {
            return Err(NotAdmitted::Full);
        }
        let ip = addr.map(hyper::address_key);
        if let Some(ip) = ip {
            let n = book.per_ip.entry(ip).or_default();
            if *n >= MAX_CLIENTS_PER_IP {
                return Err(NotAdmitted::PerIp);
            }
            *n += 1;
        }
        self.stats.connections.fetch_add(1, Ordering::Relaxed);
        Ok(Slot {
            server: self.clone(),
            ip,
            addr,
        })
    }

    /// Refuses `key` for [`BAN`] from `now`.
    pub fn ban(&self, key: Key, now: Instant) {
        let mut book = self.book();
        hyper::make_room(&mut book.bans, &key, |until| *until);
        book.bans.insert(key, now + BAN);
    }

    /// Serves an admitted connection that came in through `door` until it ends; bans `key` if
    /// the client broke the protocol or its limits.
    pub async fn serve<S>(self: Arc<Self>, slot: Slot, key: Key, stream: S, door: Door) -> ServeEnd
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let addr = slot.addr;
        let end = xkr_hyper_rpc::serve(stream, self.limits, |hello| {
            self.accept(hello, key, addr, door)
        })
        .await;
        if end.is_misbehaviour() {
            self.ban(key, Instant::now());
        }
        drop(slot);
        end
    }

    /// Resolves once the node is synchronized (`false` if the node's status is gone first).
    pub async fn wait_synced(&self) -> bool {
        let mut net = self.rpc.net.clone();
        net.wait_for(|n| hyper::synced_now(n)).await.is_ok()
    }

    /// The built-in p2pool's stratum server, while it runs.
    fn stratum_handle(&self) -> Option<StratumHandle> {
        self.p2pool.as_ref().and_then(|s| s.stratum())
    }

    /// The services offered now through `door`.
    fn services(&self, door: Door) -> Vec<Service> {
        if door == Door::Wallets {
            return vec![Service::Wallet];
        }
        let mut out = vec![Service::Node];
        if self.stratum_handle().is_some() {
            out.push(Service::Stratum);
        }
        if door == Door::Private {
            out.push(Service::Wallet);
        }
        out
    }

    /// The answer to an accepted hello through `door`: the tip, the sync state, the services,
    /// the newest stable state hash and the pruning seed.
    #[must_use]
    pub fn hello_reply(&self, door: Door) -> HelloReply {
        let tip = self.rpc.chain.tip();
        let net = self.rpc.net.borrow().clone();
        let state = self.rpc.chain.state_view().latest();
        HelloReply {
            v: VERSION,
            height: tip.height,
            hash: tip.hash.to_string(),
            synced: hyper::synced_now(&net),
            services: self
                .services(door)
                .into_iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            state_span: state.map(|(span, _)| span),
            state_hash: state.map(|(_, hash)| hash.to_string()),
            pruning_seed: net.pruning_seed,
        }
    }

    fn accept(
        self: &Arc<Self>,
        hello: &Hello,
        key: Key,
        addr: Option<SocketAddr>,
        door: Door,
    ) -> Result<Accepted, Refused> {
        let network = hyper::xkr_network(self.network).as_str();
        if hello.network != network {
            return Err(Refused::new(
                RefuseReason::Network,
                format!("this node is on {network}"),
            ));
        }
        let offered = self.services(door);
        let Some(service) = Service::parse(&hello.service).filter(|s| offered.contains(s)) else {
            let names: Vec<&str> = offered.iter().map(|s| s.as_str()).collect();
            return Err(Refused::new(
                RefuseReason::Service,
                format!("this node serves {}", names.join(", ")),
            ));
        };
        // The stratum methods, on a `node` connection too while p2pool is on.
        let (stratum, stratum_pushes) = if self.p2pool.is_some() {
            let (tx, rx) = mpsc::channel(STRATUM_PUSHES);
            let conn = Arc::new(StratumConn {
                server: self.clone(),
                key,
                addr,
                pushes: tx,
                state: Arc::new(Mutex::new(StratumState::default())),
            });
            (Some(conn), Some(receiver_stream(rx)))
        } else {
            (None, None)
        };
        // Subscribe before the reply is read, so no tip falls between the two.
        let (handler, pushes): (Arc<dyn Handler>, _) = match service {
            Service::Node => {
                let tips = self.tip_pushes();
                let pushes = match stratum_pushes {
                    Some(s) => futures::stream::select(tips, s).boxed(),
                    None => tips,
                };
                (
                    Arc::new(ConnHandler {
                        node: Some(NodeHandler {
                            rpc: self.rpc.clone(),
                            stats: self.stats.clone(),
                            blocking: self.blocking.clone(),
                        }),
                        stratum,
                    }) as Arc<dyn Handler>,
                    Some(pushes),
                )
            }
            Service::Stratum => (
                Arc::new(ConnHandler {
                    node: None,
                    stratum,
                }) as Arc<dyn Handler>,
                stratum_pushes,
            ),
            Service::Wallet => (
                Arc::new(WalletHandler {
                    rpc: self.rpc.clone(),
                    stats: self.stats.clone(),
                }) as Arc<dyn Handler>,
                Some(self.tip_pushes()),
            ),
        };
        tracing::debug!(client = %hello.client, %service, "client on the clients topic");
        Ok(Accepted {
            reply: self.hello_reply(door),
            handler,
            pushes,
        })
    }

    /// A `tip` push for every new main-chain tip from now on.
    fn tip_pushes(&self) -> BoxStream<'static, Push> {
        tip_pushes(&self.rpc)
    }
}

/// A `tip` push for every new main-chain tip of `rpc`'s chain from now on.
pub(crate) fn tip_pushes(rpc: &RpcState) -> BoxStream<'static, Push> {
    let tips = rpc.chain.subscribe_tip();
    let net = rpc.net.clone();
    futures::stream::unfold((tips, net), |(mut tips, net)| async move {
        tips.changed().await.ok()?;
        let tip = *tips.borrow_and_update();
        let synced = hyper::synced_now(&net.borrow());
        let push = Push::tip(&Tip {
            height: tip.height,
            hash: tip.hash.to_string(),
            synced,
        });
        Some((push, (tips, net)))
    })
    .boxed()
}

/// Service `node`: [`NODE_METHODS`] through the node's JSON-RPC dispatcher.
struct NodeHandler {
    rpc: Arc<RpcState>,
    stats: Arc<ClientStats>,
    blocking: Arc<Semaphore>,
}

/// The calls of one connection: service `node` ([`NodeHandler`]) and, with p2pool on, the
/// stratum methods ([`StratumConn`]).
struct ConnHandler {
    node: Option<NodeHandler>,
    stratum: Option<Arc<StratumConn>>,
}

impl Handler for ConnHandler {
    fn call(&self, method: String, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        if let Some(s) = &self.stratum
            && let Some(outcome) = s.call(&method, &params)
        {
            return futures::future::ready(outcome).boxed();
        }
        match &self.node {
            Some(n) => n.call(method, params),
            None => futures::future::ready(Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("{method} is not served on the stratum service"),
            )))
            .boxed(),
        }
    }
}

impl NodeHandler {
    fn call(&self, method: String, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        if !NODE_METHODS.contains(&method.as_str()) {
            let error = RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("{method} is not served on the clients topic"),
            );
            return futures::future::ready(Err(error)).boxed();
        }
        let (rpc, stats, blocking) = (self.rpc.clone(), self.stats.clone(), self.blocking.clone());
        async move {
            let _permit = blocking
                .acquire_owned()
                .await
                .map_err(|_| RpcError::new(code::INTERNAL, "the node is stopping"))?;
            let outcome = tokio::task::spawn_blocking(move || node_call(&rpc, &method, &params))
                .await
                .map_err(|e| RpcError::new(code::INTERNAL, e.to_string()))?;
            stats.served_calls.fetch_add(1, Ordering::Relaxed);
            outcome
        }
        .boxed()
    }
}

/// One call of service `node` (an allowlisted method) on the node's RPC state; blocks on
/// storage reads.
pub(crate) fn node_call(rpc: &RpcState, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        GET_BLOCK_BLOB => block_blob(rpc, params),
        "getinfo" => xkr_rpc::call(rpc, "get_info", params),
        other => xkr_rpc::call(rpc, other, params),
    }
    .map_err(|e| RpcError::new(e.code, e.message))
}

/// [`GET_BLOCK_BLOB`].
fn block_blob(rpc: &RpcState, params: &Value) -> Result<Value, xkr_rpc::CallError> {
    let fail = |code: i64, message: String| xkr_rpc::CallError { code, message };
    let height = params
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| fail(code::INVALID_PARAMS, "height (a number) is required".into()))?;
    let chain = &rpc.chain;
    let reader = chain
        .reader()
        .map_err(|e| fail(code::INTERNAL, e.to_string()))?;
    // -2: the C++ CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT, as getblockheaderbyheight.
    let hash = reader
        .block_hash(height)
        .map_err(|e| fail(code::INTERNAL, e.to_string()))?
        .ok_or_else(|| fail(-2, format!("no block at height {height}")))?;
    let blob = chain
        .with_compressor(|c| reader.block_blob(height, c))
        .map_err(|e| fail(code::INTERNAL, e.to_string()))?
        .map_err(|e| fail(code::INTERNAL, e.to_string()))?
        .ok_or_else(|| fail(-2, format!("no block at height {height}")))?;
    Ok(serde_json::json!({
        "height": height,
        "hash": hash.to_string(),
        "blob": hex::encode(blob),
    }))
}

/// The pushes of a channel, as a stream.
fn receiver_stream(rx: mpsc::Receiver<Push>) -> BoxStream<'static, Push> {
    futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|p| (p, rx)) }).boxed()
}

/// The stratum session state of one connection.
#[derive(Default)]
struct StratumState {
    /// Number of the last session opened.
    last: u64,
    /// The open session: its number and its message queue (dropping the queue ends it).
    current: Option<(u64, mpsc::Sender<Vec<u8>>)>,
    /// A session of this connection broke the stratum server's rules: no new one.
    refused: bool,
}

/// The stratum methods of one connection: at most one session at a time, in the node's own
/// stratum server.
struct StratumConn {
    server: Arc<ClientsServer>,
    key: Key,
    addr: Option<SocketAddr>,
    pushes: mpsc::Sender<Push>,
    state: Arc<Mutex<StratumState>>,
}

fn locked(m: &Mutex<StratumState>) -> std::sync::MutexGuard<'_, StratumState> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn session_of(params: &Value) -> Result<u64, RpcError> {
    params
        .get("session")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "session (a number) is required"))
}

impl StratumConn {
    /// Answers `method` if it is a stratum method (in call order: a message is queued before
    /// the next call is read).
    fn call(&self, method: &str, params: &Value) -> Option<Result<Value, RpcError>> {
        Some(match method {
            STRATUM_OPEN => self.open(),
            STRATUM_MESSAGE => self.message(params),
            STRATUM_CLOSE => session_of(params).map(|n| {
                let mut st = locked(&self.state);
                if st.current.as_ref().is_some_and(|(id, _)| *id == n) {
                    st.current = None;
                }
                Value::Null
            }),
            _ => return None,
        })
    }

    fn open(&self) -> Result<Value, RpcError> {
        let Some(handle) = self.server.stratum_handle() else {
            return Err(RpcError::new(
                code::UNAVAILABLE,
                "the stratum server of this node's p2pool is not running",
            ));
        };
        let (id, rx) = {
            let mut st = locked(&self.state);
            if st.refused {
                return Err(RpcError::new(
                    code::UNAVAILABLE,
                    "no stratum session on this connection",
                ));
            }
            st.last += 1;
            let id = st.last;
            let (tx, rx) = mpsc::channel(STRATUM_QUEUE);
            // Replacing the queue ends the session before.
            st.current = Some((id, tx));
            (id, rx)
        };
        let ends = SessionEnds {
            server: self.server.clone(),
            key: self.key,
            state: self.state.clone(),
        };
        tokio::spawn(run_stratum(
            handle,
            id,
            rx,
            self.pushes.clone(),
            self.addr,
            ends,
        ));
        Ok(json!({ "session": id }))
    }

    fn message(&self, params: &Value) -> Result<Value, RpcError> {
        let n = session_of(params)?;
        let Some(message) = params.get("message").filter(|m| m.is_object()) else {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                "message (a stratum JSON object) is required",
            ));
        };
        let line = serde_json::to_vec(message)
            .map_err(|e| RpcError::new(code::INVALID_PARAMS, e.to_string()))?;
        if line.len() >= STRATUM_BUF_SIZE {
            return Err(RpcError::new(
                code::INVALID_PARAMS,
                format!(
                    "a stratum message is at most {} bytes",
                    STRATUM_BUF_SIZE - 1
                ),
            ));
        }
        let st = locked(&self.state);
        let Some((_, queue)) = st.current.as_ref().filter(|(id, _)| *id == n) else {
            return Err(RpcError::new(
                code::UNAVAILABLE,
                format!("no open stratum session {n} ({STRATUM_OPEN} opens one)"),
            ));
        };
        match queue.try_send(line) {
            Ok(()) => Ok(Value::Null),
            Err(mpsc::error::TrySendError::Full(_)) => Err(RpcError::new(
                code::BUSY,
                "the stratum session has not read the messages before",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(RpcError::new(
                code::UNAVAILABLE,
                format!("stratum session {n} has ended"),
            )),
        }
    }
}

/// What the end of a stratum session changes.
struct SessionEnds {
    server: Arc<ClientsServer>,
    key: Key,
    state: Arc<Mutex<StratumState>>,
}

impl SessionEnds {
    fn ended(&self, id: u64, end: StreamEnd) {
        let mut st = locked(&self.state);
        if st.current.as_ref().is_some_and(|(n, _)| *n == id) {
            st.current = None;
        }
        if end == StreamEnd::Banned {
            st.refused = true;
            drop(st);
            tracing::debug!(peer = %hex::encode(&self.key[..8]), "stratum session over the clients topic refused");
            self.server.ban(self.key, Instant::now());
        }
    }
}

/// One stratum session: the messages of `queue` go into the stratum server as lines, its lines
/// come back as pushes; then the end is pushed.
async fn run_stratum(
    handle: StratumHandle,
    id: u64,
    mut queue: mpsc::Receiver<Vec<u8>>,
    pushes: mpsc::Sender<Push>,
    addr: Option<SocketAddr>,
    ends: SessionEnds,
) {
    let (ours, theirs) = tokio::io::duplex(STRATUM_PIPE);
    let session = tokio::spawn(async move { handle.serve_stream(theirs, addr).await });
    let (rd, mut wr) = tokio::io::split(ours);
    let feed = tokio::spawn(async move {
        while let Some(mut line) = queue.recv().await {
            line.push(b'\n');
            if wr.write_all(&line).await.is_err() {
                return;
            }
        }
        // The client closed the session: the stratum server reads the end.
        let _ = wr.shutdown().await;
    });
    let mut reader = tokio::io::BufReader::new(rd);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // The connection is gone (its pushes are no longer read): the session ends.
        let mut limited = (&mut reader).take(STRATUM_SERVER_LINE);
        let read = tokio::select! {
            r = limited.read_until(b'\n', &mut buf) => r,
            () = pushes.closed() => break,
        };
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.last() != Some(&b'\n') {
            break;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let push = Push {
            method: STRATUM_MESSAGE.to_owned(),
            params: json!({ "session": id, "message": message }),
        };
        if pushes.send(push).await.is_err() {
            break;
        }
    }
    feed.abort();
    drop(reader);
    let end = session.await.unwrap_or(StreamEnd::Closed);
    ends.ended(id, end);
    let _ = pushes.try_send(Push {
        method: STRATUM_CLOSED.to_owned(),
        params: json!({ "session": id }),
    });
}

/// A service that is negotiated but not served yet (`wallet`).
/// Service `wallet`: [`WALLET_HTTP`] requests through the node's HTTP router, in process.
struct WalletHandler {
    rpc: Arc<RpcState>,
    stats: Arc<ClientStats>,
}

impl Handler for WalletHandler {
    fn call(&self, method: String, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        if method != WALLET_HTTP {
            let error = RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("{method} is not served on the wallet service (only {WALLET_HTTP})"),
            );
            return futures::future::ready(Err(error)).boxed();
        }
        let request = match WalletRequest::parse(&params) {
            Ok(r) => r,
            Err(e) => return futures::future::ready(Err(e)).boxed(),
        };
        let (rpc, stats) = (self.rpc.clone(), self.stats.clone());
        async move {
            let answer =
                xkr_rpc::inproc::request(rpc, request.method, &request.path, request.body.as_ref())
                    .await
                    .map_err(|e| RpcError::new(code::INTERNAL, e))?;
            stats.served_calls.fetch_add(1, Ordering::Relaxed);
            Ok(json!({"status": answer.status, "body": answer.body}))
        }
        .boxed()
    }
}

/// A checked [`WALLET_HTTP`] request.
#[derive(Debug, PartialEq)]
struct WalletRequest {
    method: &'static str,
    path: String,
    body: Option<Value>,
}

impl WalletRequest {
    fn parse(params: &Value) -> Result<Self, RpcError> {
        let invalid = |m: &str| RpcError::new(code::INVALID_PARAMS, m.to_owned());
        let method = match params.get("method").and_then(Value::as_str) {
            Some("GET") => "GET",
            Some("POST") => "POST",
            _ => return Err(invalid("method must be GET or POST")),
        };
        let path = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("path is missing"))?;
        let bare = path.split_once('?').map_or(path, |(p, _)| p);
        if !WALLET_PATHS.contains(&bare) || path.contains('#') || path.matches('?').count() > 1 {
            return Err(RpcError::new(
                code::METHOD_NOT_FOUND,
                format!("{bare} is not served on the wallet service"),
            ));
        }
        let body = params.get("body").filter(|b| !b.is_null()).cloned();
        if bare == "/json_rpc" {
            let rpc_method = body
                .as_ref()
                .and_then(|b| b.get("method"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !WALLET_JSON_RPC.contains(&rpc_method) {
                return Err(RpcError::new(
                    code::METHOD_NOT_FOUND,
                    format!("json_rpc {rpc_method} is not served on the wallet service"),
                ));
            }
        }
        Ok(Self {
            method,
            path: path.to_owned(),
            body,
        })
    }
}

/// A stand-in p2pool stratum server for the tests of the `stratum` service.
#[cfg(test)]
pub(crate) mod test_stratum {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};

    use xkr_p2pool::Difficulty;
    use xkr_p2pool::stratum::{
        HashingBlob, ShareSink, StratumHandle, StratumServer, StratumServerConfig,
        TemplateDifficulties, TemplateSource,
    };
    use xkr_types::Hash;

    use super::StratumSource;

    /// Major 5, minor 16, a 5-byte timestamp varint; the nonce follows the 32-byte `prev_id`.
    const HEADER: [u8; 7] = [5, 16, 0x80, 0x80, 0x80, 0x80, 0x01];
    const NONCE_OFFSET: usize = 39;

    /// One template at difficulty 1 on both chains: every share that meets the job target is
    /// a sidechain share.
    pub(crate) struct FakeTemplate;

    fn hashing_blob(template_id: u32, extra_nonce: u32) -> HashingBlob {
        let mut blob = HEADER.to_vec();
        blob.extend_from_slice(&[0x11; 32]);
        blob.extend_from_slice(&[0; 4]);
        blob.extend_from_slice(&template_id.to_le_bytes());
        blob.extend_from_slice(&extra_nonce.to_le_bytes());
        blob.extend_from_slice(&[0x22; 24]);
        blob.push(1);
        HashingBlob {
            blob,
            nonce_offset: NONCE_OFFSET,
            template_id,
            height: 100,
            sidechain_height: 10,
            mainchain_difficulty: Difficulty::from_u64(1),
            sidechain_difficulty: Difficulty::from_u64(1),
            seed_hash: Hash::ZERO,
        }
    }

    impl TemplateSource for FakeTemplate {
        fn hashing_blob(&self, extra_nonce: u32) -> Option<HashingBlob> {
            Some(hashing_blob(1, extra_nonce))
        }

        fn hashing_blob_for(&self, template_id: u32, extra_nonce: u32) -> Option<HashingBlob> {
            (template_id == 1).then(|| hashing_blob(1, extra_nonce))
        }

        fn difficulties(&self, template_id: u32) -> Option<TemplateDifficulties> {
            self.hashing_blob_for(template_id, 0)
                .map(|b| TemplateDifficulties::from(&b))
        }

        fn sidechain_difficulty(&self) -> Difficulty {
            Difficulty::from_u64(1)
        }

        fn sidechain_tip_height(&self) -> Option<u64> {
            Some(10)
        }

        fn chain_window_size(&self) -> u64 {
            2160
        }
    }

    /// The shares that reached the sidechain: `(template_id, nonce, extra_nonce)`.
    #[derive(Default)]
    pub(crate) struct FakeSink {
        pub shares: Mutex<Vec<(u32, u32, u32)>>,
    }

    impl ShareSink for FakeSink {
        fn submit_block(&self, _template_id: u32, _nonce: u32, _extra_nonce: u32) {}

        fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool {
            self.shares
                .lock()
                .unwrap()
                .push((template_id, nonce, extra_nonce));
            true
        }
    }

    /// The stratum server as a [`StratumSource`]; `None` stands for p2pool not running.
    #[derive(Debug, Default)]
    pub(crate) struct TestStratum(pub Mutex<Option<StratumHandle>>);

    impl StratumSource for TestStratum {
        fn stratum(&self) -> Option<StratumHandle> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A stratum server on a loopback port with [`FakeTemplate`], its sink and its source.
    pub(crate) async fn server() -> (StratumServer, Arc<FakeSink>, Arc<TestStratum>) {
        let sink = Arc::new(FakeSink::default());
        let config = StratumServerConfig {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            ..StratumServerConfig::default()
        };
        let srv = StratumServer::bind(config, Arc::new(FakeTemplate), sink.clone())
            .await
            .unwrap();
        let source = Arc::new(TestStratum(Mutex::new(Some(srv.handle()))));
        (srv, sink, source)
    }

    /// Hashes nonces from 0 until one meets `target`; returns the nonce and the hash.
    pub(crate) fn mine(blob_hex: &str, target: u64) -> (u32, [u8; 32]) {
        let mut blob = hex::decode(blob_hex).unwrap();
        let off = xkr_stratum::blob::nonce_offset(&blob);
        for nonce in 0u32.. {
            blob[off..off + 4].copy_from_slice(&nonce.to_le_bytes());
            let h = xkr_crypto::cn_turtle_lite_slow_hash_v2(&blob).0;
            if u64::from_le_bytes(h[24..].try_into().unwrap()) < target {
                return (nonce, h);
            }
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::sync::{broadcast, mpsc, watch};
    use tokio::task::JoinHandle;
    use xkr_chain::{AddBlockOutcome, Chain, ChainConfig, MinerAddress, set_merge_mining_tag};
    use xkr_hyper_rpc::{Client, ClientError, MAX_FRAME, read_frame, write_frame};
    use xkr_rpc::{MiningStatus, NetStatus, NodeInfo};
    use xkr_txpool::{PoolConfig, TxPool};
    use xkr_types::RawBlock;

    use super::*;

    const WAIT: Duration = Duration::from_secs(30);

    /// A testnet chain in a temporary directory, removed on drop.
    struct TestChain {
        dir: PathBuf,
        chain: Option<Arc<Chain>>,
    }

    impl TestChain {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkrd-clients-{name}-{}", std::process::id()));
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

        fn chain(&self) -> &Arc<Chain> {
            self.chain.as_ref().unwrap()
        }

        /// A block on the tip at the testnet's difficulty 1 (every nonce is a solution), as
        /// the blob `submitblock` takes.
        fn mined_blob(&self, miner: &MinerAddress) -> Vec<u8> {
            let t = self.chain().block_template(miner, &[]).unwrap();
            let mut block = t.block.clone();
            set_merge_mining_tag(&mut block).unwrap();
            xkr_serialization::encode_block(&block).unwrap()
        }

        fn mine(&self, miner: &MinerAddress) {
            let blob = self.mined_blob(miner);
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
    }

    impl Drop for TestChain {
        fn drop(&mut self) {
            drop(self.chain.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn miner() -> MinerAddress {
        xkr_rpc::parse_miner_address(crate::fee::DEFAULT_FEE_ADDRESS).unwrap()
    }

    /// The node around a test chain: its RPC state (no P2P layer: `submitblock` adds blocks
    /// directly), the status sender and the clients server.
    struct Node {
        tc: TestChain,
        net: watch::Sender<Arc<NetStatus>>,
        stats: Arc<ClientStats>,
        server: Arc<ClientsServer>,
    }

    fn node(
        name: &str,
        p2pool: Option<Arc<test_stratum::TestStratum>>,
        max_clients: usize,
        limits: ServerLimits,
    ) -> Node {
        let tc = TestChain::new(name);
        for _ in 0..3 {
            tc.mine(&miner());
        }
        let (net, net_rx) = watch::channel(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
        let (commands, commands_rx) = mpsc::unbounded_channel();
        drop(commands_rx);
        let stats = Arc::new(ClientStats::default());
        let rpc = Arc::new(RpcState {
            chain: tc.chain().clone(),
            net: net_rx,
            commands,
            mining,
            hugin: None,
            p2pool: None,
            clients: Some(stats.clone()),
            explorer: false,
            info: NodeInfo {
                version: "clients-test".into(),
                start_time: 0,
                data_dir: tc.dir.clone(),
                db_path: tc.dir.join("chain.redb"),
                fee_address: String::new(),
                fee_amount: 0,
            },
        });
        let server = ClientsServer::with_limits(
            rpc,
            stats.clone(),
            Network::Testnet,
            max_clients,
            p2pool.map(|p| p as Arc<dyn StratumSource>),
            limits,
        );
        Node {
            tc,
            net,
            stats,
            server,
        }
    }

    fn key(i: u8) -> Key {
        [i; 32]
    }

    /// Admits `key` and serves the server end of an in-memory stream; returns the client end.
    fn open(
        server: &Arc<ClientsServer>,
        k: Key,
    ) -> (tokio::io::DuplexStream, JoinHandle<ServeEnd>) {
        let (client_end, server_end) = tokio::io::duplex(4 * MAX_FRAME);
        let slot = server.admit(&k, None, Instant::now()).expect("admitted");
        let task = tokio::spawn(server.clone().serve(slot, k, server_end, Door::Clients));
        (client_end, task)
    }

    async fn connect(server: &Arc<ClientsServer>, k: Key, service: Service) -> Client {
        let (stream, _task) = open(server, k);
        Client::connect(stream, Hello::new("testnet", service, "clients-test/1"))
            .await
            .expect("hello accepted")
    }

    fn rpc_error(r: Result<Value, ClientError>) -> RpcError {
        match r {
            Err(ClientError::Rpc(e)) => e,
            other => panic!("not an error answer: {other:?}"),
        }
    }

    /// Serves `door` over an in-memory stream and connects with `service`.
    async fn connect_door(
        server: &Arc<ClientsServer>,
        k: Key,
        door: Door,
        service: Service,
    ) -> Result<Client, ClientError> {
        let (client_end, server_end) = tokio::io::duplex(4 * MAX_FRAME);
        let slot = server.admit(&k, None, Instant::now()).expect("admitted");
        tokio::spawn(server.clone().serve(slot, k, server_end, door));
        Client::connect(client_end, Hello::new("testnet", service, "wallet-test/1")).await
    }

    /// Opens wallet connections to a test node's wallets door.
    struct WalletDoor(Arc<ClientsServer>, std::sync::atomic::AtomicU8);

    impl xkr_daemon_client::ClientSource for WalletDoor {
        fn connect(
            &self,
        ) -> BoxFuture<'static, Result<(Client, String), xkr_daemon_client::TransportError>>
        {
            let k = [self.1.fetch_add(1, Ordering::Relaxed); 32];
            let server = self.0.clone();
            async move {
                connect_door(&server, k, Door::Wallets, Service::Wallet)
                    .await
                    .map(|c| (c, "test node".to_owned()))
                    .map_err(|e| xkr_daemon_client::TransportError::Connection(e.to_string()))
            }
            .boxed()
        }
    }

    /// A wallet's daemon client over the wallets topic answers and syncs as over HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wallet_daemon_client_works_over_the_wallets_topic() {
        use xkr_daemon_client::{DaemonClient, SwarmTransport, WalletSyncRequest};
        let n = node("walletclient", None, 8, ServerLimits::default());
        let tip = n.tc.chain().tip();
        let source = Arc::new(WalletDoor(n.server.clone(), 10.into()));
        let daemon = DaemonClient::new(SwarmTransport::new(source));
        assert_eq!(daemon.info().await.unwrap().height, tip.height + 1);
        let blocks = daemon
            .wallet_sync_data(&WalletSyncRequest {
                checkpoints: Vec::new(),
                start_height: 0,
                start_timestamp: 0,
                block_count: Some(3),
            })
            .await
            .unwrap();
        assert_eq!(blocks.len(), 3);
    }

    /// The wallets topic offers only `wallet`; its `http` method answers the allowlisted daemon
    /// paths as the HTTP server does and refuses the others.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wallet_service_answers_daemon_requests() {
        let n = node("wallet", None, 8, ServerLimits::default());
        let tip = n.tc.chain().tip();

        let reply = n.server.hello_reply(Door::Wallets);
        assert_eq!(reply.services, vec!["wallet".to_owned()]);
        assert!(
            connect_door(&n.server, [1; 32], Door::Wallets, Service::Node)
                .await
                .is_err()
        );
        let private = n.server.hello_reply(Door::Private);
        assert!(private.services.contains(&"wallet".to_owned()));
        assert!(private.services.contains(&"node".to_owned()));

        let c = connect_door(&n.server, [2; 32], Door::Wallets, Service::Wallet)
            .await
            .expect("wallet hello");
        let info = c
            .call(WALLET_HTTP, json!({"method": "GET", "path": "/info"}))
            .await
            .unwrap();
        assert_eq!(info["status"], 200);
        assert_eq!(info["body"]["height"], tip.height + 1);

        let sync = c
            .call(
                WALLET_HTTP,
                json!({"method": "POST", "path": "/getwalletsyncdata",
                       "body": {"startHeight": 0, "blockCount": 2, "blockHashCheckpoints": []}}),
            )
            .await
            .unwrap();
        assert_eq!(sync["status"], 200);
        assert_eq!(sync["body"]["items"].as_array().unwrap().len(), 2);

        let count = c
            .call(
                WALLET_HTTP,
                json!({"method": "POST", "path": "/json_rpc",
                       "body": {"jsonrpc": "2.0", "id": 1, "method": "getblockcount"}}),
            )
            .await
            .unwrap();
        assert_eq!(count["body"]["result"]["count"], tip.height + 1);

        for params in [
            json!({"method": "GET", "path": "/status"}),
            json!({"method": "GET", "path": "/peers"}),
            json!({"method": "POST", "path": "/json_rpc", "body": {"method": "submitblock"}}),
            json!({"method": "GET", "path": "/info?a=1?b=2"}),
        ] {
            assert_eq!(
                rpc_error(c.call(WALLET_HTTP, params.clone()).await).code,
                code::METHOD_NOT_FOUND,
                "{params}"
            );
        }
        assert_eq!(
            rpc_error(
                c.call(WALLET_HTTP, json!({"method": "PUT", "path": "/info"}))
                    .await
            )
            .code,
            code::INVALID_PARAMS
        );
        assert_eq!(
            rpc_error(c.call("getinfo", json!({})).await).code,
            code::METHOD_NOT_FOUND
        );
    }

    /// Hello, every allowlisted method, then a block submitted over the topic comes back as a
    /// tip push.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_node_service_answers_its_methods_and_pushes_the_tip() {
        let n = node("node", None, 4, ServerLimits::default());
        let chain = n.tc.chain().clone();
        let tip = chain.tip();
        let client = connect(&n.server, key(1), Service::Node).await;
        let hello = client.hello().clone();
        assert_eq!(hello.v, 1);
        assert_eq!(hello.height, tip.height);
        assert_eq!(hello.hash, tip.hash.to_string());
        assert!(hello.synced);
        assert_eq!(hello.services, ["node"]);
        assert_eq!(
            (hello.state_span, hello.state_hash.as_deref()),
            (None, None),
            "no stable span on a short chain"
        );
        assert_eq!(hello.pruning_seed, 0);
        assert_eq!(n.stats.snapshot().connections, 1);

        let info = client.call("getinfo", json!({})).await.unwrap();
        assert_eq!(info["height"], tip.height + 1);
        assert_eq!(info["status"], "OK");

        let one = client
            .call("getblockheaderbyheight", json!({ "height": 1 }))
            .await
            .unwrap();
        assert_eq!(one["block_header"]["height"], 1);
        let hash = one["block_header"]["hash"].as_str().unwrap().to_owned();
        let by_hash = client
            .call("getblockheaderbyhash", json!({ "hash": hash }))
            .await
            .unwrap();
        assert_eq!(by_hash["block_header"]["height"], 1);

        let range = client
            .call(
                "get_block_headers_range",
                json!({ "start_height": 0, "end_height": tip.height }),
            )
            .await
            .unwrap();
        assert_eq!(
            range["headers"].as_array().unwrap().len() as u64,
            tip.height + 1
        );

        let data = client.call("get_miner_data", json!({})).await.unwrap();
        assert_eq!(data["height"], tip.height + 1);
        assert_eq!(data["prev_id"], tip.hash.to_string());

        // a method's own errors come through with their codes
        let e = rpc_error(client.call("getstatehash", json!({ "span": 0 })).await);
        assert_eq!(e.code, -2, "{e:?}");
        let e = rpc_error(
            client
                .call("getblockheaderbyheight", json!({ "height": 99 }))
                .await,
        );
        assert_eq!(e.code, -2, "{e:?}");

        let mut tips = client.tips();
        let blob = n.tc.mined_blob(&miner());
        let ok = client
            .call("submitblock", json!([hex::encode(blob)]))
            .await
            .unwrap();
        assert_eq!(ok["status"], "OK");
        tokio::time::timeout(WAIT, tips.changed())
            .await
            .expect("a tip push")
            .unwrap();
        let pushed = tips.borrow_and_update().clone();
        let now = chain.tip();
        assert_eq!(now.height, tip.height + 1);
        assert_eq!(pushed.height, now.height);
        assert_eq!(pushed.hash, now.hash.to_string());
        assert!(pushed.synced);
        // a block from elsewhere is pushed too, with the sync state of the moment
        n.net.send_replace(Arc::new(NetStatus::default()));
        n.tc.mine(&miner());
        tokio::time::timeout(WAIT, tips.changed())
            .await
            .expect("a tip push")
            .unwrap();
        let pushed = tips.borrow_and_update().clone();
        assert_eq!(pushed.height, tip.height + 2);
        assert!(!pushed.synced);

        assert_eq!(n.stats.snapshot().served_calls, 8);
        drop(client);
        tokio::time::timeout(WAIT, async {
            while n.stats.snapshot().connections > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the place is freed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn methods_outside_the_allowlist_are_refused() {
        let n = node("allowlist", None, 4, ServerLimits::default());
        let client = connect(&n.server, key(1), Service::Node).await;
        for method in [
            "getblocktemplate",
            "getblockcount",
            "get_info",
            "getlastblockheader",
            "on_getblockhash",
            "f_blocks_list_json",
            "sendrawtransaction",
            "/getinfo",
            "",
        ] {
            let e = rpc_error(client.call(method, json!({})).await);
            assert_eq!(e.code, code::METHOD_NOT_FOUND, "{method}");
        }
        assert_eq!(n.stats.snapshot().served_calls, 0);
        assert!(client.call("getinfo", json!({})).await.is_ok());
        assert_eq!(n.stats.snapshot().served_calls, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stratum_is_offered_only_while_p2pool_runs() {
        let refused = |r: Result<Client, ClientError>| match r {
            Err(ClientError::Refused { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };
        // p2pool off, and p2pool on but not running yet
        for p2pool in [None, Some(Arc::default())] {
            let on = p2pool.is_some();
            let n = node("no-stratum", p2pool, 4, ServerLimits::default());
            let (stream, _task) = open(&n.server, key(1));
            let r = Client::connect(stream, Hello::new("testnet", Service::Stratum, "t")).await;
            assert_eq!(refused(r), "service", "{on}");
            let client = connect(&n.server, key(2), Service::Node).await;
            assert_eq!(client.hello().services, ["node"]);
            let e = rpc_error(client.call(STRATUM_OPEN, json!({})).await);
            let want = if on {
                code::UNAVAILABLE
            } else {
                code::METHOD_NOT_FOUND
            };
            assert_eq!(e.code, want, "{on}");
        }
        let (_srv, _sink, source) = test_stratum::server().await;
        let n = node("stratum", Some(source), 4, ServerLimits::default());
        let client = connect(&n.server, key(1), Service::Stratum).await;
        assert_eq!(client.hello().services, ["node", "stratum"]);
        // the stratum service answers the stratum methods only
        let e = rpc_error(client.call("getinfo", json!({})).await);
        assert_eq!(e.code, code::METHOD_NOT_FOUND);
        let e = rpc_error(client.call("login", json!({})).await);
        assert_eq!(e.code, code::METHOD_NOT_FOUND);
        // wallets have a topic of their own; other networks are refused
        let (stream, _task) = open(&n.server, key(2));
        let r = Client::connect(stream, Hello::new("testnet", Service::Wallet, "t")).await;
        assert_eq!(refused(r), "service");
        let (stream, _task) = open(&n.server, key(3));
        let r = Client::connect(stream, Hello::new("mainnet", Service::Node, "t")).await;
        assert_eq!(refused(r), "network");
    }

    /// The next push `method` of stratum session `session`: its params.
    async fn stratum_push(
        pushes: &mut broadcast::Receiver<Push>,
        method: &str,
        session: u64,
    ) -> Value {
        tokio::time::timeout(WAIT, async {
            loop {
                let p = pushes.recv().await.expect("pushes");
                if p.method == method && p.params["session"] == session {
                    return p.params;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {method} push"))
    }

    /// Service `stratum`: a session in the node's p2pool stratum server over the clients topic.
    /// Login (answered with a job), a share accepted into the sidechain, a pushed job, the
    /// message checks, the close; the methods on a `node` connection; a session that breaks the
    /// stratum rules ends and its key is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stratum_sessions_reach_the_stratum_server() {
        let (srv, sink, source) = test_stratum::server().await;
        let n = node("stratum-e2e", Some(source), 4, ServerLimits::default());
        let client = connect(&n.server, key(1), Service::Stratum).await;
        let mut pushes = client.pushes();
        let login = json!({"id": 1, "method": "login",
            "params": {"login": "SEKRrelay", "pass": "x", "agent": "clients-test"}});
        let e = rpc_error(
            client
                .call(STRATUM_MESSAGE, json!({"session": 1, "message": login}))
                .await,
        );
        assert_eq!(e.code, code::UNAVAILABLE, "no session yet");

        let session = client.call(STRATUM_OPEN, json!({})).await.unwrap()["session"]
            .as_u64()
            .unwrap();
        client
            .call(
                STRATUM_MESSAGE,
                json!({"session": session, "message": login}),
            )
            .await
            .unwrap();
        let answer = stratum_push(&mut pushes, STRATUM_MESSAGE, session).await["message"].clone();
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["status"], "OK");
        let job = answer["result"]["job"].clone();
        let rpc_id = answer["result"]["id"].as_str().unwrap().to_owned();
        assert_eq!(srv.connections(), 1);

        let target = xkr_stratum::target::parse_target(job["target"].as_str().unwrap()).unwrap();
        let blob = job["blob"].as_str().unwrap().to_owned();
        let (nonce, hash) = tokio::task::spawn_blocking(move || test_stratum::mine(&blob, target))
            .await
            .unwrap();
        let submit = json!({"id": 2, "method": "submit", "params": {"id": rpc_id,
            "job_id": job["job_id"], "nonce": hex::encode(nonce.to_le_bytes()),
            "result": hex::encode(hash)}});
        client
            .call(
                STRATUM_MESSAGE,
                json!({"session": session, "message": submit}),
            )
            .await
            .unwrap();
        let answer = stratum_push(&mut pushes, STRATUM_MESSAGE, session).await["message"].clone();
        assert_eq!(answer["id"], 2);
        assert!(answer["error"].is_null(), "{answer}");
        assert_eq!(answer["result"]["status"], "OK");
        assert_eq!(sink.shares.lock().unwrap().len(), 1);

        // a new template reaches the session as a pushed job
        srv.on_block();
        let job_message =
            stratum_push(&mut pushes, STRATUM_MESSAGE, session).await["message"].clone();
        assert_eq!(job_message["method"], "job");

        // the checks of a message
        for (params, want) in [
            (
                json!({"session": session, "message": [1, 2]}),
                code::INVALID_PARAMS,
            ),
            (json!({"message": login}), code::INVALID_PARAMS),
            (
                json!({"session": session, "message": {"id": 3, "method": "x".repeat(STRATUM_BUF_SIZE)}}),
                code::INVALID_PARAMS,
            ),
            (
                json!({"session": session + 1, "message": login}),
                code::UNAVAILABLE,
            ),
        ] {
            let e = rpc_error(client.call(STRATUM_MESSAGE, params.clone()).await);
            assert_eq!(e.code, want, "{params}");
        }

        // closed: the end is job_message and the stratum server forgets the miner
        client
            .call(STRATUM_CLOSE, json!({"session": session}))
            .await
            .unwrap();
        stratum_push(&mut pushes, STRATUM_CLOSED, session).await;
        tokio::time::timeout(WAIT, async {
            while srv.connections() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the session is gone");

        // the same methods on the connection a chainless xkrd has for the chain
        let on_node = connect(&n.server, key(2), Service::Node).await;
        let mut node_pushes = on_node.pushes();
        let session = on_node.call(STRATUM_OPEN, json!({})).await.unwrap()["session"]
            .as_u64()
            .unwrap();
        on_node
            .call(
                STRATUM_MESSAGE,
                json!({"session": session, "message": login}),
            )
            .await
            .unwrap();
        let answer =
            stratum_push(&mut node_pushes, STRATUM_MESSAGE, session).await["message"].clone();
        assert_eq!(answer["result"]["status"], "OK");
        // a new session ends the one before
        let next = on_node.call(STRATUM_OPEN, json!({})).await.unwrap()["session"]
            .as_u64()
            .unwrap();
        assert_ne!(next, session);
        stratum_push(&mut node_pushes, STRATUM_CLOSED, session).await;

        // a message the stratum server does not take (no id) ends the session; the
        // connection gets no new one and its key is refused
        on_node
            .call(
                STRATUM_MESSAGE,
                json!({"session": next, "message": {"method": "login"}}),
            )
            .await
            .unwrap();
        stratum_push(&mut node_pushes, STRATUM_CLOSED, next).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let e = rpc_error(on_node.call(STRATUM_OPEN, json!({})).await);
        assert_eq!(e.code, code::UNAVAILABLE);
        assert!(
            on_node.call("getinfo", json!({})).await.is_ok(),
            "the node service goes on"
        );
        assert_eq!(
            n.server.admit(&key(2), None, Instant::now()).unwrap_err(),
            NotAdmitted::Banned
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_are_capped_per_node_and_per_address() {
        let n = node("caps", None, 2, ServerLimits::default());
        let now = Instant::now();
        let a = n.server.admit(&key(1), None, now).unwrap();
        let b = n.server.admit(&key(2), None, now).unwrap();
        assert_eq!(n.stats.snapshot().connections, 2);
        assert_eq!(
            n.server.admit(&key(3), None, now).unwrap_err(),
            NotAdmitted::Full
        );
        drop(a);
        let c = n.server.admit(&key(3), None, now).unwrap();
        drop((b, c));
        assert_eq!(n.stats.snapshot().connections, 0);

        let n = node("per-ip", None, 100, ServerLimits::default());
        let addr: SocketAddr = "203.0.113.7:4000".parse().unwrap();
        let other: SocketAddr = "203.0.113.8:4000".parse().unwrap();
        let v6 = |port| -> SocketAddr { format!("[2001:db8::{port}]:{port}").parse().unwrap() };
        let slots: Vec<Slot> = (0..MAX_CLIENTS_PER_IP)
            .map(|i| n.server.admit(&key(i as u8), Some(addr), now).unwrap())
            .collect();
        assert_eq!(
            n.server.admit(&key(99), Some(addr), now).unwrap_err(),
            NotAdmitted::PerIp
        );
        assert!(n.server.admit(&key(99), Some(other), now).is_ok());
        let v6_slots: Vec<Slot> = (1..=MAX_CLIENTS_PER_IP as u16)
            .map(|p| n.server.admit(&key(50), Some(v6(p)), now).unwrap())
            .collect();
        assert_eq!(
            n.server.admit(&key(51), Some(v6(99)), now).unwrap_err(),
            NotAdmitted::PerIp,
            "one /64"
        );
        drop(slots);
        assert!(n.server.admit(&key(99), Some(addr), now).is_ok());
        drop(v6_slots);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_breaks_the_protocol_is_banned() {
        let n = node("ban", None, 4, ServerLimits::default());
        let (stream, task) = open(&n.server, key(7));
        let (mut rd, mut wr) = tokio::io::split(stream);
        let hello = serde_json::to_vec(&Hello::new("testnet", Service::Node, "t")).unwrap();
        write_frame(&mut wr, &hello, MAX_FRAME).await.unwrap();
        assert!(read_frame(&mut rd, MAX_FRAME).await.unwrap().is_some());
        write_frame(&mut wr, b"[1,2,3]", MAX_FRAME).await.unwrap();
        let end = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        assert!(end.is_misbehaviour(), "{end}");
        let now = Instant::now();
        assert_eq!(
            n.server.admit(&key(7), None, now).unwrap_err(),
            NotAdmitted::Banned
        );
        assert!(n.server.admit(&key(8), None, now).is_ok(), "only that key");
        assert!(
            n.server
                .admit(&key(7), None, now + BAN + Duration::from_secs(1))
                .is_ok(),
            "the ban ends"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_connection_limits_apply() {
        let n = node(
            "over limits",
            None,
            4,
            ServerLimits {
                rate_per_sec: 1,
                burst: 2,
                max_refused_in_a_row: 2,
                ..ServerLimits::default()
            },
        );
        let (stream, task) = open(&n.server, key(9));
        let client = Client::connect(stream, Hello::new("testnet", Service::Node, "t"))
            .await
            .unwrap();
        assert!(client.call("getinfo", json!({})).await.is_ok());
        assert!(client.call("getinfo", json!({})).await.is_ok());
        let e = rpc_error(client.call("getinfo", json!({})).await);
        assert_eq!(e.code, code::RATE_LIMITED);
        let _ = client.call("getinfo", json!({})).await;
        assert!(matches!(
            client.call("getinfo", json!({})).await,
            Err(ClientError::Closed | ClientError::Rpc(_))
        ));
        let end = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
        assert!(matches!(end, ServeEnd::TooManyRefused), "{end}");
        assert_eq!(
            n.server.admit(&key(9), None, Instant::now()).unwrap_err(),
            NotAdmitted::Banned
        );
    }
}
