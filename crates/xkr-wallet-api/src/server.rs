//! The server: route table, middleware and the shared state
//! (`src/wallet_api/api_dispatcher.cpp` `ApiDispatcher`, routes at `ApiDispatcher::ApiDispatcher`,
//! `ApiDispatcher::middleware`; HTTP behaviour of the bundled cpp-httplib
//! `external/cpp-httplib/httplib.h` `Server::routing`).
//!
//! * The path is percent-decoded (`+` reads as a space) and must match a route pattern whole:
//!   addresses `(?:SEKR|Xkr)[a-zA-Z0-9]{95}` (so integrated addresses never match a route),
//!   hashes `[a-fA-F0-9]{64}`, heights `\d+` (`src/wallet_api/constants.h`).
//! * No matching route: 404, empty body. A method other than `GET`, `HEAD`, `POST`, `PUT`,
//!   `DELETE` and `OPTIONS`: 400, empty body. `HEAD` uses the `GET` routes.
//! * `OPTIONS` matches every path and skips the middleware ([`crate::cors`]).
//! * The middleware, in order: the body is read as JSON (anything else as `null`),
//!   `Access-Control-Allow-Origin` with `--enable-cors`, `X-API-KEY` (401), the wallet state
//!   (403 when a route needs an open wallet and none is open, or needs none and one is),
//!   view wallets on routes that need spend keys (400 `ILLEGAL_VIEW_WALLET_OPERATION`), then the
//!   handler.
//!
//! One wallet at a time: opening, importing, creating and closing hold the write lock (C++
//! `m_mutex`), every other route the read lock.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, Response, Uri};
use serde_json::Value;
use tokio::sync::RwLock;
use xkr_daemon_client::{AnyTransport, ClientSource, Transport};
use xkr_wallet::validate::{ADDRESS_PREFIX_ALT_TEXT, ADDRESS_PREFIX_TEXT, STANDARD_ADDRESS_LENGTH};
use xkr_wallet::{AutosaveConfig, ErrorCode, Node, Wallet, WalletConfig, WalletError};
use zeroize::Zeroizing;

use crate::auth::ApiKey;
use crate::handlers;
use crate::params::parse_body;
use crate::respond::Reply;

/// Largest request body accepted.
pub const MAX_BODY_BYTES: usize = 64 << 20;

/// Makes the daemon connection of a wallet from `daemonHost` and `daemonPort`.
pub trait NodeFactory: Send + Sync + 'static {
    /// The transport of the connections.
    type Transport: Transport + 'static;

    /// A connection to `host:port` (`Nigel(daemonHost, daemonPort)`).
    fn connect(&self, host: &str, port: u16) -> Result<Node<Self::Transport>, WalletError>;

    /// Whether the host [`xkr_wallet::SWARM_HOST`] works, so a wallet opened without a daemon
    /// uses Hyperswarm when no local node answers.
    fn has_swarm(&self) -> bool {
        false
    }
}

/// Daemons over HTTP, and over Hyperswarm for the host [`xkr_wallet::SWARM_HOST`] when
/// `swarm` is set.
#[derive(Clone, Default)]
pub struct Nodes {
    /// Where Hyperswarm connections come from (`None`: HTTP only).
    pub swarm: Option<Arc<dyn ClientSource>>,
}

impl std::fmt::Debug for Nodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nodes")
            .field("swarm", &self.swarm.is_some())
            .finish()
    }
}

impl NodeFactory for Nodes {
    type Transport = AnyTransport;

    fn connect(&self, host: &str, port: u16) -> Result<Node<AnyTransport>, WalletError> {
        Node::connect(host, port, self.swarm.as_ref())
    }

    fn has_swarm(&self) -> bool {
        self.swarm.is_some()
    }
}

/// Settings of the server.
#[derive(Clone)]
pub struct ApiConfig {
    /// `--rpc-password`: the `X-API-KEY` every request must carry.
    pub rpc_password: Zeroizing<String>,
    /// `--enable-cors`, `None` when unset or empty.
    pub cors: Option<String>,
    /// `daemonPort` when a request gives none (`RPC_DEFAULT_PORT`).
    pub default_daemon_port: u16,
    /// Settings of the wallets the server opens.
    pub wallet: WalletConfig,
}

impl std::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("cors", &self.cors)
            .field("default_daemon_port", &self.default_daemon_port)
            .field("wallet", &self.wallet)
            .finish_non_exhaustive()
    }
}

/// The state shared by all requests: the configuration and the open wallet, if any.
pub struct App<N: NodeFactory> {
    pub(crate) nodes: N,
    pub(crate) key: ApiKey,
    pub(crate) cors: Option<String>,
    pub(crate) default_daemon_port: u16,
    pub(crate) wallet_config: WalletConfig,
    pub(crate) wallet: RwLock<Option<Wallet<N::Transport>>>,
    /// How the wallets this server opens save by themselves (off unless set).
    pub(crate) autosave: std::sync::Mutex<Option<AutosaveConfig>>,
}

impl<N: NodeFactory> std::fmt::Debug for App<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("cors", &self.cors)
            .field("default_daemon_port", &self.default_daemon_port)
            .finish_non_exhaustive()
    }
}

impl<N: NodeFactory> App<N> {
    /// A server with no wallet open.
    #[must_use]
    pub fn new(config: ApiConfig, nodes: N) -> Arc<Self> {
        let key = ApiKey::new(&config.rpc_password);
        Arc::new(Self {
            nodes,
            key,
            cors: config.cors.filter(|c| !c.is_empty()),
            default_daemon_port: config.default_daemon_port,
            wallet_config: config.wallet,
            wallet: RwLock::new(None),
            autosave: std::sync::Mutex::new(None),
        })
    }

    /// Makes the wallets opened from now on save by themselves ([`AutosaveConfig`]): every
    /// few minutes while something changed, after each transaction and after a long sync,
    /// besides `PUT /save` and closing. `None` turns it off.
    pub fn set_autosave(&self, config: Option<AutosaveConfig>) {
        if let Ok(mut slot) = self.autosave.lock() {
            *slot = config;
        }
    }

    /// The autosave of newly opened wallets.
    pub(crate) fn autosave(&self) -> Option<AutosaveConfig> {
        self.autosave.lock().ok().and_then(|slot| *slot)
    }

    /// Whether a wallet is open.
    pub async fn is_open(&self) -> bool {
        self.wallet.read().await.is_some()
    }

    /// Stops syncing and saves the open wallet, if any (the console's `exit`, C++
    /// `~WalletBackend`). Returns whether a wallet was open.
    pub async fn close_wallet(&self) -> bool {
        let wallet = self.wallet.write().await.take();
        match wallet {
            Some(wallet) => {
                if let Err(e) = wallet.close().await {
                    tracing::warn!(error = %e, "saving the wallet failed");
                }
                true
            }
            None => false,
        }
    }
}

/// The axum router of the API.
pub fn router<N: NodeFactory>(app: Arc<App<N>>) -> Router {
    Router::new()
        .fallback(dispatch::<N>)
        .with_state(app)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// What a route needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No wallet may be open (`walletMustBeClosed`).
    Closed,
    /// `DELETE /wallet`: a wallet must be open; it is taken out.
    Close,
    /// A wallet must be open; `view` says whether a view wallet may use the route.
    Open {
        /// `viewWalletsAllowed`.
        view: bool,
    },
}

/// The routes, with the path parts the handlers read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// POST `/wallet/open`.
    WalletOpen,
    /// POST `/wallet/import/key`.
    ImportKey,
    /// POST `/wallet/import/seed`.
    ImportSeed,
    /// POST `/wallet/import/view`.
    ImportView,
    /// POST `/wallet/create`.
    WalletCreate,
    /// POST `/addresses/create`.
    AddressCreate,
    /// POST `/addresses/import`.
    AddressImport,
    /// POST `/addresses/import/view`.
    AddressImportView,
    /// POST `/transactions/send/basic`.
    SendBasic,
    /// POST `/transactions/send/advanced`.
    SendAdvanced,
    /// POST `/transactions/send/fusion/basic`.
    FusionBasic,
    /// POST `/transactions/send/fusion/advanced`.
    FusionAdvanced,
    /// DELETE `/wallet`.
    WalletClose,
    /// DELETE `/addresses/{address}`.
    AddressDelete(String),
    /// PUT `/save`.
    Save,
    /// PUT `/reset`.
    Reset,
    /// PUT `/node`.
    NodeSet,
    /// GET `/node`.
    NodeGet,
    /// GET `/keys`.
    ViewKey,
    /// GET `/keys/{address}`.
    SpendKeys(String),
    /// GET `/keys/mnemonic/{address}`.
    Mnemonic(String),
    /// GET `/status`.
    Status,
    /// GET `/addresses`.
    Addresses,
    /// GET `/addresses/primary`.
    PrimaryAddress,
    /// GET `/addresses/{address}/{paymentId}`.
    IntegratedAddress(String, String),
    /// GET `/transactions`.
    Transactions,
    /// GET `/transactions/unconfirmed`.
    Unconfirmed,
    /// GET `/transactions/unconfirmed/{address}`.
    UnconfirmedFor(String),
    /// GET `/transactions/{height}`.
    TransactionsFrom(String),
    /// GET `/transactions/{start}/{end}`.
    TransactionsRange(String, String),
    /// GET `/transactions/address/{address}/{height}`.
    AddressTransactionsFrom(String, String),
    /// GET `/transactions/address/{address}/{start}/{end}`.
    AddressTransactionsRange(String, String, String),
    /// GET `/transactions/privatekey/{hash}`.
    TxPrivateKey(String),
    /// GET `/transactions/hash/{hash}`.
    TransactionByHash(String),
    /// GET `/balance`.
    Balance,
    /// GET `/balance/{address}`.
    BalanceFor(String),
    /// GET `/balances`.
    Balances,
    /// OPTIONS, any path.
    Options,
}

impl Route {
    /// The wallet state and view-wallet rule of the route (`router(..., walletMustBeOpen,
    /// viewWalletsAllowed)` in the C++ constructor).
    #[must_use]
    pub const fn access(&self) -> Access {
        match self {
            Self::WalletOpen
            | Self::ImportKey
            | Self::ImportSeed
            | Self::ImportView
            | Self::WalletCreate => Access::Closed,
            Self::WalletClose => Access::Close,
            Self::AddressCreate
            | Self::AddressImport
            | Self::SendBasic
            | Self::SendAdvanced
            | Self::FusionBasic
            | Self::FusionAdvanced
            | Self::SpendKeys(_)
            | Self::Mnemonic(_)
            | Self::TxPrivateKey(_) => Access::Open { view: false },
            _ => Access::Open { view: true },
        }
    }
}

/// `(?:SEKR|Xkr)[a-zA-Z0-9]{95}`.
#[must_use]
pub fn is_address(s: &str) -> bool {
    let body_len = STANDARD_ADDRESS_LENGTH - ADDRESS_PREFIX_TEXT.len();
    let body = s
        .strip_prefix(ADDRESS_PREFIX_TEXT)
        .or_else(|| s.strip_prefix(ADDRESS_PREFIX_ALT_TEXT));
    body.is_some_and(|b| b.len() == body_len && b.bytes().all(|c| c.is_ascii_alphanumeric()))
}

/// `[a-fA-F0-9]{64}`.
#[must_use]
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `\d+`.
#[must_use]
pub fn is_height(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// The route of `method` and the decoded `path`, `None` for 404. `method` must be one of the six
/// methods the server knows.
#[must_use]
pub fn route(method: &Method, path: &str) -> Option<Route> {
    if *method == Method::OPTIONS {
        // ".*" does not match line terminators
        return (!path.contains(['\n', '\r'])).then_some(Route::Options);
    }
    let rest = path.strip_prefix('/')?;
    let parts: Vec<&str> = rest.split('/').collect();
    let s = |i: usize| parts[i].to_owned();
    if *method == Method::POST {
        return match parts.as_slice() {
            ["wallet", "open"] => Some(Route::WalletOpen),
            ["wallet", "import", "key"] => Some(Route::ImportKey),
            ["wallet", "import", "seed"] => Some(Route::ImportSeed),
            ["wallet", "import", "view"] => Some(Route::ImportView),
            ["wallet", "create"] => Some(Route::WalletCreate),
            ["addresses", "create"] => Some(Route::AddressCreate),
            ["addresses", "import"] => Some(Route::AddressImport),
            ["addresses", "import", "view"] => Some(Route::AddressImportView),
            ["transactions", "send", "basic"] => Some(Route::SendBasic),
            ["transactions", "send", "advanced"] => Some(Route::SendAdvanced),
            ["transactions", "send", "fusion", "basic"] => Some(Route::FusionBasic),
            ["transactions", "send", "fusion", "advanced"] => Some(Route::FusionAdvanced),
            _ => None,
        };
    }
    if *method == Method::DELETE {
        return match parts.as_slice() {
            ["wallet"] => Some(Route::WalletClose),
            ["addresses", a] if is_address(a) => Some(Route::AddressDelete(s(1))),
            _ => None,
        };
    }
    if *method == Method::PUT {
        return match parts.as_slice() {
            ["save"] => Some(Route::Save),
            ["reset"] => Some(Route::Reset),
            ["node"] => Some(Route::NodeSet),
            _ => None,
        };
    }
    if *method != Method::GET && *method != Method::HEAD {
        return None;
    }
    match parts.as_slice() {
        ["node"] => Some(Route::NodeGet),
        ["keys"] => Some(Route::ViewKey),
        ["keys", a] if is_address(a) => Some(Route::SpendKeys(s(1))),
        ["keys", "mnemonic", a] if is_address(a) => Some(Route::Mnemonic(s(2))),
        ["status"] => Some(Route::Status),
        ["addresses"] => Some(Route::Addresses),
        ["addresses", "primary"] => Some(Route::PrimaryAddress),
        ["addresses", a, h] if is_address(a) && is_hash(h) => {
            Some(Route::IntegratedAddress(s(1), s(2)))
        }
        ["transactions"] => Some(Route::Transactions),
        ["transactions", "unconfirmed"] => Some(Route::Unconfirmed),
        ["transactions", "unconfirmed", a] if is_address(a) => Some(Route::UnconfirmedFor(s(2))),
        ["transactions", h] if is_height(h) => Some(Route::TransactionsFrom(s(1))),
        ["transactions", h1, h2] if is_height(h1) && is_height(h2) => {
            Some(Route::TransactionsRange(s(1), s(2)))
        }
        ["transactions", "address", a, h] if is_address(a) && is_height(h) => {
            Some(Route::AddressTransactionsFrom(s(2), s(3)))
        }
        ["transactions", "address", a, h1, h2]
            if is_address(a) && is_height(h1) && is_height(h2) =>
        {
            Some(Route::AddressTransactionsRange(s(2), s(3), s(4)))
        }
        ["transactions", "privatekey", h] if is_hash(h) => Some(Route::TxPrivateKey(s(2))),
        ["transactions", "hash", h] if is_hash(h) => Some(Route::TransactionByHash(s(2))),
        ["balance"] => Some(Route::Balance),
        ["balance", a] if is_address(a) => Some(Route::BalanceFor(s(1))),
        ["balances"] => Some(Route::Balances),
        _ => None,
    }
}

/// The methods the server parses requests for (cpp-httplib `Server::parse_request_line`).
fn known_method(method: &Method) -> bool {
    [
        Method::GET,
        Method::HEAD,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::OPTIONS,
    ]
    .contains(method)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// cpp-httplib `detail::decode_url`: `%XX`, `%uXXXX` (as UTF-8) and `+` as a space; a `%`
/// without valid hex digits stays as it is.
#[must_use]
pub fn decode_url(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 1 < b.len() => {
                if b[i + 1] == b'u' {
                    let code = b.get(i + 2..i + 6).filter(|d| d.len() == 4).and_then(|d| {
                        d.iter()
                            .try_fold(0u32, |acc, c| Some(acc * 16 + u32::from(hex_digit(*c)?)))
                    });
                    if let Some(code) = code {
                        if let Some(ch) = char::from_u32(code) {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        i += 6;
                    } else {
                        out.push(b'%');
                        i += 1;
                    }
                } else {
                    let pair = b
                        .get(i + 1..i + 3)
                        .filter(|d| d.len() == 2)
                        .and_then(|d| Some(hex_digit(d[0])? * 16 + hex_digit(d[1])?));
                    if let Some(byte) = pair {
                        out.push(byte);
                        i += 3;
                    } else {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn dispatch<N: NodeFactory>(
    State(app): State<Arc<App<N>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if !known_method(&method) {
        return Reply::empty(400).into_response(None);
    }
    let path = decode_url(uri.path());
    let Some(route) = route(&method, &path) else {
        tracing::info!("Unknown {method} request: {path}");
        return Reply::empty(404).into_response(None);
    };
    tracing::info!("Incoming {method} request: {path}");
    if route == Route::Options {
        return crate::cors::options(app.cors.as_deref(), &headers).into_response(None);
    }
    let body = parse_body(&body);
    let reply = middleware(&app, &route, &headers, &body).await;
    reply.into_response(app.cors.as_deref())
}

/// `ApiDispatcher::middleware` after the body and the CORS header.
async fn middleware<N: NodeFactory>(
    app: &App<N>,
    route: &Route,
    headers: &HeaderMap,
    body: &Value,
) -> Reply {
    if !app.key.accepts(headers) {
        tracing::info!("Rejecting unauthorized request: X-API-KEY is missing or incorrect");
        return Reply::empty(401);
    }
    match route.access() {
        Access::Closed => handlers::wallet::open(app, route, body).await,
        Access::Close => handlers::wallet::close(app).await,
        Access::Open { view } => {
            let guard = app.wallet.read().await;
            let Some(wallet) = guard.as_ref() else {
                return Reply::empty(403);
            };
            if !view && wallet.is_view_wallet() {
                return Reply::error(&ErrorCode::IllegalViewWalletOperation.into());
            }
            handlers::run(app, route, wallet, body).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(prefix: &str, len: usize) -> String {
        format!("{prefix}{}", "a".repeat(len))
    }

    #[test]
    fn address_pattern() {
        assert!(is_address(&addr("SEKR", 95)));
        assert!(is_address(&addr("Xkr", 95)));
        assert!(!is_address(&addr("SEKR", 94)));
        assert!(!is_address(&addr("SEKR", 96)));
        assert!(!is_address(&addr("Xkr", 96)));
        assert!(!is_address(&addr("TRTL", 95)));
        // integrated addresses are 187 (SEKR) or 186 (Xkr) characters
        assert!(!is_address(&addr("SEKR", 183)));
        assert!(!is_address(&addr("Xkr", 183)));
        assert!(!is_address(&format!("SEKR{}-", "a".repeat(94))));
        assert!(is_hash(&"aF".repeat(32)));
        assert!(!is_hash(&"ag".repeat(32)));
        assert!(!is_hash(&"a".repeat(63)));
        assert!(is_height("0123"));
        assert!(!is_height(""));
        assert!(!is_height("12x"));
    }

    #[test]
    fn route_table() {
        let a = addr("SEKR", 95);
        let x = addr("Xkr", 95);
        let int = addr("SEKR", 183);
        let h = "ab".repeat(32);
        let get = |p: &str| route(&Method::GET, p);
        assert_eq!(get("/status"), Some(Route::Status));
        assert_eq!(route(&Method::HEAD, "/status"), Some(Route::Status));
        assert_eq!(get("/status/"), None);
        assert_eq!(get("status"), None);
        assert_eq!(
            get(&format!("/keys/{a}")),
            Some(Route::SpendKeys(a.clone()))
        );
        assert_eq!(
            get(&format!("/keys/{x}")),
            Some(Route::SpendKeys(x.clone()))
        );
        assert_eq!(get(&format!("/keys/{int}")), None);
        assert_eq!(
            get(&format!("/keys/mnemonic/{x}")),
            Some(Route::Mnemonic(x.clone()))
        );
        assert_eq!(get("/addresses/primary"), Some(Route::PrimaryAddress));
        assert_eq!(
            get(&format!("/addresses/{x}/{h}")),
            Some(Route::IntegratedAddress(x.clone(), h.clone()))
        );
        assert_eq!(get(&format!("/addresses/{int}/{h}")), None);
        assert_eq!(get(&format!("/addresses/{a}/{}", &h[1..])), None);
        assert_eq!(get("/transactions/unconfirmed"), Some(Route::Unconfirmed));
        assert_eq!(
            get(&format!("/transactions/unconfirmed/{a}")),
            Some(Route::UnconfirmedFor(a.clone()))
        );
        assert_eq!(get(&format!("/transactions/unconfirmed/{int}")), None);
        assert_eq!(
            get("/transactions/10"),
            Some(Route::TransactionsFrom("10".into()))
        );
        assert_eq!(get("/transactions/12x"), None);
        assert_eq!(
            get("/transactions/1/2"),
            Some(Route::TransactionsRange("1".into(), "2".into()))
        );
        assert_eq!(
            get(&format!("/transactions/address/{x}/5")),
            Some(Route::AddressTransactionsFrom(x.clone(), "5".into()))
        );
        assert_eq!(
            get(&format!("/transactions/address/{a}/5/6")),
            Some(Route::AddressTransactionsRange(
                a.clone(),
                "5".into(),
                "6".into()
            ))
        );
        assert_eq!(get(&format!("/transactions/address/{int}/5")), None);
        assert_eq!(
            get(&format!("/transactions/hash/{h}")),
            Some(Route::TransactionByHash(h.clone()))
        );
        assert_eq!(
            get(&format!("/transactions/privatekey/{h}")),
            Some(Route::TxPrivateKey(h.clone()))
        );
        assert_eq!(
            get(&format!("/balance/{x}")),
            Some(Route::BalanceFor(x.clone()))
        );
        assert_eq!(get(&format!("/balance/{int}")), None);
        assert_eq!(get("/balances"), Some(Route::Balances));
        assert_eq!(get("/wallet/open"), None);
        assert_eq!(
            route(&Method::POST, "/wallet/open"),
            Some(Route::WalletOpen)
        );
        assert_eq!(route(&Method::POST, "/status"), None);
        assert_eq!(
            route(&Method::POST, "/transactions/send/fusion/advanced"),
            Some(Route::FusionAdvanced)
        );
        assert_eq!(route(&Method::DELETE, "/wallet"), Some(Route::WalletClose));
        assert_eq!(
            route(&Method::DELETE, &format!("/addresses/{x}")),
            Some(Route::AddressDelete(x.clone()))
        );
        assert_eq!(route(&Method::DELETE, &format!("/addresses/{int}")), None);
        assert_eq!(route(&Method::PUT, "/node"), Some(Route::NodeSet));
        assert_eq!(get("/node"), Some(Route::NodeGet));
        assert_eq!(
            route(&Method::OPTIONS, "/anything/at/all"),
            Some(Route::Options)
        );
        assert_eq!(route(&Method::OPTIONS, "/a\nb"), None);
        assert!(!known_method(&Method::PATCH));
    }

    #[test]
    fn access_rules() {
        assert_eq!(Route::WalletOpen.access(), Access::Closed);
        assert_eq!(Route::WalletClose.access(), Access::Close);
        assert_eq!(Route::SendAdvanced.access(), Access::Open { view: false });
        assert_eq!(
            Route::Mnemonic(String::new()).access(),
            Access::Open { view: false }
        );
        assert_eq!(
            Route::AddressImportView.access(),
            Access::Open { view: true }
        );
        assert_eq!(
            Route::AddressDelete(String::new()).access(),
            Access::Open { view: true }
        );
        assert_eq!(Route::Balances.access(), Access::Open { view: true });
    }

    #[test]
    fn url_decoding() {
        assert_eq!(decode_url("/status"), "/status");
        assert_eq!(decode_url("/sta%74us"), "/status");
        assert_eq!(decode_url("/a+b"), "/a b");
        assert_eq!(decode_url("/a%zz"), "/a%zz");
        assert_eq!(decode_url("/a%"), "/a%");
        assert_eq!(decode_url("/a%u0041"), "/aA");
        assert_eq!(decode_url("/a%0A"), "/a\n");
    }
}
