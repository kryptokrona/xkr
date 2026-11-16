//! xkr-daemon-client — a typed async client for the daemon's HTTP API: xkrd (`crates/xkr-rpc`)
//! and the C++ `kryptokronad` (`src/rpc/rpc_server.cpp`).
//!
//! It replaces what the C++ wallets do through Nigel (`src/nigel/nigel.cpp`) and what the miner
//! and pools do through `/json_rpc`:
//!
//! | method | route | C++ |
//! |---|---|---|
//! | [`DaemonClient::info`] | `GET /info` (same handler as `/getinfo`) | `Nigel::getDaemonInfo` |
//! | [`DaemonClient::fee`] | `GET /fee` | `Nigel::getFeeInfo` |
//! | [`DaemonClient::wallet_sync_data`] | `/getwalletsyncdata` | `Nigel::getWalletSyncData` |
//! | [`DaemonClient::global_indexes_for_range`] | `/get_global_indexes_for_range` | `Nigel::getGlobalIndexesForRange` |
//! | [`DaemonClient::random_outs`] | `/getrandom_outs` | `Nigel::getRandomOutsByAmounts` |
//! | [`DaemonClient::send_raw_transaction`] | `/sendrawtransaction` | `Nigel::sendTransaction` |
//! | [`DaemonClient::transactions_status`] | `/get_transactions_status` | `Nigel::getTransactionsStatus` |
//! | [`DaemonClient::transactions`] | `/gettransactions` | `on_get_transactions` |
//! | [`DaemonClient::block_count`] | `getblockcount` | `on_getblockcount` |
//! | [`DaemonClient::last_block_header`] | `getlastblockheader` | `on_get_last_block_header` |
//! | [`DaemonClient::block_header_by_height`] | `getblockheaderbyheight` | `on_get_block_header_by_height` |
//! | [`DaemonClient::block_header_by_hash`] | `getblockheaderbyhash` | `on_get_block_header_by_hash` |
//! | [`DaemonClient::block_headers_range`] | `get_block_headers_range` | `on_get_block_headers_range_json` |
//! | [`DaemonClient::block_template`] | `getblocktemplate` | `on_getblocktemplate` |
//! | [`DaemonClient::submit_block`] | `submitblock` | `on_submitblock` |
//! | [`DaemonClient::json_rpc`] | any `/json_rpc` method | `processJsonRpcRequest` |
//!
//! plus the explorer routes in [`explorer`]. Requests carry exactly the keys the C++ clients
//! send, and answers parse whether or not they carry the xkr additions (`tx_versions`,
//! `view_tag`, `transactionHash`, `error`).
//!
//! Errors ([`DaemonError`]): no answer ([`DaemonError::Transport`]; C++ Nigel's
//! "connection error"), an HTTP status other than 200 ([`DaemonError::Http`]), a `status` other
//! than `"OK"` ([`DaemonError::Status`]), a JSON-RPC error object ([`DaemonError::Rpc`]) and an
//! answer of the wrong shape ([`DaemonError::Parse`]). A refused transaction is not an error but
//! [`SendOutcome::Failed`].
//!
//! Requests go through a [`Transport`]: [`HttpTransport`] over the network, or any other
//! implementation (the tests hand requests straight to the `xkr-rpc` router).
#![forbid(unsafe_code)]

pub mod explorer;
pub mod http;
pub mod mining;
pub mod types;
mod wallet;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub use http::{
    DaemonUrl, HttpConfig, HttpRequest, HttpResponse, HttpTransport, Method, Transport,
    TransportError,
};
pub use types::{
    BlockHeader, BlockTemplate, Info, KeyInput, KeyOutput, NodeFee, OutputEntry, RandomOuts,
    RawCoinbaseTransaction, RawTransaction, SendOutcome, Transactions, TransactionsStatus,
    WalletBlock, WalletSyncRequest,
};

/// `status` of a successful C++ answer (`CORE_RPC_STATUS_OK`).
pub const STATUS_OK: &str = "OK";

/// Longest part of an error body kept in [`DaemonError::Http`].
const MAX_ERROR_TEXT: usize = 512;

/// A failed call.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// No answer: address, connection, TLS, timeout or size limit.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// An HTTP status other than 200. `message` is the JSON `status` of the answer when it has
    /// one (xkrd's `400`, `404` and `500` answers), otherwise the start of the body (`403 Host
    /// not allowed`, `503`, the C++ `500 Core is busy`).
    #[error("daemon answered HTTP {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// What the daemon said.
        message: String,
    },
    /// A `status` other than `"OK"`, e.g. `"Failed to perform query"` from
    /// `/getwalletsyncdata` when none of the checkpoints is on the node's main chain, or
    /// `"Failed"` from `/getrandom_outs` when an amount has too few outputs.
    #[error("daemon answered status {0:?}")]
    Status(String),
    /// A JSON-RPC error object (`json_rpc::JsonRpcError`; codes in [`mining::codes`]).
    #[error("daemon error {code}: {message}")]
    Rpc {
        /// Error code.
        code: i64,
        /// Error message.
        message: String,
    },
    /// The answer is not JSON or does not have the expected shape.
    #[error("unexpected daemon answer: {0}")]
    Parse(String),
}

impl DaemonError {
    /// Whether the daemon could not be reached at all (C++ Nigel's `connectionError`), as
    /// opposed to an answer that reports a failure.
    #[must_use]
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }

    /// The JSON-RPC error code, if this is a JSON-RPC error.
    #[must_use]
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            Self::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// A typed client for one daemon. Cheap to clone when the transport is ([`HttpTransport`]
/// clones share their connection pool).
#[derive(Debug, Clone)]
pub struct DaemonClient<T = HttpTransport> {
    transport: T,
}

impl DaemonClient<HttpTransport> {
    /// A client for the daemon at `url` (`http://host:port`, `https://host[:port][/prefix]` or
    /// `host:port`) with the default [`HttpConfig`].
    pub fn connect(url: &str) -> Result<Self, DaemonError> {
        Self::connect_with(url, &HttpConfig::default())
    }

    /// A client for the daemon at `url` with `config`.
    pub fn connect_with(url: &str, config: &HttpConfig) -> Result<Self, DaemonError> {
        Ok(Self::new(HttpTransport::new(url, config)?))
    }
}

impl<T: Transport> DaemonClient<T> {
    /// A client sending through `transport`.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// The transport.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// `GET path`; the answer as JSON (any HTTP status but 200 is an error).
    pub async fn get(&self, path: &str) -> Result<Value, DaemonError> {
        self.request(Method::Get, path, Vec::new()).await
    }

    /// `POST path` with the JSON `body`; the answer as JSON (any HTTP status but 200 is an
    /// error).
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, DaemonError> {
        let body = serde_json::to_vec(body).map_err(|e| DaemonError::Parse(e.to_string()))?;
        self.request(Method::Post, path, body).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Value, DaemonError> {
        let response = self
            .transport
            .send(HttpRequest {
                method,
                path: path.to_owned(),
                body,
            })
            .await?;
        answer_json(path, &response)
    }

    /// A JSON-RPC 2.0 call on `/json_rpc` (`RpcServer::processJsonRpcRequest`): the `result`,
    /// or [`DaemonError::Rpc`] for an `error` object. `params` is sent as given (`null`, an
    /// object or an array).
    pub async fn json_rpc(&self, method: &str, params: Value) -> Result<Value, DaemonError> {
        let request = json!({ "jsonrpc": "2.0", "id": 0, "method": method, "params": params });
        let mut answer = self.post("/json_rpc", &request).await?;
        if let Some(error) = answer.get("error").filter(|e| !e.is_null()) {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Err(DaemonError::Rpc { code, message });
        }
        answer.get_mut("result").map(Value::take).ok_or_else(|| {
            DaemonError::Parse(format!("{method}: neither result nor error in the answer"))
        })
    }

    /// [`Self::json_rpc`] with a result that must have `status` `"OK"`, parsed as `R`.
    pub(crate) async fn json_rpc_ok<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<R, DaemonError> {
        let result = self.json_rpc(method, params).await?;
        require_ok(&result)?;
        parse(method, result)
    }

    /// `POST path` whose answer must have `status` `"OK"`.
    pub(crate) async fn post_ok(&self, path: &str, body: &Value) -> Result<Value, DaemonError> {
        let answer = self.post(path, body).await?;
        require_ok(&answer)?;
        Ok(answer)
    }
}

/// The JSON of a 200 answer; any other status is [`DaemonError::Http`].
fn answer_json(path: &str, response: &HttpResponse) -> Result<Value, DaemonError> {
    if response.status != 200 {
        let message = serde_json::from_slice::<Value>(&response.body)
            .ok()
            .and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| text_start(&response.body));
        return Err(DaemonError::Http {
            status: response.status,
            message,
        });
    }
    serde_json::from_slice(&response.body)
        .map_err(|e| DaemonError::Parse(format!("{path}: {e}: {}", text_start(&response.body))))
}

/// The start of a body as text, for error messages.
fn text_start(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    match text.char_indices().nth(MAX_ERROR_TEXT) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// [`DaemonError::Status`] unless `answer.status` is `"OK"`.
pub(crate) fn require_ok(answer: &Value) -> Result<(), DaemonError> {
    match answer.get("status").and_then(Value::as_str) {
        Some(STATUS_OK) => Ok(()),
        Some(other) => Err(DaemonError::Status(other.to_owned())),
        None => Err(DaemonError::Parse("answer without status".to_owned())),
    }
}

/// Hashes as the hex strings the requests carry.
pub(crate) fn hex_strings(hashes: &[xkr_types::Hash]) -> Vec<String> {
    hashes.iter().map(xkr_types::Hash::to_hex).collect()
}

/// `answer[key]`, moved out (`null` when missing).
pub(crate) fn field(mut answer: Value, key: &str) -> Value {
    answer.get_mut(key).map_or_default(Value::take)
}

/// `value` as `R`; [`DaemonError::Parse`] naming `what` otherwise.
pub(crate) fn parse<R: DeserializeOwned>(what: &str, value: Value) -> Result<R, DaemonError> {
    serde_json::from_value(value).map_err(|e| DaemonError::Parse(format!("{what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn http_errors_carry_the_daemon_status_text() {
        let body = r#"{"status":"Failed to parse request: x"}"#;
        let e = answer_json("/x", &response(400, body)).unwrap_err();
        let DaemonError::Http { status, message } = e else {
            panic!("not an HTTP error: {e}")
        };
        assert_eq!(
            (status, message.as_str()),
            (400, "Failed to parse request: x")
        );
        let e = answer_json("/x", &response(500, "Core is busy")).unwrap_err();
        let DaemonError::Http { status, message } = e else {
            panic!("not an HTTP error: {e}")
        };
        assert_eq!((status, message.as_str()), (500, "Core is busy"));
        let long = "x".repeat(2 * MAX_ERROR_TEXT);
        let e = answer_json("/x", &response(503, &long)).unwrap_err();
        let DaemonError::Http { message, .. } = e else {
            panic!("not an HTTP error: {e}")
        };
        assert_eq!(message.chars().count(), MAX_ERROR_TEXT + 1);
        let e = answer_json("/x", &response(200, "")).unwrap_err();
        assert!(matches!(e, DaemonError::Parse(_)));
        assert!(!e.is_unreachable());
    }

    #[test]
    fn statuses_other_than_ok_are_errors() {
        assert!(require_ok(&json!({ "status": "OK" })).is_ok());
        assert!(matches!(
            require_ok(&json!({ "status": "Failed" })),
            Err(DaemonError::Status(s)) if s == "Failed"
        ));
        assert!(matches!(require_ok(&json!({})), Err(DaemonError::Parse(_))));
    }
}
