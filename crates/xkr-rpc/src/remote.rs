//! The HTTP API of xkrd without the node (`xkrd --p2pool`): the main chain comes from remote
//! nodes over the Hyperswarm clients topic, so there is no chain to answer the C++ routes.
//! Only `/status` and `/info` are served ([`crate::remote_router`]).

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::Extensions;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use xkr_chain::unix_now;
use xkr_consensus::Network;

use crate::handlers::{STATUS_OK, from_loopback, hyper_json, p2pool_json};
use crate::{HyperNetStatus, MiningStatus, NodeInfo, P2poolStatus};

/// The remote nodes as `/status` `remote` shows them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteStatus {
    /// The connected nodes.
    pub nodes: Vec<RemoteNodeStatus>,
    /// Index of the main-chain tip in use (`None` before one is chosen).
    pub height: Option<u64>,
    /// Its hash, hex.
    pub hash: Option<String>,
    /// At least one node answers and is synchronized, and the tip was confirmed less than
    /// three minutes ago.
    pub synced: bool,
    /// Seconds since a node last confirmed the tip (`None` before the first).
    pub tip_age: Option<u64>,
}

/// One connected node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteNodeStatus {
    /// The start of its DHT key on the clients topic, hex.
    pub key: String,
    /// Index of its top block.
    pub height: u64,
    /// The node says it is synchronized.
    pub synced: bool,
    /// It has the tip in use or builds on it.
    pub on_tip: bool,
}

/// What `/status` and `/info` answer from without the node.
#[derive(Debug, Clone)]
pub struct RemoteState {
    pub network: Network,
    /// The parts that run, e.g. `p2pool`.
    pub mode: String,
    /// The remote nodes.
    pub remote: watch::Receiver<Arc<RemoteStatus>>,
    /// The built-in p2pool (`None`: off).
    pub p2pool: Option<watch::Receiver<Arc<P2poolStatus>>>,
    /// The Hyperswarm DHT node (`None` until it reports).
    pub hyper: watch::Receiver<Option<HyperNetStatus>>,
    /// The built-in miner.
    pub mining: watch::Receiver<Arc<MiningStatus>>,
    /// Version, start time and data directory (the database fields stay empty).
    pub info: NodeInfo,
}

const fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    }
}

/// `/status` without the node: mode, remote nodes, Hyperswarm, p2pool and miner.
/// `remote.nodes`, the Hyperswarm details and the p2pool payout are shown to a loopback
/// client only, like the node's `/status`.
pub async fn status(State(s): State<Arc<RemoteState>>, ext: Extensions) -> Response {
    let local = from_loopback(&ext);
    let remote = s.remote.borrow().clone();
    let mut remote_json = serde_json::to_value(remote.as_ref()).unwrap_or(Value::Null);
    if !local && let Some(o) = remote_json.as_object_mut() {
        o.remove("nodes");
    }
    let now = unix_now();
    Json(json!({
        "version": s.info.version,
        "network": network_name(s.network),
        "mode": s.mode,
        "chain": "remote",
        "height": remote.height.map_or(0, |h| h + 1),
        "top_block_hash": remote.hash,
        "synced": remote.synced,
        "remote": remote_json,
        "hyper": hyper_json(*s.hyper.borrow(), local),
        "mining": serde_json::to_value(s.mining.borrow().as_ref()).unwrap_or(Value::Null),
        "hugin": Value::Null,
        "p2pool": s.p2pool.as_ref().map_or(Value::Null, |p| p2pool_json(&p.borrow(), local)),
        "clients": Value::Null,
        "data_dir": (local && !s.info.data_dir.as_os_str().is_empty())
            .then(|| s.info.data_dir.display().to_string()),
        "start_time": s.info.start_time,
        "uptime": now.saturating_sub(s.info.start_time),
    }))
    .into_response()
}

/// `/info` without the node: version, mode and the remote tip (`height` is the tip index + 1,
/// as in the node's `/info`).
pub async fn info(State(s): State<Arc<RemoteState>>) -> Response {
    let remote = s.remote.borrow().clone();
    Json(json!({
        "status": STATUS_OK,
        "version": s.info.version,
        "network": network_name(s.network),
        "testnet": s.network == Network::Testnet,
        "mode": s.mode,
        "chain": "remote",
        "height": remote.height.map_or(0, |h| h + 1),
        "top_block_hash": remote.hash,
        "synced": remote.synced,
        "remote_nodes": remote.nodes.len(),
        "start_time": s.info.start_time,
    }))
    .into_response()
}
