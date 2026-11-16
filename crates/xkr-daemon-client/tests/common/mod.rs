//! Test transports: [`RouterTransport`] hands requests to the `xkr-rpc` router in process (no
//! socket), [`Fake`] answers from a table (C++-shaped answers) and records what it was sent.
//! Also a genesis-only node for the router.
#![allow(dead_code)] // every test file uses a different subset

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig};
use xkr_consensus::Network;
use xkr_daemon_client::{HttpRequest, HttpResponse, Method, Transport, TransportError};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_txpool::{PoolConfig, TxPool};

/// Sends requests straight into an axum router.
#[derive(Debug, Clone)]
pub struct RouterTransport(pub Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = axum::http::Request::builder()
            .method(method)
            .uri(&request.path)
            .header("content-type", "application/json")
            .body(Body::from(request.body))
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        // The router's error type is `Infallible`.
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

/// Answers from a table keyed by path; a path without an answer is a connection failure.
/// Records every request.
#[derive(Debug, Default)]
pub struct Fake {
    answers: Mutex<HashMap<String, (u16, Vec<u8>)>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Fake {
    /// Answers `path` with `status` and the JSON `body` from now on.
    pub fn answer(&self, path: &str, status: u16, body: &Value) {
        self.answer_raw(path, status, &serde_json::to_vec(body).expect("json"));
    }

    /// Answers `path` with `status` and the raw `body` from now on.
    pub fn answer_raw(&self, path: &str, status: u16, body: &[u8]) {
        self.answers
            .lock()
            .expect("lock")
            .insert(path.to_owned(), (status, body.to_vec()));
    }

    /// The last request sent and its JSON body (`Null` when empty).
    pub fn last(&self) -> (Method, String, Value) {
        let requests = self.requests.lock().expect("lock");
        let r = requests.last().expect("a request was sent");
        let body = if r.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&r.body).expect("JSON request body")
        };
        (r.method, r.path.clone(), body)
    }
}

impl Transport for Fake {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
        std::future::ready(self.respond(request))
    }
}

impl Fake {
    fn respond(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let answer = self
            .answers
            .lock()
            .expect("lock")
            .get(&request.path)
            .cloned();
        let path = request.path.clone();
        self.requests.lock().expect("lock").push(request);
        let (status, body) =
            answer.ok_or_else(|| TransportError::Connection(format!("nothing at {path}")))?;
        Ok(HttpResponse { status, body })
    }
}

/// A database file under the temp dir, removed on drop.
#[derive(Debug)]
pub struct GenesisDb(PathBuf);

impl GenesisDb {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-daemon-client-test-{}-{}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for GenesisDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A mainnet node with only the genesis block and the node fee `fee`.
pub fn genesis_node(db: &GenesisDb, fee: (&str, u32)) -> Arc<RpcState> {
    let store = xkr_storage::Store::open(&db.0).expect("store");
    let chain = Chain::open(
        store,
        ChainConfig::new(Network::Mainnet),
        Arc::new(TxPool::new(PoolConfig::default())),
    )
    .expect("chain");
    let (_net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
    let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
    let (commands, _commands_rx) = mpsc::unbounded_channel();
    Arc::new(RpcState {
        chain: Arc::new(chain),
        net,
        commands,
        mining,
        hugin: None,
        p2pool: None,
        info: NodeInfo {
            version: "daemon-client-test".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: fee.0.to_owned(),
            fee_amount: fee.1,
        },
    })
}

/// The fork-14 wallet-sync vector (`docs/test-vectors/tx-v2-walletsync.json`).
pub fn walletsync_fixture() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/test-vectors/tx-v2-walletsync.json");
    let text = std::fs::read_to_string(&path).expect("fixture E");
    serde_json::from_str(&text).expect("fixture E is JSON")
}
