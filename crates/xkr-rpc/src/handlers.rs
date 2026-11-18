//! REST-style handlers (C++ "JSON handlers").

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::connect_info::{ConnectInfo, MockConnectInfo};
use axum::http::{Extensions, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use xkr_chain::{Chain, ChainError, unix_now};
use xkr_consensus::Network;
use xkr_consensus::params::DIFFICULTY_TARGET;
use xkr_serialization::{decode_block, encode_transaction, transaction_hash};
use xkr_storage::Reader;
use xkr_storage::pruning::PruningSeed;
use xkr_txpool::TxSource;
use xkr_types::Hash;

use crate::explorer::MAX_POOL_LIST_TXS;
use crate::listing::{self, json_response};
use crate::{NetStatus, RpcCommand, RpcState, wallet};

/// Status string of successful C++ responses.
pub(crate) const STATUS_OK: &str = "OK";

// Request list caps. The 2 MB body alone allows ~30 000 hashes; each cap is far above what
// wallet-backend-js, explorers and the Hugin apps send, so real answers are unchanged. C++ has
// none of them.

/// Most `blockHashCheckpoints` a `/getwalletsyncdata` call looks at (the rest is ignored).
/// wallet-backend-js sends its last 50 block hashes plus one checkpoint every 5 000 blocks
/// (`LAST_KNOWN_BLOCK_HASHES_SIZE`, `BLOCK_HASH_CHECKPOINTS_INTERVAL`): about 650 entries at
/// height 3 M, 10 000 only at height 50 M. Lookups stop at the first hash on the main chain.
pub(crate) const MAX_WALLET_SYNC_CHECKPOINTS: usize = 10_000;
/// Most `transactionHashes` per `/get_transactions_status` call (one pool and one database
/// lookup each; wallets ask for their few unconfirmed transactions).
pub(crate) const MAX_TX_STATUS_HASHES: usize = 10_000;
/// Most distinct `txs_hashes` per `/gettransactions` call (one database read and hex encoding
/// each; explorers ask for one block's transactions).
pub(crate) const MAX_GET_TRANSACTIONS: usize = 10_000;
/// Most transaction bytes (before hex) one `/gettransactions` answer carries: a block's
/// transactions are far below it (the cumulative block size limit is a few hundred KB).
pub(crate) const MAX_GET_TRANSACTIONS_BYTES: usize = 32 * 1024 * 1024;
/// Most `knownTxsIds` per `/get_pool_changes_lite` call. wallet-backend-js sends the pool
/// transactions it has seen; the 2 MB body bounds the list at ~30 000 already, the cap keeps
/// it bounded if the body limit is ever raised.
pub(crate) const MAX_KNOWN_TXS_IDS: usize = 100_000;
/// Most `addedTxs` per `/get_pool_changes_lite` answer. wallet-backend-js adds every returned
/// hash to its known list, so a larger pool is delivered over the next polls; each entry
/// clones one pool transaction, so without a cap every call cloned the whole pool.
pub(crate) const MAX_POOL_CHANGES_ADDED: usize = 2_000;

/// A failed request.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// The request does not parse.
    BadRequest(String),
    /// The node failed.
    Internal(String),
    /// The item needs the ring signatures of the block at this height, which this node pruned
    /// (`docs/storage.md` "Stripe pruning"). Not a node failure: routes answer it per item
    /// where their shape allows (`missed_tx`, a "pruned" status), otherwise with 404.
    Pruned(u64),
}

impl ApiError {
    /// The message of [`Self::Pruned`].
    pub(crate) fn pruned_message(height: u64) -> String {
        format!("block {height} is pruned on this node (its ring signatures were dropped)")
    }
}

impl From<ChainError> for ApiError {
    fn from(e: ChainError) -> Self {
        match e {
            ChainError::Pruned(h) | ChainError::Storage(xkr_storage::Error::Pruned(h)) => {
                Self::Pruned(h)
            }
            e => Self::Internal(e.to_string()),
        }
    }
}

impl From<xkr_storage::Error> for ApiError {
    fn from(e: xkr_storage::Error) -> Self {
        match e {
            xkr_storage::Error::Pruned(h) => Self::Pruned(h),
            e => Self::Internal(e.to_string()),
        }
    }
}

impl From<xkr_serialization::DecodeError> for ApiError {
    fn from(e: xkr_serialization::DecodeError) -> Self {
        Self::Internal(format!("stored data does not decode: {e}"))
    }
}

impl From<xkr_serialization::EncodeError> for ApiError {
    fn from(e: xkr_serialization::EncodeError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "status": m }))).into_response()
            }
            Self::Internal(m) => {
                tracing::warn!(error = %m, "RPC request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "status": format!("Failed: {m}") })),
                )
                    .into_response()
            }
            Self::Pruned(h) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "status": format!("Failed: {}", Self::pruned_message(h)) })),
            )
                .into_response(),
        }
    }
}

/// Runs a storage-reading closure on the blocking pool and turns the result into JSON.
pub(crate) async fn run<F>(s: Arc<RpcState>, f: F) -> Response
where
    F: FnOnce(&RpcState) -> Result<Value, ApiError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(s.as_ref())).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError::Internal(e.to_string()).into_response(),
    }
}

/// Like [`run`] for a closure that writes the JSON answer itself.
pub(crate) async fn run_bytes<F>(s: Arc<RpcState>, f: F) -> Response
where
    F: FnOnce(&RpcState) -> Result<Vec<u8>, ApiError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || f(s.as_ref())).await {
        Ok(Ok(body)) => json_response(body),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ApiError::Internal(e.to_string()).into_response(),
    }
}

/// Parses a JSON body; an empty body is the default request (C++ accepts that too).
pub(crate) fn parse<T: DeserializeOwned + Default>(body: &Bytes) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body)
        .map_err(|e| ApiError::BadRequest(format!("Failed to parse request: {e}")))
}

/// Parses a hex hash.
pub(crate) fn parse_hash(s: &str) -> Option<Hash> {
    s.trim().parse().ok()
}

/// Top block index of a snapshot.
pub(crate) fn top_index(reader: &Reader) -> Result<u64, ApiError> {
    Ok(reader.top_height()?.unwrap_or(0))
}

/// Network height (C++ `max(1, getBlockchainHeight())`), never below our own height.
fn network_height(s: &RpcState, height: u64) -> u64 {
    s.net.borrow().network_height.max(height).max(1)
}

/// C++ `fill_block_header_response` for the main-chain block at `height`.
pub(crate) fn block_header(
    chain: &Chain,
    reader: &Reader,
    height: u64,
    top: u64,
) -> Result<Value, ApiError> {
    let info = reader
        .block_info(height)?
        .ok_or_else(|| ApiError::Internal(format!("no block at {height}")))?;
    let blob = chain.with_compressor(|c| reader.block_blob(height, c))??;
    let blob = blob.ok_or_else(|| ApiError::Internal(format!("no block blob at {height}")))?;
    let block = decode_block(&blob)?;
    let coinbase_size = encode_transaction(&block.miner_tx)?.len() as u64;
    let reward: u64 = block.miner_tx.prefix.outputs.iter().map(|o| o.amount).sum();
    Ok(json!({
        "major_version": block.header.major_version,
        "minor_version": block.header.minor_version,
        "timestamp": block.header.timestamp,
        "prev_hash": block.header.prev_hash.to_string(),
        "nonce": block.header.nonce,
        "orphan_status": false,
        "height": height,
        "depth": top - height,
        "hash": Hash(info.hash).to_string(),
        "difficulty": info.difficulty,
        "reward": reward,
        "num_txes": info.tx_count + 1,
        "block_size": blob.len() as u64 + info.block_size - coinbase_size,
    }))
}

/// `on_get_info`.
pub(crate) fn info_json(s: &RpcState) -> Result<Value, ApiError> {
    let chain = &s.chain;
    let reader = chain.reader()?;
    let top = top_index(&reader)?;
    let height = top + 1;
    let info = reader
        .block_info(top)?
        .ok_or_else(|| ApiError::Internal("no top block".into()))?;
    let blob = chain
        .with_compressor(|c| reader.block_blob(top, c))??
        .unwrap_or_default();
    let (major, minor) =
        decode_block(&blob).map_or((0, 0), |b| (b.header.major_version, b.header.minor_version));
    let difficulty = chain.next_difficulty()?;
    let net = s.net.borrow().clone();
    let network_height = network_height(s, height);
    let forks = chain.params().forks();
    let fork = chain.fork_status()?;
    Ok(json!({
        "status": STATUS_OK,
        "height": height,
        "difficulty": difficulty,
        "tx_count": info.cumulative_tx_count.saturating_sub(height),
        "tx_pool_size": chain.pool().len(),
        "alt_blocks_count": chain.alt_block_count(),
        "outgoing_connections_count": net.outgoing,
        "incoming_connections_count": net.incoming,
        "hyper_connections_count": net.hyper,
        "white_peerlist_size": net.white_peers.len(),
        "grey_peerlist_size": net.gray_peers.len(),
        "last_known_block_index": network_height - 1,
        "network_height": network_height,
        // C++ FORK_HEIGHTS / FORK_HEIGHTS[CURRENT_FORK_INDEX]; fork 13 appears once scheduled.
        "upgrade_heights": forks.upgrade_heights(),
        "supported_height": forks.supported_height(),
        // xkr: share of the last 720 blocks signalling the next block-version fork (null: none).
        "fork_readiness": fork.readiness,
        // xkr: transaction versions the next block accepts ([1] before fork 14, [1, 2] in its
        // grace period, [2] after it; docs/tx-v2.md §9), so wallets know which one to build.
        "tx_versions": tx_versions(&forks, height),
        "hashrate": (difficulty + DIFFICULTY_TARGET / 2) / DIFFICULTY_TARGET,
        "major_version": major,
        "minor_version": minor,
        "start_time": s.info.start_time,
        "synced": height >= network_height,
        "testnet": chain.network() == Network::Testnet,
        "version": s.info.version,
        // xkr: stripe pruning (`stripes << 8 | stripe`, 0 = full node, 0x0100 = pruned
        // completely, recent blocks only; Monero's key).
        "pruning_seed": reader.pruning_seed().raw(),
    }))
}

/// `/status` `pruning`: our seed decoded, the pruned peers we are connected to and the stripe
/// the downloader waits for (raw seed, `null` when nothing is missing). A completely pruned
/// node (seed `0x0100`) has `full` false, `recent_only` true and no stripe.
fn pruning_json(raw: u32, net: &NetStatus) -> Value {
    let seed = PruningSeed::decode(raw);
    let striped = seed.filter(|s| s.is_stripe());
    json!({
        "seed": raw,
        "full": seed.is_some_and(PruningSeed::is_full),
        "recent_only": seed.is_some_and(PruningSeed::is_recent_only),
        "stripes": striped.map(PruningSeed::stripes),
        "stripe": striped.map(PruningSeed::stripe),
        "pruned_peers": net.pruned_peers,
        "missing_stripe": net.missing_stripe,
    })
}

/// `/status` `state`: the newest stored state hash (`span`, `hash`; `null` before the first
/// stable span) and the first span whose hash differs from the built-in list (`mismatch`:
/// `{ span, expected, found }`, `null` when none does).
pub(crate) fn state_json(view: &xkr_chain::StateView) -> Value {
    let latest = view.latest();
    json!({
        "span": latest.map(|(span, _)| span),
        "hash": latest.map(|(_, hash)| hash.to_string()),
        "mismatch": view.mismatch.map(|m| json!({
            "span": m.span,
            "expected": m.expected.to_string(),
            "found": m.found.to_string(),
        })),
    })
}

/// Transaction versions the block at `height` accepts, ascending (version 0 counts as 1).
fn tx_versions(forks: &xkr_consensus::ForkSchedule, height: u64) -> Vec<u8> {
    let allowed = forks.tx_versions_at(height);
    (allowed.min..=allowed.max).collect()
}

/// `/getinfo`, `/info`.
pub async fn get_info(State(s): State<Arc<RpcState>>) -> Response {
    run(s, info_json).await
}

/// `/getheight`, `/height`.
pub async fn get_height(State(s): State<Arc<RpcState>>) -> Response {
    run(s, |s| {
        let height = s.chain.tip().height + 1;
        Ok(json!({ "height": height, "network_height": network_height(s, height), "status": STATUS_OK }))
    })
    .await
}

/// `/fee`, `/feeinfo`.
pub async fn get_fee(State(s): State<Arc<RpcState>>) -> Response {
    Json(json!({ "address": s.info.fee_address, "amount": s.info.fee_amount, "status": STATUS_OK }))
        .into_response()
}

/// `/peers`, `/getpeers`.
pub async fn get_peers(State(s): State<Arc<RpcState>>) -> Response {
    let net = s.net.borrow().clone();
    Json(json!({ "status": STATUS_OK, "peers": net.white_peers, "gray_peers": net.gray_peers }))
        .into_response()
}

/// `/status`: xkr's own status for the desktop app. `hyper` is the Hyperswarm network's DHT node
/// ([`crate::HyperNetStatus`]: `state`, `udp_port`, `port_fixed`, `firewalled`, `nat`,
/// `private`), `null` with `--no-hyper`. `p2pool` is the built-in p2pool
/// ([`crate::P2poolStatus`]), `null` while it is off. `hyper.udp_port`, `hyper.firewalled`, `hyper.nat`, `hugin.fee_address`,
/// `hugin.p2pool.host`, `p2pool.payout_address`, `p2pool.stratum` and `p2pool.stratum_off` are
/// part of the answer only for a client on a loopback address ([`from_loopback`]); other clients get the rest.
#[allow(clippy::cast_precision_loss)]
pub async fn get_status(State(s): State<Arc<RpcState>>, ext: Extensions) -> Response {
    let local = from_loopback(&ext);
    run(s, move |s| {
        let tip = s.chain.tip();
        let height = tip.height + 1;
        let net = s.net.borrow().clone();
        let network_height = network_height(s, height);
        let pool = s.chain.pool().stats();
        let db_size = std::fs::metadata(&s.info.db_path).map_or(0, |m| m.len());
        let now = unix_now();
        let fork = s.chain.fork_status()?;
        let rate = s.chain.block_rate(now)?;
        let pruning = pruning_json(s.chain.reader()?.pruning_seed().raw(), &net);
        let fork_json = json!({
            "active_index": fork.active_index,
            "next_index": fork.next_index,
            "next_height": fork.next_height,
            "supported_index": fork.supported_index,
            "next_block_major_version": fork.next_block_major_version,
            "signal_index": fork.signal_index,
            "readiness": fork.readiness,
            "signalling": fork.signalling,
            "window": fork.window,
            "tx_versions": tx_versions(&s.chain.params().forks(), height),
        });
        let rate_json = json!({
            "window_secs": rate.window_secs,
            "blocks": rate.blocks,
            "expected": rate.expected,
            "ratio": rate.ratio,
            "status": rate.status.as_str(),
        });
        let network = match s.chain.network() {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
        };
        Ok(json!({
            "version": s.info.version,
            "network": network,
            "height": height,
            "top_block_hash": tip.hash.to_string(),
            "top_block_timestamp": tip.timestamp,
            "network_height": network_height,
            "synced": height >= network_height,
            "sync_progress": height as f64 / network_height as f64,
            "peers": {
                "legacy_outgoing": net.outgoing,
                "legacy_incoming": net.incoming,
                "hyper": net.hyper,
                "total": net.outgoing + net.incoming + net.hyper,
            },
            "hyper": hyper_json(net.hyper_net, local),
            "mining": serde_json::to_value(s.mining.borrow().as_ref()).unwrap_or(Value::Null),
            "hugin": s.hugin.as_deref().map_or(Value::Null, |h| hugin_json(h, local)),
            "p2pool": s.p2pool.as_ref().map_or(Value::Null, |p| p2pool_json(&p.borrow(), local)),
            "white_peerlist_size": net.white_peers.len(),
            "gray_peerlist_size": net.gray_peers.len(),
            "download": {
                "active": net.downloading,
                "blocks_per_second": net.blocks_per_second,
                "bytes_per_second": net.bytes_per_second,
            },
            "pool": { "count": pool.count, "bytes": pool.bytes, "fusion": pool.fusion },
            "alt_blocks": s.chain.alt_block_count(),
            "fork": fork_json,
            "block_rate": rate_json,
            "pruning": pruning,
            "state": state_json(&s.chain.state_view()),
            "db_size": db_size,
            "data_dir": (!s.info.data_dir.as_os_str().is_empty())
                .then(|| s.info.data_dir.display().to_string()),
            "start_time": s.info.start_time,
            "uptime": now.saturating_sub(s.info.start_time),
        }))
    })
    .await
}

/// `/status` `hugin`: the relay counters (`xkr_hugin::HuginStatsSnapshot`, field names as
/// there): connections, messages, posts, gossip, sync, `shares_accepted` (paid for a message) /
/// `shares_submitted` (sent to p2pool) / `shares_rejected_by_pool`, bans, `apps_cap` (most app
/// connections), `syncs_running` / `syncs_queued` (history syncs of apps being sent / waiting),
/// `queued_bytes` (send budget in use), `admission_paused` (new apps are closed right away),
/// `refused_busy` (app connections closed for that), `syncs_limited` (history sync answers
/// made smaller by an app's sync allowance), `apps_on_allowance` (apps whose sync allowance is
/// used up), `pool_ready` (a job can be handed out),
/// `fee_address` (`null`: none, shares are not submitted), `fee_donation` (the default donation
/// address) and `p2pool { host, connected }`. Without `local`, `fee_address` and `p2pool.host`
/// are left out.
fn hugin_json(stats: &xkr_hugin::HuginStats, local: bool) -> Value {
    let mut v = serde_json::to_value(stats.snapshot()).unwrap_or(Value::Null);
    if !local && let Some(o) = v.as_object_mut() {
        o.remove("fee_address");
        if let Some(p) = o.get_mut("p2pool").and_then(Value::as_object_mut) {
            p.remove("host");
        }
    }
    v
}

/// `/status` `p2pool` ([`crate::P2poolStatus`], field names as there); without `local`,
/// `payout_address`, `stratum` and `stratum_off` are left out.
fn p2pool_json(status: &crate::P2poolStatus, local: bool) -> Value {
    let mut v = serde_json::to_value(status).unwrap_or(Value::Null);
    if !local && let Some(o) = v.as_object_mut() {
        o.remove("payout_address");
        o.remove("stratum");
        o.remove("stratum_off");
    }
    v
}

/// `/status` `hyper` ([`crate::HyperNetStatus`], `null` without the Hyperswarm network);
/// without `local`, `udp_port`, `firewalled` and `nat` are left out.
fn hyper_json(hyper: Option<crate::HyperNetStatus>, local: bool) -> Value {
    let mut v = serde_json::to_value(hyper).unwrap_or(Value::Null);
    if !local && let Some(o) = v.as_object_mut() {
        for key in ["udp_port", "firewalled", "nat"] {
            o.remove(key);
        }
    }
    v
}

/// Whether the request came from a loopback address: the connection's remote address
/// (`ConnectInfo`, set by [`crate::serve`]) or a `MockConnectInfo` layer in tests. A request
/// without either does not count as one.
fn from_loopback(ext: &Extensions) -> bool {
    ext.get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .or_else(|| ext.get::<MockConnectInfo<SocketAddr>>().map(|c| c.0))
        .is_some_and(|a| a.ip().to_canonical().is_loopback())
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WalletSyncRequest {
    block_hash_checkpoints: Vec<String>,
    start_height: u64,
    start_timestamp: u64,
    block_count: u64,
}

/// `/getwalletsyncdata` (`Core::getWalletSyncData`).
pub async fn get_wallet_sync_data(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: WalletSyncRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let checkpoints: Vec<Hash> = req
            .block_hash_checkpoints
            .iter()
            .take(MAX_WALLET_SYNC_CHECKPOINTS)
            .filter_map(|h| parse_hash(h))
            .collect();
        match wallet::wallet_sync_data(
            &s.chain,
            &checkpoints,
            req.start_height,
            req.start_timestamp,
            req.block_count,
        )? {
            Some(items) => Ok(json!({ "status": STATUS_OK, "items": items })),
            None => Ok(json!({ "status": "Failed to perform query", "items": [] })),
        }
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RangeRequest {
    start_height: u64,
    end_height: u64,
}

/// Largest range `/get_global_indexes_for_range` serves at once.
pub(crate) const MAX_INDEX_RANGE: u64 = 10_000;

/// `/get_global_indexes_for_range` (`Core::getGlobalIndexesForRange`, end exclusive). Unlike
/// C++ the range is clamped to [`MAX_INDEX_RANGE`] blocks from `startHeight`: a longer range
/// is answered for its first 10 000 blocks only (wallet-backend-js asks per sync batch of at
/// most 100 blocks), and the caller asks again from where the answer ends.
pub async fn get_global_indexes_for_range(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: RangeRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let end = req
            .end_height
            .min(req.start_height.saturating_add(MAX_INDEX_RANGE));
        let indexes = wallet::global_indexes_for_range(&s.chain, req.start_height, end)?;
        Ok(json!({ "status": STATUS_OK, "indexes": indexes }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TxStatusRequest {
    transaction_hashes: Vec<String>,
}

/// `/get_transactions_status` (`Core::getTransactionsStatus`).
pub async fn get_transactions_status(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: TxStatusRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        if req.transaction_hashes.len() > MAX_TX_STATUS_HASHES {
            return Err(ApiError::BadRequest(format!(
                "Too many transaction hashes: {}, at most {MAX_TX_STATUS_HASHES}",
                req.transaction_hashes.len()
            )));
        }
        let reader = s.chain.reader()?;
        let mut in_pool = Vec::new();
        let mut in_block = Vec::new();
        let mut unknown = Vec::new();
        for text in &req.transaction_hashes {
            let Some(h) = parse_hash(text) else {
                return Err(ApiError::BadRequest(format!(
                    "Failed to parse transaction hash {text}"
                )));
            };
            if s.chain.pool().contains(&h) {
                in_pool.push(h.to_string());
            } else if reader.tx_location(&h)?.is_some() {
                in_block.push(h.to_string());
            } else {
                unknown.push(h.to_string());
            }
        }
        Ok(json!({
            "status": STATUS_OK,
            "transactionsInPool": in_pool,
            "transactionsInBlock": in_block,
            "transactionsUnknown": unknown,
        }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OIndexesRequest {
    txid: String,
}

/// `/get_o_indexes` (`Core::getTransactionGlobalIndexes`).
pub async fn get_o_indexes(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: OIndexesRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let reader = s.chain.reader()?;
        let indexes = match parse_hash(&req.txid) {
            Some(h) => match reader.tx_location(&h)? {
                Some(loc) => s
                    .chain
                    .with_compressor(|c| reader.tx_global_indexes(loc.height, loc.tx_index, c))??,
                None => None,
            },
            None => None,
        };
        Ok(match indexes {
            Some(v) => json!({ "o_indexes": v, "status": STATUS_OK }),
            None => json!({ "o_indexes": [], "status": "Failed" }),
        })
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GetTransactionsRequest {
    txs_hashes: Vec<String>,
}

/// One transaction blob from the pool or the chain (miner transactions included).
pub(crate) fn transaction_blob(
    chain: &Chain,
    reader: &Reader,
    hash: &Hash,
) -> Result<Option<Vec<u8>>, ApiError> {
    if let Some(b) = chain.pool().blob(hash) {
        return Ok(Some(b));
    }
    let Some(loc) = reader.tx_location(hash)? else {
        return Ok(None);
    };
    if loc.tx_index > 0 {
        return Ok(chain.with_compressor(|c| reader.transaction_blob(hash, c))??);
    }
    let Some(blob) = chain.with_compressor(|c| reader.block_blob(loc.height, c))?? else {
        return Ok(None);
    };
    let block = decode_block(&blob)?;
    if transaction_hash(&block.miner_tx)? != *hash {
        return Ok(None);
    }
    Ok(Some(encode_transaction(&block.miner_tx)?))
}

/// `/gettransactions` (`on_get_transactions`).
pub async fn get_transactions(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: GetTransactionsRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let reader = s.chain.reader()?;
        let mut found = Vec::new();
        let mut missed = Vec::new();
        // Each hash is answered once: ~30 k copies of one large transaction's hash fit in the
        // 2 MB body and would otherwise build gigabytes of hex.
        let mut asked = HashSet::new();
        for text in &req.txs_hashes {
            let Some(h) = parse_hash(text) else {
                return Ok(json!({ "txs_as_hex": [], "missed_tx": [], "status": "Failed to parse hex representation of transaction hash" }));
            };
            asked.insert(h);
        }
        if asked.len() > MAX_GET_TRANSACTIONS {
            return Ok(json!({
                "txs_as_hex": [],
                "missed_tx": [],
                "status": format!("Failed: too many transactions requested (at most {MAX_GET_TRANSACTIONS})"),
            }));
        }
        let mut seen = HashSet::with_capacity(asked.len());
        let mut bytes = 0usize;
        for text in &req.txs_hashes {
            // Parsed above; request order is kept, each hash answered once.
            let Some(h) = parse_hash(text) else { continue };
            if !seen.insert(h) {
                continue;
            }
            // A transaction in a block whose signatures this node pruned is missed, like an
            // unknown one (the other hashes are still answered).
            let blob = match transaction_blob(&s.chain, &reader, &h) {
                Err(ApiError::Pruned(_)) => None,
                other => other?,
            };
            match blob {
                Some(b) => {
                    bytes = bytes.saturating_add(b.len());
                    if bytes > MAX_GET_TRANSACTIONS_BYTES {
                        return Ok(json!({
                            "txs_as_hex": [],
                            "missed_tx": [],
                            "status": "Failed: the requested transactions exceed the response size limit, ask for fewer",
                        }));
                    }
                    found.push(hex::encode(b));
                }
                None => missed.push(h.to_string()),
            }
        }
        Ok(json!({ "txs_as_hex": found, "missed_tx": missed, "status": STATUS_OK }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SendRawTxRequest {
    tx_as_hex: String,
}

/// `/sendrawtransaction` (`on_send_raw_tx`): into the pool, then relayed.
pub async fn send_raw_transaction(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: SendRawTxRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        let Ok(blob) = hex::decode(req.tx_as_hex.trim()) else {
            return Ok(json!({ "status": "Failed", "error": "transaction is not valid hex" }));
        };
        match s.chain.submit_transaction(blob.clone(), TxSource::Local)? {
            Ok(hash) => {
                tracing::info!(%hash, "transaction received over RPC");
                let _ = s.commands.send(RpcCommand::RelayTransactions(vec![blob]));
                Ok(json!({ "status": STATUS_OK, "transactionHash": hash.to_string() }))
            }
            Err(e) => {
                tracing::debug!(error = %e, "transaction from RPC rejected");
                Ok(json!({ "status": "Failed", "error": e.to_string() }))
            }
        }
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RandomOutsRequest {
    amounts: Vec<u64>,
    outs_count: u16,
}

/// Most amounts served per `/getrandom_outs` call (wallets ask per transaction input).
const MAX_RANDOM_OUT_AMOUNTS: usize = 1_000;
/// Most outputs (`amounts.len() * outs_count`) served per `/getrandom_outs` call. C++ has no
/// bound; without one a single request (1 000 amounts x 65 535 outs) asks for 65 million
/// outputs. A wallet needs inputs x ring size (a large fusion transaction: a few thousand).
const MAX_RANDOM_OUTS_TOTAL: usize = 50_000;

/// `/getrandom_outs` (`on_get_random_outs` / `Core::getRandomOutputs`): for every amount up to
/// `outs_count` random unlocked outputs, sorted by global index. Like C++, the first amount
/// that has no usable output ends the answer with `"status": "Failed"` (and the outs so far).
pub async fn get_random_outs(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: RandomOutsRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run(s, move |s| {
        if req.amounts.len() > MAX_RANDOM_OUT_AMOUNTS
            || req.amounts.len().saturating_mul(usize::from(req.outs_count))
                > MAX_RANDOM_OUTS_TOTAL
        {
            return Ok(json!({ "outs": [], "status": "Failed" }));
        }
        let mut outs = Vec::with_capacity(req.amounts.len());
        for amount in &req.amounts {
            let Some(found) = s.chain.random_outputs(*amount, usize::from(req.outs_count))? else {
                return Ok(json!({ "outs": outs, "status": "Failed" }));
            };
            if found.len() != usize::from(req.outs_count) {
                tracing::debug!(amount, requested = req.outs_count, found = found.len(), "not enough outputs for amount");
            }
            let entries: Vec<Value> = found
                .iter()
                .map(|(index, key)| json!({ "global_amount_index": index, "out_key": key.to_string() }))
                .collect();
            outs.push(json!({ "amount": amount, "outs": entries }));
        }
        Ok(json!({ "outs": outs, "status": STATUS_OK }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PoolChangesRequest {
    tail_block_id: String,
    known_txs_ids: Vec<String>,
}

/// `/get_pool_changes_lite` (`Core::getPoolChangesLite`): pool transactions the caller does not
/// know (as prefixes) and known ids that left the pool. The C++ keys really contain a dot
/// (`KV_MEMBER(transactionPrefixInfo.txHash)`); wallet-backend-js rewrites them. At most
/// [`MAX_POOL_CHANGES_ADDED`] entries and [`listing::MAX_POOL_ANSWER_BYTES`] of JSON per
/// answer; the rest follows on the next poll.
pub async fn get_pool_changes_lite(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: PoolChangesRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        let known = parse_known_txs_ids(&req.known_txs_ids)?;
        let pool = s.chain.pool();
        let in_pool: HashSet<Hash> = pool.hashes().into_iter().collect();
        let known_set: HashSet<&Hash> = known.iter().collect();
        let deleted: Vec<Hash> = known
            .iter()
            .filter(|h| !in_pool.contains(*h))
            .copied()
            .collect();
        let tail_actual = parse_hash(&req.tail_block_id).is_some_and(|h| h == s.chain.tip().hash);
        let added = in_pool
            .iter()
            .filter(|h| !known_set.contains(h))
            .filter_map(|h| pool.entry(h));
        Ok(listing::pool_changes_lite(
            added,
            MAX_POOL_CHANGES_ADDED,
            &deleted,
            tail_actual,
        ))
    })
    .await
}

/// `knownTxsIds` of `/get_pool_changes(_lite)`: at most [`MAX_KNOWN_TXS_IDS`], all valid hashes.
fn parse_known_txs_ids(texts: &[String]) -> Result<Vec<Hash>, ApiError> {
    if texts.len() > MAX_KNOWN_TXS_IDS {
        return Err(ApiError::BadRequest(format!(
            "Too many known transaction ids: {}, at most {MAX_KNOWN_TXS_IDS}",
            texts.len()
        )));
    }
    texts
        .iter()
        .map(|text| {
            parse_hash(text).ok_or_else(|| {
                ApiError::BadRequest(format!("Failed to parse transaction hash {text}"))
            })
        })
        .collect()
}

/// `/get_pool_changes` (`Core::getPoolChanges`): like `/get_pool_changes_lite`, but `addedTxs`
/// are whole transaction blobs, each a JSON array of byte values (C++ serializes a
/// `BinaryArray` that way). At most [`MAX_POOL_CHANGES_ADDED`] blobs and
/// [`listing::MAX_POOL_ANSWER_BYTES`] of JSON per answer; the rest follows on the next poll.
pub async fn get_pool_changes(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: PoolChangesRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        let known = parse_known_txs_ids(&req.known_txs_ids)?;
        let pool = s.chain.pool();
        let in_pool: HashSet<Hash> = pool.hashes().into_iter().collect();
        let known_set: HashSet<&Hash> = known.iter().collect();
        let deleted: Vec<Hash> = known
            .iter()
            .filter(|h| !in_pool.contains(*h))
            .copied()
            .collect();
        let tail_actual = parse_hash(&req.tail_block_id).is_some_and(|h| h == s.chain.tip().hash);
        let added = in_pool
            .iter()
            .filter(|h| !known_set.contains(h))
            .filter_map(|h| pool.blob(h));
        Ok(listing::pool_changes(
            added,
            MAX_POOL_CHANGES_ADDED,
            &deleted,
            tail_actual,
        ))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PoolRequest {
    timestamp_begin: u64,
}

/// `/get_pool` (`Core::getPool`): pool transactions received at or after `timestampBegin`, as
/// prefixes with the dotted keys of `/get_pool_changes_lite`, oldest first (C++ walks an
/// `unordered_set`, so its order carries no meaning). At most [`MAX_POOL_LIST_TXS`] (the
/// `f_on_transactions_pool_json` cap) and [`listing::MAX_POOL_ANSWER_BYTES`] of JSON; a caller
/// that needs the rest asks again with a later `timestampBegin`.
pub async fn get_pool(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: PoolRequest = match parse(&body) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    run_bytes(s, move |s| {
        let pool = s.chain.pool();
        let mut summaries: Vec<_> = pool
            .summaries()
            .into_iter()
            .filter(|t| t.received >= req.timestamp_begin)
            .collect();
        summaries.sort_unstable_by_key(|t| (t.received, t.hash));
        let entries = summaries.iter().filter_map(|t| pool.entry(&t.hash));
        Ok(listing::pool_list(entries, MAX_POOL_LIST_TXS))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/status.pruning` for a full, a stripe-pruned and a completely pruned node.
    #[test]
    fn pruning_status_names_the_kind_of_seed() {
        let net = NetStatus::default();
        let full = pruning_json(0, &net);
        assert_eq!(full["full"], json!(true));
        assert_eq!(full["recent_only"], json!(false));
        assert_eq!(full["stripes"], Value::Null);
        let striped = pruning_json(0x0402, &net);
        assert_eq!(striped["full"], json!(false));
        assert_eq!(striped["recent_only"], json!(false));
        assert_eq!(striped["stripes"], json!(4));
        assert_eq!(striped["stripe"], json!(2));
        let recent = pruning_json(0x0100, &net);
        assert_eq!(recent["seed"], json!(256));
        assert_eq!(recent["full"], json!(false));
        assert_eq!(recent["recent_only"], json!(true));
        assert_eq!(recent["stripes"], Value::Null);
        assert_eq!(recent["stripe"], Value::Null);
    }

    /// `/status.state` before the first span, with a stored span, and with a mismatch.
    #[test]
    fn state_status_names_the_newest_span_and_a_mismatch() {
        let empty = state_json(&xkr_chain::StateView::default());
        assert_eq!(
            empty,
            json!({ "span": null, "hash": null, "mismatch": null })
        );
        let view = xkr_chain::StateView {
            hashes: vec![
                (Hash([1; 32]), Hash([9; 32])),
                (Hash([2; 32]), Hash([9; 32])),
            ],
            mismatch: Some(xkr_chain::StateMismatch {
                span: 2,
                expected: Hash([3; 32]),
                found: Hash([4; 32]),
            }),
        };
        let v = state_json(&view);
        assert_eq!(v["span"], json!(1));
        assert_eq!(v["hash"], json!("02".repeat(32)));
        assert_eq!(v["mismatch"]["span"], json!(2));
        assert_eq!(v["mismatch"]["expected"], json!("03".repeat(32)));
        assert_eq!(v["mismatch"]["found"], json!("04".repeat(32)));
    }

    /// A pruned block is its own error (404 per item), never an internal error.
    #[test]
    fn pruned_blocks_are_not_internal_errors() {
        assert!(matches!(
            ApiError::from(xkr_storage::Error::Pruned(7)),
            ApiError::Pruned(7)
        ));
        assert!(matches!(
            ApiError::from(ChainError::Pruned(8)),
            ApiError::Pruned(8)
        ));
        assert!(matches!(
            ApiError::from(ChainError::Storage(xkr_storage::Error::Pruned(9))),
            ApiError::Pruned(9)
        ));
        assert!(matches!(
            ApiError::from(ChainError::Corrupt("x".into())),
            ApiError::Internal(_)
        ));
        let response = ApiError::Pruned(9).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
