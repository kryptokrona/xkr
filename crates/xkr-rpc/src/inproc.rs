//! One HTTP request through the node's router without a socket: the wallet service of the
//! clients and wallets topics (`crates/xkrd/src/clients.rs`) answers wallets with exactly the
//! responses of the HTTP server.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, header};
use serde_json::Value;
use tower::ServiceExt as _;

use crate::{AccessPolicy, RpcState, router_with_access};

/// Largest response body read back, in bytes.
pub const MAX_RESPONSE_BYTES: usize = 16 << 20;

/// An answer: the HTTP status and the body (JSON when it parses, else a string).
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// HTTP status code.
    pub status: u16,
    /// The body.
    pub body: Value,
}

/// `method path` with `body` (JSON, for `POST`) through the router over `state`, as a request
/// from `localhost` would reach it (not counted as a loopback client for `/status` details).
///
/// # Errors
/// A method other than `GET`/`POST`, a path that is not a valid URI, or a response body larger
/// than [`MAX_RESPONSE_BYTES`].
pub async fn request(
    state: Arc<RpcState>,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Answer, String> {
    let method = match method {
        "GET" => Method::GET,
        "POST" => Method::POST,
        other => return Err(format!("method {other} is not GET or POST")),
    };
    let body = match body {
        Some(v) => Body::from(v.to_string()),
        None => Body::empty(),
    };
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .map_err(|e| format!("bad request: {e}"))?;
    let access = AccessPolicy::new(std::net::Ipv4Addr::LOCALHOST.into(), &[], &[]);
    let resp = router_with_access(state, access)
        .oneshot(req)
        .await
        .map_err(|e| format!("response: {e}"))?;
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| format!("response: {e}"))?;
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    Ok(Answer { status, body })
}
