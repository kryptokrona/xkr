//! Stratum messages as typed values and their line framing (cryptonote-nodejs-pool `pool.js`
//! `handleMinerMethod`, xmrig `src/base/net/stratum/Client.cpp`), for pool servers and tests.
//!
//! Every message is one JSON object on one line (`\n`-terminated, at most
//! [`crate::client::MAX_LINE`] bytes):
//!
//! | direction | message | params / result |
//! |---|---|---|
//! | miner -> pool | `login` | [`LoginParams`] -> [`LoginResult`] |
//! | miner -> pool | `submit` | [`SubmitParams`] -> [`StatusResult`] (`OK`) |
//! | miner -> pool | `keepalived` | [`KeepalivedParams`] -> [`StatusResult`] (`KEEPALIVED`) |
//! | pool -> miner | `job` (notification) | [`JobParams`] |
//!
//! Errors are `{ "code": <int>, "message": <text> }` ([`RpcError`]).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::codec::LinesCodec;

use crate::client::MAX_LINE;

/// `login` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginParams {
    /// Payout address, optionally with a `.rigid` or `+difficulty` suffix the pool may read.
    pub login: String,
    /// Password (usually `x`).
    #[serde(default)]
    pub pass: String,
    /// Miner software name and version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Rig name (xmrig `--rig-id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rigid: Option<String>,
}

/// A job (`job` notification params, and `job` in [`LoginResult`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobParams {
    /// Job id, echoed in submits.
    pub job_id: String,
    /// Hashing blob as hex; the nonce goes at the usual offset ([`crate::blob::nonce_offset`]).
    pub blob: String,
    /// Compact share target as hex ([`crate::target::parse_target`]).
    pub target: String,
    /// Height of the block the job is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u64>,
    /// Algorithm name, if the pool sends one (xmrig extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algo: Option<String>,
}

/// `login` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginResult {
    /// Session id, sent as `id` with every later request.
    pub id: String,
    /// The first job.
    pub job: JobParams,
    /// `OK`.
    pub status: String,
}

/// `submit` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitParams {
    /// Session id from [`LoginResult::id`].
    pub id: String,
    /// The job the share is for.
    pub job_id: String,
    /// The 4 nonce bytes as 8 hex characters, in blob order.
    pub nonce: String,
    /// The proof-of-work hash as 64 hex characters.
    pub result: String,
}

/// `keepalived` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepalivedParams {
    /// Session id.
    pub id: String,
}

/// A result that is only a status (`OK` for a submit, `KEEPALIVED` for a keepalive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResult {
    /// The status text.
    pub status: String,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// Error code (pools use `-1` for most refusals).
    pub code: i64,
    /// Error text (`Low difficulty share`, `Duplicate share`, `Unauthenticated`, ...).
    pub message: String,
}

/// A line that is not a stratum message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    /// Not JSON.
    #[error("not JSON: {0}")]
    Json(String),
    /// JSON, but not a request, response or notification.
    #[error("not a stratum message: {0}")]
    Shape(&'static str),
    /// Params or result of another shape than asked for.
    #[error("unexpected {what}: {error}")]
    Content {
        /// `params` or `result`.
        what: &'static str,
        /// The parse error.
        error: String,
    },
}

/// One stratum message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A call that expects an answer with the same `id`.
    Request {
        /// Request id (a number from the clients we know, any JSON value on the wire).
        id: Value,
        /// Method name.
        method: String,
        /// Parameters.
        params: Value,
    },
    /// The answer to a request.
    Response {
        /// Id of the request.
        id: Value,
        /// The result (`None` is sent as `null`).
        result: Option<Value>,
        /// The error (`None` is sent as `null`).
        error: Option<RpcError>,
    },
    /// A message without an answer (the pool's `job`).
    Notification {
        /// Method name.
        method: String,
        /// Parameters.
        params: Value,
    },
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn from_value<T: DeserializeOwned>(what: &'static str, v: &Value) -> Result<T, MessageError> {
    T::deserialize(v).map_err(|e| MessageError::Content {
        what,
        error: e.to_string(),
    })
}

impl Message {
    /// A `login` request.
    #[must_use]
    pub fn login(id: u64, params: &LoginParams) -> Self {
        Self::request(id, "login", to_value(params))
    }

    /// A `submit` request.
    #[must_use]
    pub fn submit(id: u64, params: &SubmitParams) -> Self {
        Self::request(id, "submit", to_value(params))
    }

    /// A `keepalived` request.
    #[must_use]
    pub fn keepalived(id: u64, session: &str) -> Self {
        Self::request(
            id,
            "keepalived",
            to_value(&KeepalivedParams {
                id: session.to_owned(),
            }),
        )
    }

    /// A request with any method.
    #[must_use]
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self::Request {
            id: json!(id),
            method: method.to_owned(),
            params,
        }
    }

    /// A pushed `job` notification.
    #[must_use]
    pub fn job(job: &JobParams) -> Self {
        Self::Notification {
            method: "job".to_owned(),
            params: to_value(job),
        }
    }

    /// The successful answer to a `login`.
    #[must_use]
    pub fn login_ok(id: Value, result: &LoginResult) -> Self {
        Self::ok(id, to_value(result))
    }

    /// A successful answer that is only a status (`OK`, `KEEPALIVED`).
    #[must_use]
    pub fn status(id: Value, status: &str) -> Self {
        Self::ok(
            id,
            to_value(&StatusResult {
                status: status.to_owned(),
            }),
        )
    }

    /// A successful answer.
    #[must_use]
    pub fn ok(id: Value, result: Value) -> Self {
        Self::Response {
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error answer.
    #[must_use]
    pub fn error(id: Value, code: i64, message: &str) -> Self {
        Self::Response {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.to_owned(),
            }),
        }
    }

    /// The method of a request or notification.
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } => None,
        }
    }

    /// The params of a request or notification as `T`.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T, MessageError> {
        match self {
            Self::Request { params, .. } | Self::Notification { params, .. } => {
                from_value("params", params)
            }
            Self::Response { .. } => Err(MessageError::Shape("a response has no params")),
        }
    }

    /// The result of a successful response as `T`.
    pub fn result<T: DeserializeOwned>(&self) -> Result<T, MessageError> {
        match self {
            Self::Response {
                result: Some(r),
                error: None,
                ..
            } => from_value("result", r),
            _ => Err(MessageError::Shape("not a successful response")),
        }
    }

    /// The message as JSON.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::Request { id, method, params } => {
                json!({ "id": id, "jsonrpc": "2.0", "method": method, "params": params })
            }
            Self::Response { id, result, error } => {
                json!({ "id": id, "jsonrpc": "2.0", "error": error, "result": result })
            }
            Self::Notification { method, params } => {
                json!({ "jsonrpc": "2.0", "method": method, "params": params })
            }
        }
    }

    /// The message from JSON: an object with `method` is a request when it has a non-null `id`,
    /// otherwise a notification; an object with `id` and without `method` is a response.
    pub fn from_json(v: &Value) -> Result<Self, MessageError> {
        let obj: &Map<String, Value> = v.as_object().ok_or(MessageError::Shape("not an object"))?;
        let id = obj.get("id").filter(|i| !i.is_null()).cloned();
        if let Some(method) = obj.get("method") {
            let method = method
                .as_str()
                .ok_or(MessageError::Shape("method is not a string"))?
                .to_owned();
            let params = obj.get("params").cloned().unwrap_or(Value::Null);
            return Ok(match id {
                Some(id) => Self::Request { id, method, params },
                None => Self::Notification { method, params },
            });
        }
        let id = id.ok_or(MessageError::Shape("neither method nor id"))?;
        let result = obj.get("result").filter(|r| !r.is_null()).cloned();
        let error = match obj.get("error").filter(|e| !e.is_null()) {
            None => None,
            Some(e) => Some(from_value::<RpcError>("error", e)?),
        };
        Ok(Self::Response { id, result, error })
    }
}

/// One message as a line (without the trailing `\n`, which the codec adds).
#[must_use]
pub fn encode_line(message: &Message) -> String {
    message.to_json().to_string()
}

/// One received line as a message.
pub fn decode_line(line: &str) -> Result<Message, MessageError> {
    let v: Value = serde_json::from_str(line).map_err(|e| MessageError::Json(e.to_string()))?;
    Message::from_json(&v)
}

/// The line codec of the protocol: `\n`-separated, at most [`MAX_LINE`] bytes per line.
#[must_use]
pub fn codec() -> LinesCodec {
    LinesCodec::new_with_max_length(MAX_LINE)
}

#[cfg(test)]
mod tests {
    use futures::{SinkExt, StreamExt};
    use tokio_util::codec::{FramedRead, FramedWrite};

    use super::*;

    fn job() -> JobParams {
        JobParams {
            job_id: "j1".into(),
            blob: format!("0700{}{}", "01", "33".repeat(32)),
            target: "ffffffff".into(),
            height: Some(42),
            algo: None,
        }
    }

    fn samples() -> Vec<Message> {
        vec![
            Message::login(
                1,
                &LoginParams {
                    login: "SEKRpayout".into(),
                    pass: "x".into(),
                    agent: Some(concat!("xkr-miner/", env!("CARGO_PKG_VERSION")).into()),
                    rigid: Some("rig".into()),
                },
            ),
            Message::login_ok(
                json!(1),
                &LoginResult {
                    id: "sess".into(),
                    job: job(),
                    status: "OK".into(),
                },
            ),
            Message::job(&job()),
            Message::submit(
                2,
                &SubmitParams {
                    id: "sess".into(),
                    job_id: "j1".into(),
                    nonce: "01020304".into(),
                    result: "00".repeat(32),
                },
            ),
            Message::status(json!(2), "OK"),
            Message::error(json!(3), -1, "Duplicate share"),
            Message::keepalived(4, "sess"),
            Message::status(json!(4), "KEEPALIVED"),
        ]
    }

    /// Messages written through the line codec come back equal, one per line.
    #[tokio::test]
    async fn framing_round_trip() {
        let (a, b) = tokio::io::duplex(1024);
        let mut out = FramedWrite::new(a, codec());
        let mut lines = FramedRead::new(b, codec());
        let sent = samples();
        for m in &sent {
            out.send(encode_line(m)).await.unwrap();
        }
        drop(out);
        let mut got = Vec::new();
        while let Some(line) = lines.next().await {
            let line = line.unwrap();
            assert!(!line.contains('\n'));
            got.push(decode_line(&line).unwrap());
        }
        assert_eq!(got, sent);
    }

    #[test]
    fn typed_params_and_results() {
        let m = samples();
        let login: LoginParams = m[0].params().unwrap();
        assert_eq!(login.rigid.as_deref(), Some("rig"));
        let result: LoginResult = m[1].result().unwrap();
        assert_eq!(result.job, job());
        assert_eq!(m[2].method(), Some("job"));
        assert_eq!(m[2].params::<JobParams>().unwrap().height, Some(42));
        let submit: SubmitParams = m[3].params().unwrap();
        assert_eq!(submit.nonce, "01020304");
        assert_eq!(m[4].result::<StatusResult>().unwrap().status, "OK");
        assert!(m[5].result::<StatusResult>().is_err());
        assert!(m[0].result::<StatusResult>().is_err());
        assert!(matches!(m[2], Message::Notification { .. }));
        // A job without the optional fields, as pools send it.
        let bare = decode_line(
            r#"{"jsonrpc":"2.0","method":"job","params":{"job_id":"x","blob":"07","target":"ff"}}"#,
        )
        .unwrap();
        let j: JobParams = bare.params().unwrap();
        assert_eq!((j.height, j.algo), (None, None));
        assert!(decode_line("[1]").is_err());
        assert!(decode_line("nope").is_err());
        assert!(decode_line(r#"{"result":{}}"#).is_err());
    }
}
