//! Shared helpers for the router tests: a temporary database with only the genesis block, the
//! [`RpcState`] around it, and requests sent straight into the router (no socket).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig};
use xkr_consensus::Network;
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_txpool::{PoolConfig, TxPool};

/// A database file under the temp dir, removed on drop.
pub struct TempDb(pub PathBuf);

impl TempDb {
    pub fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-rpc-test-{}-{}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A mainnet node state over `db` (genesis only), with the Hugin relay counters `hugin`.
pub fn state_with_hugin(db: &TempDb, hugin: Option<Arc<xkr_hugin::HuginStats>>) -> Arc<RpcState> {
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
        hugin,
        p2pool: None,
        info: NodeInfo {
            version: "rpc-test".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: String::new(),
            fee_amount: 0,
        },
    })
}

/// [`state_with_hugin`] without the Hugin relay.
pub fn state(db: &TempDb) -> Arc<RpcState> {
    state_with_hugin(db, None)
}

/// POSTs `body` as JSON to `path`; returns the status and the body.
pub async fn post(app: &Router, path: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .expect("body");
    (status, bytes.to_vec())
}

/// A JSON value as a request body.
pub fn json_body(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("json")
}
