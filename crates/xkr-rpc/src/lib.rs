//! xkr-rpc — the daemon's HTTP API on axum.
//!
//! A C++-compatible subset (`src/rpc/rpc_server.cpp`; same paths, JSON keys and status
//! strings) for wallets (wallet-backend-js, and the C++ `NodeRpcProxy` of kryptokrona-service
//! and xkrwallet) and p2pool, plus `GET /status` for the desktop app:
//!
//! | path | C++ handler |
//! |---|---|
//! | `/getinfo`, `/info` | `on_get_info` |
//! | `/getheight`, `/height` | `on_get_height` |
//! | `/fee`, `/feeinfo` | `on_get_fee_info` |
//! | `/peers`, `/getpeers` | `on_get_peers` |
//! | `/getwalletsyncdata` | `on_get_wallet_sync_data` |
//! | `/get_global_indexes_for_range` | `onGetGlobalIndexesForRange` |
//! | `/get_transactions_status` | `onGetTransactionsStatus` |
//! | `/get_o_indexes` | `on_get_indexes` |
//! | `/gettransactions` | `on_get_transactions` |
//! | `/sendrawtransaction` | `on_send_raw_tx` (pool + relay) |
//! | `/getrandom_outs` | `on_get_random_outs` (decoys for wallets) |
//! | `/get_pool_changes_lite` | `onGetPoolChangesLite` |
//! | `/get_pool_changes` | `onGetPoolChanges` |
//! | `/get_pool` | `onGetPool` |
//! | `/get_block_details_by_height` | `onGetBlockDetailsByHeight` |
//! | `/get_blocks_details_by_heights` | `onGetBlocksDetailsByHeights` |
//! | `/get_blocks_details_by_hashes` | `onGetBlocksDetailsByHashes` |
//! | `/get_transaction_details_by_hashes` | `onGetTransactionDetailsByHashes` |
//! | `/get_transaction_hashes_by_payment_id` | `onGetTransactionHashesByPaymentId` |
//! | `/queryblockslite` | `on_query_blocks_lite` (C++ walletd / xkrwallet sync) |
//! | `/get_blocks_hashes_by_timestamps` | `onGetBlocksHashesByTimestamps` |
//! | `/json_rpc` | `getblockcount`, `on_getblockhash`, `getlastblockheader`, `getblockheaderbyhash`, `getblockheaderbyheight`, `get_block_headers_range`, `get_info`, `getcurrencyid`, `getblocktemplate`, `get_miner_data`, `submitblock`, `f_blocks_list_json`, `f_block_json`, `f_transaction_json`, `f_on_transactions_pool_json` (the `f_*` methods only on a node started with `--explorer`, [`RpcState::explorer`]) |
//! | `/status` | xkr only: sync, peer, mining, Hugin relay and p2pool state for the desktop app |
//!
//! Like the C++ server, every path accepts GET and POST (the C++ HTTP server ignores the
//! method). Storage reads run on `spawn_blocking`.
//!
//! Request limits: bodies over 2 MB are refused with `413` (axum's
//! `DefaultBodyLimit`, applied by every body extractor), at most [`MAX_CONCURRENT_REQUESTS`]
//! are handled at once, counted until the response body is sent (more get `503` at once), and a
//! request that takes longer than [`REQUEST_TIMEOUT`] (reading its body included) gets `408`.
//! The server accepts at most [`MAX_CONNECTIONS`] connections at once (further ones wait) and
//! closes a connection whose writes make no progress for [`WRITE_STALL_TIMEOUT`].
//!
//! Browser access ([`AccessPolicy`]): before anything else a request is refused with `403` when its
//! `Host` names neither a loopback address, `localhost`, the bound IP (any IP literal when bound to
//! `0.0.0.0` / `::`) nor an explicitly allowed host (DNS rebinding), or when it carries an `Origin`
//! header that `--enable-cors` does not allow (cross-site "simple" POSTs from web pages). Clients
//! that are not browsers (wallet-backend-js under Node, the C++ miner, curl, Hugin) send no
//! `Origin` and a loopback or configured `Host`, so they are unaffected. A browser request that a
//! web page made without CORS (an `<img>`, `<script>` or `<iframe>` source, a link) carries
//! `Sec-Fetch-Site: cross-site` or `same-site` and no `Origin`; such requests need an allowed
//! `Origin` too and are refused with `403` otherwise.

mod details;
mod explorer;
mod handlers;
mod jsonrpc;
mod listing;
mod mining;
mod node_proxy;
mod wallet;

pub use details::MAX_DETAILS_ANSWER_BYTES;
pub use mining::{parse_miner_address, submit_mined_block};

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, BodyDataStream, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use futures::Stream;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use xkr_chain::{AddBlockResult, Chain};
use xkr_types::RawBlock;

/// Network state published by the P2P layer for the API.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetStatus {
    /// Outgoing legacy (Levin) connections.
    pub outgoing: usize,
    /// Incoming legacy connections.
    pub incoming: usize,
    /// Hyperswarm connections (peers added through `P2pHandle::add_stream` with a Hyperswarm
    /// origin).
    pub hyper: usize,
    /// White peer list as `ip:port`.
    pub white_peers: Vec<String>,
    /// Gray peer list as `ip:port`.
    pub gray_peers: Vec<String>,
    /// Highest chain height (top index + 1) seen from any peer or ourselves.
    pub network_height: u64,
    /// Index of our main-chain top block (the Hugin relay dates its jobs with it).
    pub top_index: u64,
    /// Id of our main-chain top block.
    pub top_hash: [u8; 32],
    /// Whether our chain is at the network height.
    pub synced: bool,
    /// Whether the block downloader is active.
    pub downloading: bool,
    /// Download rate over the last seconds, blocks per second.
    pub blocks_per_second: f64,
    /// Download rate over the last seconds, bytes per second.
    pub bytes_per_second: f64,
    /// Our raw pruning seed (`stripes << 8 | stripe`, 0 = full node, `0x0100` = pruned
    /// completely, recent blocks only).
    pub pruning_seed: u32,
    /// Connected peers whose pruning seed is not the full one (an invalid seed is read as
    /// "recent blocks only", so it counts here too).
    pub pruned_peers: usize,
    /// Raw seed of the stripe the downloader needs and no connected peer holds, if any.
    pub missing_stripe: Option<u32>,
    /// The Hyperswarm network's DHT node (`/status` `hyper`); `None` with `--no-hyper`.
    pub hyper_net: Option<HyperNetStatus>,
}

/// State of the node's HyperDHT node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HyperDhtState {
    /// Binding the UDP port and bootstrapping.
    #[default]
    Starting,
    /// Bootstrapped.
    Ready,
    /// Could not start (for example the `--port` UDP port is taken).
    Failed,
}

/// What the DHT nodes saw of the node's public UDP port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HyperNat {
    /// Not known yet.
    #[default]
    Unknown,
    /// One public port for every destination.
    Consistent,
    /// A new public port per destination (a strict NAT: holepunching towards another such node
    /// does not work).
    Random,
}

/// The Hyperswarm network's DHT node (one UDP port for node sync, Hugin and later p2pool),
/// shown by `/status` as `hyper` and by the console's `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HyperNetStatus {
    pub state: HyperDhtState,
    /// Local UDP port of the DHT (`None` until it is bound).
    pub udp_port: Option<u16>,
    /// The port was given with `--port`.
    pub port_fixed: bool,
    /// Other DHT nodes cannot reach the UDP port without holepunching (`None` until the DHT is
    /// ready).
    pub firewalled: Option<bool>,
    pub nat: HyperNat,
}

/// Requests from the API (and the built-in miner) to the P2P layer.
#[derive(Debug)]
pub enum RpcCommand {
    /// Relay these transaction blobs to peers (`relayTransactions`).
    RelayTransactions(Vec<Vec<u8>>),
    /// A mined block (`submitblock`, built-in miner): add it through the chain thread like a
    /// relayed block, relay it with hop 0 if it changed the main chain, and report the result.
    SubmitBlock {
        /// The block with its transactions.
        block: RawBlock,
        /// Outcome (`Err` = node failure).
        reply: oneshot::Sender<Result<AddBlockResult, String>>,
    },
}

/// State of the built-in miner (`xkrd --mine`), shown by `/status` as `mining`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MiningStatus {
    /// Whether the miner runs.
    pub active: bool,
    /// `solo` (blocks paid to [`Self::address`]) or `p2pool` (shares on the built-in p2pool,
    /// paid to the node's p2pool payout address); empty before the miner was started.
    pub mode: String,
    /// Address the rewards go to (in `p2pool` mode the payout address of the share being
    /// mined).
    pub address: String,
    /// Hashing threads.
    pub threads: usize,
    /// Hashes per second over the last measurement interval.
    pub hashrate: f64,
    /// Hashes computed since start.
    pub total_hashes: u64,
    /// Height of the block being mined.
    pub height: u64,
    /// Difficulty of the block being mined.
    pub difficulty: u64,
    /// Blocks found and accepted onto the main chain (`solo` mode).
    pub blocks_found: u64,
    /// Shares found and added to the p2pool sidechain (`p2pool` mode; a share that also meets
    /// the main-chain difficulty is submitted as a block by p2pool and counted there).
    pub shares_found: u64,
    /// Blocks found but refused by the chain.
    pub blocks_rejected: u64,
    /// Hash of the last accepted block.
    pub last_block_hash: Option<String>,
    /// Last error (template or submit), if any.
    pub last_error: Option<String>,
}

/// State of the built-in p2pool (`xkrd --p2pool`), shown by `/status` as `p2pool`.
/// `payout_address`, `stratum` and `stratum_off` are part of the answer only for a loopback
/// client.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct P2poolStatus {
    /// `waiting` (for the node to synchronize), `running`, `stopped` or `failed`.
    pub state: String,
    /// Sidechain name (`default`, `mini`, `nano` or the name of a custom sidechain).
    pub sidechain: String,
    /// Consensus id of the sidechain (hex).
    pub consensus_id: String,
    /// Sidechain height of the tip share (0 without one).
    pub height: u64,
    /// Difficulty of the next share.
    pub difficulty: u64,
    /// Sidechain hashrate: difficulty over the share time, hashes per second.
    pub hashrate: u64,
    /// Payout wallets seen on the sidechain in the last 72 hours.
    pub miners: u64,
    /// Shares stored.
    pub shares: u64,
    /// The sidechain has a tip and no shares are missing.
    pub synced: bool,
    /// Outgoing TCP peers.
    pub peers_out: u64,
    /// Incoming TCP peers.
    pub peers_in: u64,
    /// Hyperswarm peers.
    pub peers_hyper: u64,
    /// Miners connected to the stratum server.
    pub stratum_miners: u64,
    /// Hashrate of those miners over 15 minutes, hashes per second.
    pub stratum_hashrate: u64,
    /// Sidechain shares found by this node's miners since start.
    pub shares_found: u64,
    /// Main-chain blocks found by p2pool and added by this node since start.
    pub blocks_found: u64,
    /// The payout address (the node's fee address), `None`: none, no templates.
    pub payout_address: Option<String>,
    /// The payout address is the default donation address.
    pub payout_donation: bool,
    /// The stratum listen address (`None` while the stratum server is off).
    pub stratum: Option<String>,
    /// Why the stratum server is off while p2pool runs without it, e.g.
    /// `127.0.0.1:3333 in use; set --p2pool-stratum`.
    pub stratum_off: Option<String>,
    /// Why p2pool is not running (state `failed`).
    pub error: Option<String>,
}

/// Static facts about the node.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Version string (`/getinfo` `version`).
    pub version: String,
    /// Unix start time.
    pub start_time: u64,
    /// Data directory, reported by `/status` (`null` when empty).
    pub data_dir: PathBuf,
    /// Database file (its size is reported by `/status`).
    pub db_path: PathBuf,
    /// `--fee-address` (`/fee`).
    pub fee_address: String,
    /// `--fee-amount` (`/fee`).
    pub fee_amount: u32,
}

/// Everything the handlers need.
#[derive(Debug, Clone)]
pub struct RpcState {
    /// The chain (and its pool).
    pub chain: Arc<Chain>,
    /// Latest network status.
    pub net: watch::Receiver<Arc<NetStatus>>,
    /// Commands for the P2P layer.
    pub commands: mpsc::UnboundedSender<RpcCommand>,
    /// Latest state of the built-in miner.
    pub mining: watch::Receiver<Arc<MiningStatus>>,
    /// Counters of the Hugin relay (on by default), shown by `/status` as `hugin`; `None`
    /// (`"hugin": null`) when the relay is off.
    pub hugin: Option<Arc<xkr_hugin::HuginStats>>,
    /// The built-in p2pool, shown by `/status` as `p2pool`; `None` (`"p2pool": null`) when it
    /// is off.
    pub p2pool: Option<watch::Receiver<Arc<P2poolStatus>>>,
    /// Whether the block explorer methods (`f_blocks_list_json`, `f_block_json`,
    /// `f_transaction_json`, `f_on_transactions_pool_json`) are answered (`xkrd --explorer`,
    /// C++ `--enable-blockexplorer`). Off, `/json_rpc` reports them as not found.
    pub explorer: bool,
    /// Static node facts.
    pub info: NodeInfo,
}

/// Server settings.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// Listen address (default `127.0.0.1:11898`, testnet `11899`).
    pub bind: SocketAddr,
    /// `--enable-cors` origins (`*` allows any). Besides adding `Access-Control-Allow-Origin`
    /// like C++, these are the only web origins whose requests are served at all
    /// ([`AccessPolicy`]).
    pub cors: Vec<String>,
    /// Extra host names accepted in the `Host` header (lowercase, without port), for a node
    /// reached through a DNS name (reverse proxy, LAN name). Empty by default: then only
    /// loopback addresses, `localhost` and the bound IP (any IP literal when bound to
    /// `0.0.0.0` / `::`) are accepted. xkrd sets it with `--rpc-allowed-host`.
    pub allowed_hosts: Vec<String>,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 11898),
            cors: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }
}

/// Which browsers may talk to the API.
///
/// * `Host` (or, without one, the request URI's authority; port ignored, case-insensitive):
///   accepted when it is `localhost`, a loopback IP literal (`127.0.0.0/8`, `::1`), the bound
///   IP, any IP literal when the server is bound to `0.0.0.0` / `::`, or one of
///   [`RpcConfig::allowed_hosts`]. Any other DNS name is refused, which covers DNS rebinding
///   (a name that resolves to `127.0.0.1` is still sent as that name by the browser). A
///   request with neither a `Host` header nor an authority (HTTP/1.0) is accepted: browsers
///   always send one.
/// * `Sec-Fetch-Site` (sent by browsers): `cross-site` or `same-site` (a request a web page
///   made, CORS or not) needs an `Origin` that is accepted by the rule below; `same-origin`,
///   `none` (address bar) and an absent header (non-browser clients) need nothing more.
/// * `Origin`: absent is accepted (non-browser clients, address-bar navigation). Present, it
///   is accepted only when `--enable-cors` lists it exactly or lists `*`, or when it is the
///   node's own origin (`http://` + the accepted `Host`). Everything else, `null` included
///   (sandboxed iframes, `file://` and `data:` pages send `null`, so any web page can produce
///   it), is refused; a desktop app whose renderer calls the API directly passes its origin
///   with `--enable-cors` (for example `--enable-cors null` or `--enable-cors app://wallet`).
///
/// Refusals are `403` and happen before the concurrency limit, the body read and CORS
/// handling; preflights (`OPTIONS`) of allowed origins pass on to the CORS layer.
#[derive(Debug, Clone)]
pub struct AccessPolicy {
    bind_ip: IpAddr,
    allowed_hosts: Vec<String>,
    cors: Vec<String>,
}

impl AccessPolicy {
    /// A policy for a server bound to `bind_ip`, with the extra `allowed_hosts` and the
    /// `--enable-cors` origins `cors`.
    #[must_use]
    pub fn new(bind_ip: IpAddr, allowed_hosts: &[String], cors: &[String]) -> Self {
        Self {
            bind_ip: bind_ip.to_canonical(),
            allowed_hosts: allowed_hosts.iter().filter_map(|h| host_name(h)).collect(),
            cors: cors.to_vec(),
        }
    }

    /// The policy of `cfg`.
    #[must_use]
    pub fn from_config(cfg: &RpcConfig) -> Self {
        Self::new(cfg.bind.ip(), &cfg.allowed_hosts, &cfg.cors)
    }

    /// Whether a `Host` header value (with or without port) is accepted.
    #[must_use]
    pub fn host_allowed(&self, raw: &str) -> bool {
        let Some(host) = host_name(raw) else {
            return false;
        };
        if host == "localhost" || self.allowed_hosts.contains(&host) {
            return true;
        }
        host.parse::<IpAddr>().is_ok_and(|ip| {
            let ip = ip.to_canonical();
            ip.is_loopback() || ip == self.bind_ip || self.bind_ip.is_unspecified()
        })
    }

    /// Whether an `Origin` header value is accepted; `host` is the request's `Host`.
    #[must_use]
    pub fn origin_allowed(&self, origin: &str, host: Option<&str>) -> bool {
        if self.cors.iter().any(|c| c == "*" || c == origin) {
            return true;
        }
        host.is_some_and(|h| {
            self.host_allowed(h)
                && origin
                    .strip_prefix("http://")
                    .is_some_and(|o| o.eq_ignore_ascii_case(h.trim()))
        })
    }
}

/// The host part of a `Host` value: lowercase, without port, brackets or a trailing dot.
/// `None` when it is empty or malformed (a non-numeric port, garbage after `]`).
fn host_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        let end = rest.find(']')?;
        let after = &rest[end + 1..];
        let port_ok = after.is_empty()
            || after
                .strip_prefix(':')
                .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()));
        if !port_ok {
            return None;
        }
        &rest[..end]
    } else {
        match raw.rsplit_once(':') {
            // `name:port` / `ipv4:port`
            Some((h, port)) if !h.contains(':') => {
                if !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                h
            }
            // An unbracketed IPv6 literal (not valid in `Host`, parsed as a whole) or no port.
            _ => raw,
        }
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Most requests handled at once; more are answered `503 Service Unavailable` at once.
pub const MAX_CONCURRENT_REQUESTS: usize = 256;
/// A request still unanswered after this (a slow body included) gets `408 Request Timeout`.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A request answered after this long is logged (warn), with how many others were running.
const SLOW_REQUEST: Duration = Duration::from_secs(5);
/// Most connections served at once by [`serve`]; further connections wait to be accepted.
pub const MAX_CONNECTIONS: usize = 1024;
/// A connection whose writes make no progress for this long is closed by [`serve`].
pub const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The router with all routes, the request limits and the CORS layer, for a server bound to
/// loopback (`127.0.0.1`) with the `--enable-cors` origins `cors`. See [`router_with_access`].
pub fn router(state: Arc<RpcState>, cors: &[String]) -> Router {
    let access = AccessPolicy::new(IpAddr::V4(Ipv4Addr::LOCALHOST), &[], cors);
    router_with_access(state, access)
}

/// The router with all routes, the [`AccessPolicy`], the request limits and the CORS layer.
pub fn router_with_access(state: Arc<RpcState>, access: AccessPolicy) -> Router {
    use crate::details as d;
    use crate::handlers as h;
    use crate::node_proxy as n;
    let cors = access.cors.clone();
    Router::new()
        .route("/getinfo", get(h::get_info).post(h::get_info))
        .route("/info", get(h::get_info).post(h::get_info))
        .route("/getheight", get(h::get_height).post(h::get_height))
        .route("/height", get(h::get_height).post(h::get_height))
        .route("/fee", get(h::get_fee).post(h::get_fee))
        .route("/feeinfo", get(h::get_fee).post(h::get_fee))
        .route("/peers", get(h::get_peers).post(h::get_peers))
        .route("/getpeers", get(h::get_peers).post(h::get_peers))
        .route("/status", get(h::get_status).post(h::get_status))
        .route(
            "/getwalletsyncdata",
            get(h::get_wallet_sync_data).post(h::get_wallet_sync_data),
        )
        .route(
            "/get_global_indexes_for_range",
            get(h::get_global_indexes_for_range).post(h::get_global_indexes_for_range),
        )
        .route(
            "/get_transactions_status",
            get(h::get_transactions_status).post(h::get_transactions_status),
        )
        .route(
            "/get_o_indexes",
            get(h::get_o_indexes).post(h::get_o_indexes),
        )
        .route(
            "/gettransactions",
            get(h::get_transactions).post(h::get_transactions),
        )
        .route(
            "/sendrawtransaction",
            get(h::send_raw_transaction).post(h::send_raw_transaction),
        )
        .route(
            "/getrandom_outs",
            get(h::get_random_outs).post(h::get_random_outs),
        )
        .route(
            "/get_pool_changes_lite",
            get(h::get_pool_changes_lite).post(h::get_pool_changes_lite),
        )
        .route(
            "/get_pool_changes",
            get(h::get_pool_changes).post(h::get_pool_changes),
        )
        .route("/get_pool", get(h::get_pool).post(h::get_pool))
        .route(
            "/get_block_details_by_height",
            get(d::get_block_details_by_height).post(d::get_block_details_by_height),
        )
        .route(
            "/get_blocks_details_by_heights",
            get(d::get_blocks_details_by_heights).post(d::get_blocks_details_by_heights),
        )
        .route(
            "/get_blocks_details_by_hashes",
            get(d::get_blocks_details_by_hashes).post(d::get_blocks_details_by_hashes),
        )
        .route(
            "/get_transaction_details_by_hashes",
            get(d::get_transaction_details_by_hashes).post(d::get_transaction_details_by_hashes),
        )
        .route(
            "/get_transaction_hashes_by_payment_id",
            get(d::get_transaction_hashes_by_payment_id)
                .post(d::get_transaction_hashes_by_payment_id),
        )
        .route(
            "/queryblockslite",
            get(n::query_blocks_lite).post(n::query_blocks_lite),
        )
        .route(
            "/get_blocks_hashes_by_timestamps",
            get(n::get_blocks_hashes_by_timestamps).post(n::get_blocks_hashes_by_timestamps),
        )
        .route("/json_rpc", get(jsonrpc::json_rpc).post(jsonrpc::json_rpc))
        .layer(middleware::from_fn_with_state(
            Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            limits_layer,
        ))
        .layer(middleware::from_fn_with_state(Arc::new(cors), cors_layer))
        // Outermost, so it runs first: refusals cost a header lookup.
        .layer(middleware::from_fn_with_state(
            Arc::new(access),
            access_layer,
        ))
        .with_state(state)
}

/// [`AccessPolicy`]: `403` for a foreign `Host` or a web `Origin` that is not allowed.
async fn access_layer(
    State(policy): State<Arc<AccessPolicy>>,
    req: Request,
    next: Next,
) -> Response {
    // A `Host` header that is not visible ASCII becomes "" and is refused.
    let host: Option<String> = match req.headers().get(header::HOST) {
        Some(v) => Some(v.to_str().unwrap_or_default().to_owned()),
        None => req.uri().authority().map(|a| a.as_str().to_owned()),
    };
    if let Some(h) = host.as_deref()
        && !policy.host_allowed(h)
    {
        tracing::debug!(host = h, "RPC request refused: Host not allowed");
        return (StatusCode::FORBIDDEN, "Host not allowed").into_response();
    }
    let origin = req.headers().get(header::ORIGIN);
    if let Some(origin) = origin {
        let allowed = origin
            .to_str()
            .is_ok_and(|o| policy.origin_allowed(o, host.as_deref()));
        if !allowed {
            tracing::debug!(origin = ?origin, "RPC request refused: Origin not allowed");
            return (StatusCode::FORBIDDEN, "Origin not allowed").into_response();
        }
    }
    let from_other_site = req
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|v| !matches!(v.as_bytes(), b"same-origin" | b"none"));
    if from_other_site && origin.is_none() {
        tracing::debug!("RPC request refused: request from another site without Origin");
        return (StatusCode::FORBIDDEN, "Origin required").into_response();
    }
    next.run(req).await
}

/// [`MAX_CONCURRENT_REQUESTS`] and [`REQUEST_TIMEOUT`]. The permit moves into the response
/// body ([`PermitStream`]) and is released when the body has been sent or dropped.
async fn limits_layer(
    State(inflight): State<Arc<Semaphore>>,
    req: Request,
    next: Next,
) -> Response {
    let Ok(permit) = Arc::clone(&inflight).try_acquire_owned() else {
        tracing::warn!(
            "{} {}: too many concurrent requests",
            req.method(),
            req.uri().path()
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many concurrent requests",
        )
            .into_response();
    };
    let what = format!("{} {}", req.method(), req.uri().path());
    let started = std::time::Instant::now();
    let Ok(res) = tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await else {
        tracing::warn!(
            "{what}: no answer after {:?} ({} other requests running)",
            REQUEST_TIMEOUT,
            MAX_CONCURRENT_REQUESTS - 1 - inflight.available_permits()
        );
        return (StatusCode::REQUEST_TIMEOUT, "request timed out").into_response();
    };
    let elapsed = started.elapsed();
    if elapsed >= SLOW_REQUEST {
        tracing::warn!(
            "{what}: answered after {elapsed:?} ({} other requests running)",
            MAX_CONCURRENT_REQUESTS - 1 - inflight.available_permits()
        );
    }
    let (mut parts, body) = res.into_parts();
    let len = body.size_hint().exact();
    if len == Some(0) {
        return Response::from_parts(parts, body);
    }
    if let Some(n) = len
        && !parts.headers.contains_key(header::CONTENT_LENGTH)
    {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    let body = Body::from_stream(PermitStream {
        inner: body.into_data_stream(),
        permit: Some(permit),
    });
    Response::from_parts(parts, body)
}

/// A response body that holds a [`MAX_CONCURRENT_REQUESTS`] permit until it has been read to
/// the end or dropped.
struct PermitStream {
    inner: BodyDataStream,
    permit: Option<OwnedSemaphorePermit>,
}

impl Stream for PermitStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.inner).poll_next(cx);
        if matches!(next, Poll::Ready(None)) {
            drop(self.permit.take());
        }
        next
    }
}

/// Adds `Access-Control-Allow-Origin` like C++ `--enable-cors` and answers preflights.
async fn cors_layer(State(origins): State<Arc<Vec<String>>>, req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let preflight = req.method() == Method::OPTIONS;
    let mut res = if preflight && !origins.is_empty() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let allow = if origins.iter().any(|o| o == "*") {
        Some("*".to_owned())
    } else {
        origin.filter(|o| origins.iter().any(|x| x == o))
    };
    if let Some(allow) = allow
        && let Ok(v) = HeaderValue::from_str(&allow)
    {
        let h = res.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Content-Type"),
        );
    }
    res
}

/// Binds and serves until `shutdown` completes, with at most [`MAX_CONNECTIONS`] connections
/// and [`WRITE_STALL_TIMEOUT`].
pub async fn serve(
    cfg: RpcConfig,
    state: Arc<RpcState>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "RPC server listening");
    let access = AccessPolicy::from_config(&cfg);
    let listener = CappedListener {
        inner: listener,
        slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
    };
    // `tap_io` (a no-op here) gives the listener the wrapper whose remote address axum hands to
    // the handlers as `ConnectInfo<SocketAddr>` (`/status` reads it).
    let listener = listener.tap_io(|_: &mut StallGuard<TcpStream>| {});
    axum::serve(
        listener,
        router_with_access(state, access).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// A TCP listener that accepts a connection only while fewer than its `slots` are open; each
/// connection is wrapped in a [`StallGuard`] that holds its slot.
struct CappedListener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}

impl axum::serve::Listener for CappedListener {
    type Io = StallGuard<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // The semaphore is never closed, so this waits for a free slot.
        let slot = self.slots.clone().acquire_owned().await.ok();
        let (io, addr) = axum::serve::Listener::accept(&mut self.inner).await;
        (StallGuard::new(io, WRITE_STALL_TIMEOUT, slot), addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A stream whose writes fail with `TimedOut` once they have made no progress for `timeout`
/// (the peer stopped reading). Holds an optional connection slot until dropped.
pub(crate) struct StallGuard<S> {
    inner: S,
    timeout: Duration,
    stalled: Option<Pin<Box<tokio::time::Sleep>>>,
    _slot: Option<OwnedSemaphorePermit>,
}

impl<S> StallGuard<S> {
    pub(crate) fn new(inner: S, timeout: Duration, slot: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            inner,
            timeout,
            stalled: None,
            _slot: slot,
        }
    }

    /// Passes a ready result through (and forgets the stall); on `Pending` starts or checks the
    /// stall timer.
    fn check<T>(&mut self, cx: &mut Context<'_>, r: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if r.is_ready() {
            self.stalled = None;
            return r;
        }
        let timeout = self.timeout;
        let timer = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "write made no progress",
            )));
        }
        Poll::Pending
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for StallGuard<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for StallGuard<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.check(cx, r)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.check(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = Pin::new(&mut self.inner).poll_flush(cx);
        self.check(cx, r)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let r = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.check(cx, r)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn writes_without_progress_time_out() {
        let (a, _b) = tokio::io::duplex(64);
        let mut g = StallGuard::new(a, Duration::from_millis(100), None);
        let r = tokio::time::timeout(Duration::from_secs(5), g.write_all(&[0u8; 1024]))
            .await
            .expect("the write ends");
        assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);

        let (a, mut b) = tokio::io::duplex(64);
        let mut g = StallGuard::new(a, Duration::from_millis(100), None);
        let reader = tokio::spawn(async move {
            let mut got = Vec::new();
            let mut buf = [0u8; 16];
            while got.len() < 1024 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                let n = b.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            got.len()
        });
        g.write_all(&[1u8; 1024]).await.unwrap();
        assert_eq!(reader.await.unwrap(), 1024);
    }

    #[tokio::test]
    async fn a_response_body_holds_its_permit_until_read() {
        let sem = Arc::new(Semaphore::new(1));
        let permit = sem.clone().try_acquire_owned().unwrap();
        let body = Body::from_stream(PermitStream {
            inner: Body::from("hello").into_data_stream(),
            permit: Some(permit),
        });
        assert_eq!(sem.available_permits(), 0);
        let bytes = axum::body::to_bytes(body, 1024).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
        assert_eq!(sem.available_permits(), 1);
    }
}
