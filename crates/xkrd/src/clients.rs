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
//! * service `stratum`: listed only while the built-in p2pool runs; for now its calls are
//!   answered with "not available yet".
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
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Semaphore, watch};
use xkr_consensus::Network;
use xkr_hyper_rpc::{
    Accepted, Handler, Hello, HelloReply, Push, RefuseReason, Refused, RpcError, ServeEnd,
    ServerLimits, Service, Tip, VERSION, code,
};
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

/// Client connections from one IP address (IPv6: one /64).
pub const MAX_CLIENTS_PER_IP: usize = 8;

/// Calls of all clients that read storage at once.
const MAX_BLOCKING_CALLS: usize = 16;

/// How long a misbehaving client's key is refused.
pub const BAN: Duration = Duration::from_mins(10);

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
    /// `true` while the built-in p2pool runs (`None`: p2pool off).
    p2pool: Option<watch::Receiver<bool>>,
    blocking: Arc<Semaphore>,
    book: Mutex<Book>,
}

/// An admitted connection's place; dropping it frees the place.
#[derive(Debug)]
pub struct Slot {
    server: Arc<ClientsServer>,
    ip: Option<IpAddr>,
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
    /// The server over the node's RPC state; `p2pool` is the built-in p2pool's "running"
    /// switch (service `stratum` is listed while it is `true`).
    #[must_use]
    pub fn new(
        rpc: Arc<RpcState>,
        stats: Arc<ClientStats>,
        network: Network,
        max_clients: usize,
        p2pool: Option<watch::Receiver<bool>>,
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
        p2pool: Option<watch::Receiver<bool>>,
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
        })
    }

    /// Refuses `key` for [`BAN`] from `now`.
    pub fn ban(&self, key: Key, now: Instant) {
        let mut book = self.book();
        hyper::make_room(&mut book.bans, &key, |until| *until);
        book.bans.insert(key, now + BAN);
    }

    /// Serves an admitted connection until it ends; bans `key` if the client broke the
    /// protocol or its limits.
    pub async fn serve<S>(self: Arc<Self>, slot: Slot, key: Key, stream: S) -> ServeEnd
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let end = xkr_hyper_rpc::serve(stream, self.limits, |hello| self.accept(hello)).await;
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

    /// The services offered now.
    fn services(&self) -> Vec<Service> {
        let mut out = vec![Service::Node];
        if self.p2pool.as_ref().is_some_and(|r| *r.borrow()) {
            out.push(Service::Stratum);
        }
        out
    }

    /// The answer to an accepted hello: the tip, the sync state, the services, the newest
    /// stable state hash and the pruning seed.
    #[must_use]
    pub fn hello_reply(&self) -> HelloReply {
        let tip = self.rpc.chain.tip();
        let net = self.rpc.net.borrow().clone();
        let state = self.rpc.chain.state_view().latest();
        HelloReply {
            v: VERSION,
            height: tip.height,
            hash: tip.hash.to_string(),
            synced: hyper::synced_now(&net),
            services: self
                .services()
                .into_iter()
                .map(|s| s.as_str().to_owned())
                .collect(),
            state_span: state.map(|(span, _)| span),
            state_hash: state.map(|(_, hash)| hash.to_string()),
            pruning_seed: net.pruning_seed,
        }
    }

    fn accept(&self, hello: &Hello) -> Result<Accepted, Refused> {
        let network = hyper::xkr_network(self.network).as_str();
        if hello.network != network {
            return Err(Refused::new(
                RefuseReason::Network,
                format!("this node is on {network}"),
            ));
        }
        let offered = self.services();
        let Some(service) = Service::parse(&hello.service).filter(|s| offered.contains(s)) else {
            let names: Vec<&str> = offered.iter().map(|s| s.as_str()).collect();
            return Err(Refused::new(
                RefuseReason::Service,
                format!("this node serves {}", names.join(", ")),
            ));
        };
        // Subscribe before the reply is read, so no tip falls between the two.
        let (handler, pushes): (Arc<dyn Handler>, _) = match service {
            Service::Node => (
                Arc::new(NodeHandler {
                    rpc: self.rpc.clone(),
                    stats: self.stats.clone(),
                    blocking: self.blocking.clone(),
                }) as Arc<dyn Handler>,
                Some(self.tip_pushes()),
            ),
            Service::Stratum | Service::Wallet => {
                (Arc::new(NotYet { service }) as Arc<dyn Handler>, None)
            }
        };
        tracing::debug!(client = %hello.client, %service, "client on the clients topic");
        Ok(Accepted {
            reply: self.hello_reply(),
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

impl Handler for NodeHandler {
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

/// A service that is negotiated but not served yet (`stratum`).
struct NotYet {
    service: Service,
}

impl Handler for NotYet {
    fn call(&self, _method: String, _params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let error = RpcError::new(
            code::UNAVAILABLE,
            format!(
                "the {} service is not available on the clients topic yet",
                self.service
            ),
        );
        futures::future::ready(Err(error)).boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use tokio::sync::mpsc;
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
        _p2pool: Option<watch::Sender<bool>>,
    }

    fn node(name: &str, p2pool: Option<bool>, max_clients: usize, limits: ServerLimits) -> Node {
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
        let (p2pool_tx, p2pool_rx) = p2pool.map(watch::channel).unzip();
        let server = ClientsServer::with_limits(
            rpc,
            stats.clone(),
            Network::Testnet,
            max_clients,
            p2pool_rx,
            limits,
        );
        Node {
            tc,
            net,
            stats,
            server,
            _p2pool: p2pool_tx,
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
        let task = tokio::spawn(server.clone().serve(slot, k, server_end));
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
        for p2pool in [None, Some(false)] {
            let n = node("no-stratum", p2pool, 4, ServerLimits::default());
            let (stream, _task) = open(&n.server, key(1));
            let r = Client::connect(stream, Hello::new("testnet", Service::Stratum, "t")).await;
            assert_eq!(refused(r), "service", "{p2pool:?}");
            let client = connect(&n.server, key(2), Service::Node).await;
            assert_eq!(client.hello().services, ["node"]);
        }
        let n = node("stratum", Some(true), 4, ServerLimits::default());
        let client = connect(&n.server, key(1), Service::Stratum).await;
        assert_eq!(client.hello().services, ["node", "stratum"]);
        let e = rpc_error(client.call("login", json!({})).await);
        assert_eq!(e.code, code::UNAVAILABLE);
        // wallets have a topic of their own; other networks are refused
        let (stream, _task) = open(&n.server, key(2));
        let r = Client::connect(stream, Hello::new("testnet", Service::Wallet, "t")).await;
        assert_eq!(refused(r), "service");
        let (stream, _task) = open(&n.server, key(3));
        let r = Client::connect(stream, Hello::new("mainnet", Service::Node, "t")).await;
        assert_eq!(refused(r), "network");
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
