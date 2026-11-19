//! Daemon requests over the wallets topic (`docs/hyper-transport.md`, "Wallets topic"): a
//! [`Transport`] that sends each request as one `http` call of service `wallet` on an
//! `xkr-hyper-rpc` connection, and [`AnyTransport`], which lets one wallet switch between a
//! daemon over HTTP and one over Hyperswarm.
//!
//! Where the connection comes from is a [`ClientSource`]: the Hyperswarm lookup lives in
//! `xkr-hyper-wallet`, tests use in-memory streams. A closed connection is opened again on the
//! next request (once per request).

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use xkr_hyper_rpc::{Client, ClientError};

use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method, Transport, TransportError};

/// The method of service `wallet` that carries one daemon HTTP request.
pub const WALLET_HTTP: &str = "http";

/// Opens connections with service `wallet` (hello done) to a node.
pub trait ClientSource: Send + Sync {
    /// A new connection; the text names the node for logs and status.
    fn connect(&self) -> BoxFuture<'static, Result<(Client, String), TransportError>>;
}

/// A [`Transport`] over the wallets topic.
#[derive(Clone)]
pub struct SwarmTransport {
    source: Arc<dyn ClientSource>,
    current: Arc<Mutex<Option<(Client, String)>>>,
}

impl fmt::Debug for SwarmTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwarmTransport").finish_non_exhaustive()
    }
}

impl SwarmTransport {
    /// A transport that connects through `source` on its first request.
    #[must_use]
    pub fn new(source: Arc<dyn ClientSource>) -> Self {
        Self {
            source,
            current: Arc::new(Mutex::new(None)),
        }
    }

    /// A transport over an open connection; later connections come from `source`.
    #[must_use]
    pub fn with_client(source: Arc<dyn ClientSource>, client: Client, node: String) -> Self {
        Self {
            source,
            current: Arc::new(Mutex::new(Some((client, node)))),
        }
    }

    /// The node of the open connection, if one is open.
    pub async fn node(&self) -> Option<String> {
        self.current
            .lock()
            .await
            .as_ref()
            .filter(|(c, _)| !c.is_closed())
            .map(|(_, n)| n.clone())
    }

    async fn client(&self) -> Result<Client, TransportError> {
        let mut current = self.current.lock().await;
        if let Some((c, _)) = current.as_ref()
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        let (client, node) = self.source.connect().await?;
        tracing::info!(%node, "wallet daemon over Hyperswarm");
        *current = Some((client.clone(), node));
        Ok(client)
    }

    async fn forget(&self) {
        *self.current.lock().await = None;
    }

    async fn send_once(&self, params: &Value) -> Result<HttpResponse, Retry> {
        let client = self.client().await.map_err(Retry::No)?;
        match client.call(WALLET_HTTP, params.clone()).await {
            Ok(answer) => response(&answer).map_err(Retry::No),
            Err(ClientError::Rpc(e)) if e.code == xkr_hyper_rpc::code::TOO_LARGE => Err(Retry::No(
                TransportError::TooLarge(xkr_hyper_rpc::MAX_FRAME),
            )),
            Err(ClientError::Rpc(e)) => Err(Retry::No(TransportError::Connection(e.to_string()))),
            Err(e) => {
                self.forget().await;
                Err(Retry::Yes(TransportError::Connection(e.to_string())))
            }
        }
    }
}

enum Retry {
    Yes(TransportError),
    No(TransportError),
}

impl Transport for SwarmTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let params = request_params(&request)?;
        match self.send_once(&params).await {
            Ok(r) => Ok(r),
            Err(Retry::No(e)) => Err(e),
            Err(Retry::Yes(_)) => match self.send_once(&params).await {
                Ok(r) => Ok(r),
                Err(Retry::Yes(e) | Retry::No(e)) => Err(e),
            },
        }
    }
}

/// The `http` call parameters of `request`.
fn request_params(request: &HttpRequest) -> Result<Value, TransportError> {
    let method = match request.method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    let body = if request.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&request.body)
            .map_err(|e| TransportError::Connection(format!("request body is not JSON: {e}")))?
    };
    Ok(json!({"method": method, "path": request.path, "body": body}))
}

/// The HTTP response of an `http` answer `{"status", "body"}`.
fn response(answer: &Value) -> Result<HttpResponse, TransportError> {
    let status = answer
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .ok_or_else(|| TransportError::Connection("answer without a status".to_owned()))?;
    let body = match answer.get("body") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => s.clone().into_bytes(),
        Some(v) => v.to_string().into_bytes(),
    };
    Ok(HttpResponse { status, body })
}

/// A daemon over HTTP or over the wallets topic, so a wallet can switch between the two.
#[derive(Debug, Clone)]
pub enum AnyTransport {
    /// A daemon at an address.
    Http(HttpTransport),
    /// A node found on Hyperswarm.
    Swarm(SwarmTransport),
}

impl Transport for AnyTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        match self {
            Self::Http(t) => t.send(request).await,
            Self::Swarm(t) => t.send(request).await,
        }
    }
}

impl From<HttpTransport> for AnyTransport {
    fn from(t: HttpTransport) -> Self {
        Self::Http(t)
    }
}

impl From<SwarmTransport> for AnyTransport {
    fn from(t: SwarmTransport) -> Self {
        Self::Swarm(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_become_http_calls() {
        let get = HttpRequest {
            method: Method::Get,
            path: "/info".into(),
            body: Vec::new(),
        };
        assert_eq!(
            request_params(&get).unwrap(),
            json!({"method": "GET", "path": "/info", "body": null})
        );
        let post = HttpRequest {
            method: Method::Post,
            path: "/getwalletsyncdata".into(),
            body: br#"{"startHeight":5}"#.to_vec(),
        };
        assert_eq!(request_params(&post).unwrap()["body"]["startHeight"], 5);
        let bad = HttpRequest {
            method: Method::Post,
            path: "/x".into(),
            body: b"not json".to_vec(),
        };
        assert!(request_params(&bad).is_err());
    }

    #[test]
    fn answers_become_http_responses() {
        let r = response(&json!({"status": 200, "body": {"height": 7}})).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&r.body).unwrap()["height"],
            7
        );
        let text = response(&json!({"status": 404, "body": "not found"})).unwrap();
        assert_eq!((text.status, text.body), (404, b"not found".to_vec()));
        assert!(response(&json!({"body": 1})).is_err());
    }
}
