//! The messages: hello and its answers, requests, responses and pushes.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version (`v` of the hello and its answers).
pub const VERSION: u32 = 1;

/// Error codes of [`RpcError`]. Errors of the node's JSON-RPC methods keep their own codes (the
/// C++ `CORE_RPC_ERROR_CODE_*`, e.g. -1 wrong parameter, -2 too big height).
pub mod code {
    /// The method is not served on this service.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The parameters do not fit the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// The server failed to answer.
    pub const INTERNAL: i64 = -32603;
    /// Too many calls of this connection are being answered; retry when one is answered.
    pub const BUSY: i64 = -32000;
    /// Over the request rate of a connection; retry later.
    pub const RATE_LIMITED: i64 = -32001;
    /// The answer would be larger than a frame.
    pub const TOO_LARGE: i64 = -32002;
    /// The service exists but cannot answer this yet.
    pub const UNAVAILABLE: i64 = -32003;
}

/// What a client asks a node for in its hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    /// Chain data and block submission for chainless modes (p2pool, Hugin relay).
    Node,
    /// p2pool stratum of the node's built-in p2pool.
    Stratum,
    /// Wallet synchronization (a topic of its own, `xkr/<network>/wallets/v1`).
    Wallet,
}

impl Service {
    /// Every service, in the order a reply lists them.
    pub const ALL: [Service; 3] = [Service::Node, Service::Stratum, Service::Wallet];

    /// The name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Service::Node => "node",
            Service::Stratum => "stratum",
            Service::Wallet => "wallet",
        }
    }

    /// The service of a wire name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The client's first frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// [`VERSION`].
    pub v: u32,
    /// `mainnet` or `testnet`.
    pub network: String,
    /// [`Service::as_str`].
    pub service: String,
    /// The client program and version, e.g. `xkrd/1.0.0` (for logs only).
    #[serde(default)]
    pub client: String,
}

impl Hello {
    /// A version-1 hello.
    #[must_use]
    pub fn new(network: &str, service: Service, client: &str) -> Self {
        Self {
            v: VERSION,
            network: network.to_owned(),
            service: service.as_str().to_owned(),
            client: client.to_owned(),
        }
    }
}

/// The server's answer to an accepted hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    /// [`VERSION`].
    pub v: u32,
    /// Index of the node's top block (the block `hash` names).
    pub height: u64,
    /// Hash of the top block, hex.
    pub hash: String,
    /// The node is at the network height and not downloading.
    pub synced: bool,
    /// The services this node serves on this topic ([`Service::as_str`]).
    pub services: Vec<String>,
    /// The newest stable span with a state hash (`getstatehash`), if any.
    #[serde(default)]
    pub state_span: Option<u64>,
    /// Its state hash, hex.
    #[serde(default)]
    pub state_hash: Option<String>,
    /// The node's raw pruning seed (`stripes << 8 | stripe`; 0: full node).
    #[serde(default)]
    pub pruning_seed: u32,
}

impl HelloReply {
    /// The tip the reply describes.
    #[must_use]
    pub fn tip(&self) -> Tip {
        Tip {
            height: self.height,
            hash: self.hash.clone(),
            synced: self.synced,
        }
    }

    /// Whether `service` is listed.
    #[must_use]
    pub fn serves(&self, service: Service) -> bool {
        self.services.iter().any(|s| s == service.as_str())
    }
}

/// Why a hello was refused ([`Refusal::error`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefuseReason {
    /// Another protocol version.
    Version,
    /// Another network.
    Network,
    /// The service is unknown or not served by this node.
    Service,
    /// The node serves as many clients as it takes.
    Busy,
    /// The first frame was not a hello.
    Hello,
}

impl RefuseReason {
    /// The name on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            RefuseReason::Version => "version",
            RefuseReason::Network => "network",
            RefuseReason::Service => "service",
            RefuseReason::Busy => "busy",
            RefuseReason::Hello => "hello",
        }
    }
}

/// The server's answer to a refused hello; the server closes after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// [`VERSION`].
    #[serde(default = "version")]
    pub v: u32,
    /// [`RefuseReason::as_str`].
    pub error: String,
    /// For people.
    #[serde(default)]
    pub message: String,
}

fn version() -> u32 {
    VERSION
}

impl Refusal {
    #[must_use]
    pub fn new(reason: RefuseReason, message: impl Into<String>) -> Self {
        Self {
            v: VERSION,
            error: reason.as_str().to_owned(),
            message: message.into(),
        }
    }
}

/// A call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the client; the response carries it back.
    pub id: u64,
    pub method: String,
    /// Object or array, as the method takes it; absent means `null`.
    #[serde(default)]
    pub params: Value,
}

/// An error answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message} ({code})")]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The answer to a [`Request`]: `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// The response to call `id` with `outcome`.
    #[must_use]
    pub fn of(id: u64, outcome: Result<Value, RpcError>) -> Self {
        match outcome {
            Ok(result) => Self {
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => Self {
                id,
                result: None,
                error: Some(error),
            },
        }
    }

    /// The outcome; neither field (a `null` result) is `Ok(null)`.
    pub fn into_outcome(self) -> Result<Value, RpcError> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.result.unwrap_or(Value::Null)),
        }
    }
}

/// A message from the server that answers no call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Push {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Name of the tip push.
pub const TIP: &str = "tip";

/// The node's main-chain tip, pushed as `{"method":"tip","params":<Tip>}` whenever it changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tip {
    /// Index of the top block.
    pub height: u64,
    /// Its hash, hex.
    pub hash: String,
    /// The node is at the network height and not downloading.
    pub synced: bool,
}

impl Push {
    /// The tip push.
    #[must_use]
    pub fn tip(tip: &Tip) -> Self {
        Self {
            method: TIP.to_owned(),
            params: serde_json::to_value(tip).unwrap_or(Value::Null),
        }
    }

    /// The tip of a tip push.
    #[must_use]
    pub fn as_tip(&self) -> Option<Tip> {
        if self.method != TIP {
            return None;
        }
        serde_json::from_value(self.params.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn wire_shapes() {
        let hello = Hello::new("mainnet", Service::Node, "xkrd/1.0.0");
        assert_eq!(
            serde_json::to_value(&hello).unwrap(),
            json!({"v": 1, "network": "mainnet", "service": "node", "client": "xkrd/1.0.0"})
        );
        let reply = HelloReply {
            v: 1,
            height: 41,
            hash: "ab".repeat(32),
            synced: true,
            services: vec!["node".into(), "stratum".into()],
            state_span: None,
            state_hash: None,
            pruning_seed: 0,
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({"v": 1, "height": 41, "hash": "ab".repeat(32), "synced": true,
                   "services": ["node", "stratum"], "state_span": null, "state_hash": null,
                   "pruning_seed": 0})
        );
        assert!(reply.serves(Service::Stratum));
        assert!(!reply.serves(Service::Wallet));
        assert_eq!(
            serde_json::to_value(Refusal::new(RefuseReason::Service, "no")).unwrap(),
            json!({"v": 1, "error": "service", "message": "no"})
        );
        let r: Refusal = serde_json::from_value(json!({"error": "busy"})).unwrap();
        assert_eq!((r.v, r.error.as_str(), r.message.as_str()), (1, "busy", ""));
        assert_eq!(
            serde_json::to_value(Response::of(3, Ok(json!({"a": 1})))).unwrap(),
            json!({"id": 3, "result": {"a": 1}})
        );
        assert_eq!(
            serde_json::to_value(Response::of(4, Err(RpcError::new(-1, "bad")))).unwrap(),
            json!({"id": 4, "error": {"code": -1, "message": "bad"}})
        );
        let none: Response = serde_json::from_value(json!({"id": 5, "result": null})).unwrap();
        assert_eq!(none.into_outcome(), Ok(Value::Null));
        let tip = Tip {
            height: 9,
            hash: "cd".repeat(32),
            synced: false,
        };
        let push = Push::tip(&tip);
        assert_eq!(
            serde_json::to_value(&push).unwrap(),
            json!({"method": "tip", "params": {"height": 9, "hash": "cd".repeat(32), "synced": false}})
        );
        assert_eq!(push.as_tip(), Some(tip));
        let req: Request = serde_json::from_value(json!({"id": 1, "method": "getinfo"})).unwrap();
        assert_eq!(req.params, Value::Null);
        assert!(serde_json::from_value::<Request>(json!({"id": -1, "method": "x"})).is_err());
        assert!(serde_json::from_value::<Request>(json!({"id": "a", "method": "x"})).is_err());
    }

    #[test]
    fn service_names() {
        for s in Service::ALL {
            assert_eq!(Service::parse(s.as_str()), Some(s));
            assert_eq!(s.to_string(), s.as_str());
        }
        assert_eq!(Service::parse("Node"), None);
    }
}
