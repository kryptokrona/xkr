//! Test support: an in-process xkrd (the `xkr-rpc` router on a fresh testnet chain, as
//! `testnet/fixtures` runs xkrd "X"), blocks mined like `xkr-testwallet mine`
//! (`getblocktemplate`, merge-mining tag, `submitblock`), and wallet-api instances whose wallets
//! talk to that node in process.
#![allow(dead_code)] // each test file uses a different subset

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig, set_merge_mining_tag};
use xkr_consensus::Network;
use xkr_daemon_client::{
    DaemonClient, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_serialization::{decode_block, encode_block};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_wallet::{FileFormat, Node, SyncConfig, WalletConfig, WalletError};
use xkr_wallet_api::{ApiConfig, App, NodeFactory, router};

/// `X-API-KEY` of the test servers (`FX_API_KEY` of `testnet/fixtures/lib-wallet.sh`).
pub const KEY: &str = "fixture-api-key";
/// `--enable-cors` of the CORS test server (`FX_CORS`).
pub const CORS: &str = "https://fixture.example";
/// The daemon port the fixture requests name (`PORT_FX_RPC`).
pub const FX_DAEMON_PORT: u16 = 24899;

// fixed key material of `testnet/fixtures/lib-wallet.sh`
pub const SEED_A: &str = "skulls woozy ouch summon gifts huts waffle ourselves obtains hexagon tadpoles hacksaw dormant hence abort listen history atom cadets stylishly snout vegan girth guest history";
pub const A_SPEND: &str = "d61a57a59318d70ff77cc7f8ad7f62887c828da1d5d3f3b0d2f7d3fa596c2904";
pub const A_VIEW: &str = "688e5229df6463ec4c27f6ee11c3f1d3d4b4d2480c0aabe64fb807182cfdc801";
pub const SEED_B: &str = "nugget lazy gang sonic vulture exit veteran poverty affair ringing opus soapy sonic afield dating lectures worry tuxedo ruffled rated locker bested aunt bifocals opus";
pub const K_SPEND: &str = "55e0aa4ca65c0ae016c7364eec313f56fc162901ead0e38a9f846686ac78560f";
pub const K_VIEW: &str = "1f3f6c220dd9f97619dbf44d967f79f3041b9b1c63da2c895f980f1411d5d704";
pub const DUMMY_ADDR: &str = "SEKReTRGXxc41SGrKyw5ucHKoM4nhMYazTwseWGB181H9zRB68oSApmjSaUS8yPjgEGEnH4WesJEW1zaCmEB5ykSLQZvfs7CFTU";
pub const PID1: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
pub const UNKNOWN_HASH: &str = "abababababababababababababababababababababababababababababababab";

// ---------------------------------------------------------------------------------------------
// the node

/// xkrd's RPC on a fresh testnet chain (genesis only), in process.
pub struct TestNode {
    pub state: Arc<RpcState>,
    pub router: Router,
    _dir: tempfile::TempDir,
}

impl Default for TestNode {
    fn default() -> Self {
        Self::new()
    }
}

impl TestNode {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = xkr_storage::Store::open(dir.path().join("chain.redb")).expect("store");
        let pool = Arc::new(TxPool::new(PoolConfig::default()));
        let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).expect("chain");
        let (_net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
        // no receiver: submitted blocks go straight to the chain
        let (commands, _commands_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RpcState {
            chain: Arc::new(chain),
            net,
            commands,
            mining,
            hugin: None,
            p2pool: None,
            clients: None,
            explorer: true,
            info: NodeInfo {
                version: "xkr-wallet-api-tests".into(),
                start_time: 0,
                data_dir: dir.path().to_path_buf(),
                db_path: dir.path().join("chain.redb"),
                fee_address: String::new(),
                fee_amount: 0,
            },
        });
        let router = xkr_rpc::router(Arc::clone(&state), &[]);
        Self {
            state,
            router,
            _dir: dir,
        }
    }

    /// Index of the top block.
    pub fn top(&self) -> u64 {
        self.state.chain.tip().height
    }

    /// A JSON-RPC call; the `result`.
    pub async fn json_rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 0, "method": method, "params": params });
        let req = Request::builder()
            .method("POST")
            .uri("/json_rpc")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let Ok(resp) = self.router.clone().oneshot(req).await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let answer: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "{method}: HTTP {status}, not JSON ({e}): {:?}",
                String::from_utf8_lossy(&bytes)
            )
        });
        assert!(answer.get("error").is_none(), "{method}: {answer}");
        answer["result"].clone()
    }

    /// One block paying `address`, with what the pool offers.
    pub async fn mine_block(&self, address: &str) {
        let template = self
            .json_rpc(
                "getblocktemplate",
                json!({ "wallet_address": address, "reserve_size": 0 }),
            )
            .await;
        let blob =
            hex::decode(template["blocktemplate_blob"].as_str().expect("blob")).expect("hex");
        let mut block = decode_block(&blob).expect("template decodes");
        set_merge_mining_tag(&mut block).expect("merge-mining tag");
        let blob = encode_block(&block).expect("block encodes");
        let result = self
            .json_rpc("submitblock", json!([hex::encode(blob)]))
            .await;
        assert_eq!(result["status"], "OK", "submitblock: {result}");
    }

    /// Blocks paying `address` until the top block index is `top` (`fx_mine`).
    pub async fn mine_to(&self, address: &str, top: u64) {
        while self.top() < top {
            self.mine_block(address).await;
        }
    }

    /// `count` blocks paying `address`.
    pub async fn mine(&self, address: &str, count: u64) {
        let top = self.top() + count;
        self.mine_to(address, top).await;
    }

    /// Transactions waiting in the pool.
    pub fn pool_len(&self) -> usize {
        self.state.chain.pool().len()
    }
}

/// Hands daemon requests straight to the router.
#[derive(Debug, Clone)]
pub struct RouterTransport(pub Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = Request::builder()
            .method(method)
            .uri(&request.path)
            .header("content-type", "application/json")
            .body(Body::from(request.body))
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        let Ok(resp) = self.0.clone().oneshot(req).await;
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }
}

/// Every `daemonHost:daemonPort` reaches the in-process node; `GET /node` still reports the
/// host and port the request gave.
#[derive(Debug, Clone)]
pub struct RouterNodes(pub Router);

impl NodeFactory for RouterNodes {
    type Transport = RouterTransport;

    fn connect(&self, host: &str, port: u16) -> Result<Node<RouterTransport>, WalletError> {
        Ok(Node::new(
            DaemonClient::new(RouterTransport(self.0.clone())),
            host,
            port,
        ))
    }
}

// ---------------------------------------------------------------------------------------------
// the API

/// Sync timing for tests.
pub fn fast_sync() -> SyncConfig {
    SyncConfig {
        poll_interval: Duration::from_millis(20),
        info_interval: Duration::from_millis(20),
        block_count: 100,
        queue_blocks: 1000,
    }
}

/// An answer of the API.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: HeaderMap,
    pub text: String,
}

impl Answer {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.text))
    }

    /// A response header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .map(|v| v.to_str().expect("ascii header"))
    }

    /// `errorCode` of an error body.
    pub fn error_code(&self) -> u64 {
        assert_eq!(self.status, 400, "{self:?}");
        self.json()["errorCode"].as_u64().expect("errorCode")
    }
}

/// A wallet-api server in process.
pub struct Api {
    pub app: Arc<App<RouterNodes>>,
    pub router: Router,
}

impl Api {
    /// A server whose wallets use `node`; `cors` is `--enable-cors`.
    pub fn new(node: &TestNode, cors: Option<&str>) -> Self {
        let config = ApiConfig {
            rpc_password: KEY.to_owned().into(),
            cors: cors.map(str::to_owned),
            default_daemon_port: Network::Testnet.params().rpc_port,
            wallet: WalletConfig {
                network: Network::Testnet,
                format: FileFormat::WalletBackend,
                sync: fast_sync(),
                start_sync: true,
            },
        };
        let app = App::new(config, RouterNodes(node.router.clone()));
        let router = router(Arc::clone(&app));
        Self { app, router }
    }

    /// Any request.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
            .expect("request");
        let Ok(resp) = self.router.clone().oneshot(req).await;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        Answer {
            status,
            headers,
            text: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
        }
    }

    /// A request with the right key; a JSON body if given.
    pub async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Answer {
        let text = body.map(Value::to_string);
        let mut headers = vec![("X-API-KEY", KEY)];
        if text.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        self.request(method, path, &headers, text.as_deref()).await
    }

    pub async fn get(&self, path: &str) -> Answer {
        self.call("GET", path, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Answer {
        self.call("POST", path, Some(body)).await
    }

    /// `fx_api_synced`: the open wallet has scanned what the node has.
    pub async fn synced(&self, node: &TestNode) -> bool {
        let s = self.get("/status").await;
        if s.status != 200 {
            return false;
        }
        let s = s.json();
        let (w, n, l) = (
            s["walletBlockCount"].as_u64().unwrap_or(0),
            s["networkBlockCount"].as_u64().unwrap_or(0),
            s["localDaemonBlockCount"].as_u64().unwrap_or(0),
        );
        l >= node.top() && w >= n && w >= l
    }

    /// Waits (at most 120 s) until [`Api::synced`].
    pub async fn wait_synced(&self, node: &TestNode) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        while !self.synced(node).await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the wallet did not reach the node's top {}: {}",
                node.top(),
                self.get("/status").await.text
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The parameters of open, import and create (`fx_wparams`): a file in `dir`.
pub fn wallet_params(dir: &Path, file: &str, password: &str, extra: &Value) -> Value {
    let mut v = json!({
        "filename": dir.join(file).to_string_lossy(),
        "password": password,
        "daemonHost": "127.0.0.1",
        "daemonPort": FX_DAEMON_PORT,
    });
    if let (Some(map), Some(more)) = (v.as_object_mut(), extra.as_object()) {
        for (k, value) in more {
            map.insert(k.clone(), value.clone());
        }
    }
    v
}
