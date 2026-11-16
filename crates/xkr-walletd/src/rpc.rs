//! The HTTP side of walletd (`src/json_rpc_server/json_rpc_server.cpp` `JsonRpcServer`,
//! `src/wallet_service/payment_service_json_rpc_server.cpp`
//! `PaymentServiceJsonRpcServer::processJsonRpcRequest`, `src/http/http_response.cpp`).
//!
//! * Only the URL `/json_rpc` is served, with any HTTP method; anything else is `404` with the
//!   body `Requested url is not found\n`.
//! * Every answer on `/json_rpc` is `200` with compact JSON whose object keys are sorted (C++
//!   `JsonValue` objects are `std::map`s), and no `Content-Type` header. Responses carry
//!   `Server: CryptoNote-based HTTP server` and `Connection: keep-alive`.
//! * A body that does not parse as JSON is answered `{"error":{"code":-32700,"message":"Parse
//!   error"},"id":null,"jsonrpc":"2.0"}` without the CORS header.
//! * Otherwise the answer echoes `id` (when present) and `"jsonrpc":"2.0"`; then the password
//!   check (`-32604`), the method name (`-3600` if missing or not a string, `-32601` if
//!   unknown), the parameters (`-32600`) and the method (`-32700` with
//!   `data.application_code`, see [`crate::errors`]).

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::{Map, Value, json};
use xkr_daemon_client::Transport;
use xkr_types::Hash;

use crate::errors::AppError;
use crate::methods;
use crate::service::Service;

/// The only URL walletd serves.
pub const JSON_RPC_PATH: &str = "/json_rpc";
/// `HttpResponse` default `Server` header.
pub const SERVER_HEADER: &str = "CryptoNote-based HTTP server";
/// `getErrorBody(STATUS_404)`.
pub const NOT_FOUND_BODY: &str = "Requested url is not found\n";
/// `getErrorBody(STATUS_500)`.
pub const SERVER_ERROR_BODY: &str = "Internal server error is occurred\n";
/// Largest request body read.
const MAX_BODY: usize = 64 << 20;

/// `cryptonote::json_rpc::errParseError`; also the code of every method failure.
pub const ERR_PARSE: i64 = -32700;
/// `errInvalidRequest`: parameters that do not read.
pub const ERR_INVALID_REQUEST: i64 = -32600;
/// `errMethodNotFound`.
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
/// `errInvalidPassword`.
pub const ERR_INVALID_PASSWORD: i64 = -32604;
/// What `processJsonRpcRequest` answers for a missing or non-string `method`.
pub const ERR_NO_METHOD: i64 = -3600;
/// `makeGenericErrorReponse`'s default code (an exception while handling the request).
pub const ERR_GENERIC: i64 = -32001;

/// How a method failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The parameters do not read (`-32600 "Invalid Request"`).
    InvalidRequest,
    /// The method failed with an application code (`-32700`).
    App(AppError),
}

impl From<AppError> for Failure {
    fn from(e: AppError) -> Self {
        Self::App(e)
    }
}

impl From<crate::params::Invalid> for Failure {
    fn from(_: crate::params::Invalid) -> Self {
        Self::InvalidRequest
    }
}

impl From<xkr_wallet::WalletError> for Failure {
    fn from(e: xkr_wallet::WalletError) -> Self {
        Self::App(AppError::from(&e))
    }
}

/// The router: every path and method goes to [`handle`].
pub fn router<T: Transport + 'static>(service: Arc<Service<T>>) -> Router {
    Router::new().fallback(handle::<T>).with_state(service)
}

/// A response with the C++ server's headers.
fn respond(status: StatusCode, body: String, cors: Option<&str>) -> Response {
    let length = HeaderValue::from(body.len());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    // `HttpResponse::setBody`
    headers.insert(header::CONTENT_LENGTH, length);
    headers.insert(header::SERVER, HeaderValue::from_static(SERVER_HEADER));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    if let Some(origin) = cors.and_then(|c| HeaderValue::from_str(c).ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    response
}

/// `JsonRpcServer::makeJsonParsingErrorResponse`.
#[must_use]
pub fn parse_error() -> Value {
    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": ERR_PARSE, "message": "Parse error" } })
}

/// `JsonRpcServer::processRequest`.
async fn handle<T: Transport + 'static>(
    State(service): State<Arc<Service<T>>>,
    request: Request,
) -> Response {
    let url = request
        .uri()
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .to_owned();
    if url != JSON_RPC_PATH {
        tracing::warn!("Requested url \"{url}\" is not found");
        return respond(StatusCode::NOT_FOUND, NOT_FOUND_BODY.to_owned(), None);
    }
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY).await else {
        return respond(
            StatusCode::INTERNAL_SERVER_ERROR,
            SERVER_ERROR_BODY.to_owned(),
            None,
        );
    };
    // `jsonInputStream >> jsonRpcRequest` reads one value and ignores what follows
    let parsed = serde_json::Deserializer::from_slice(&body)
        .into_iter::<Value>()
        .next();
    let Some(Ok(request)) = parsed else {
        tracing::debug!(
            "Couldn't parse request: \"{}\"",
            String::from_utf8_lossy(&body)
        );
        return respond(StatusCode::OK, parse_error().to_string(), None);
    };
    let answer = process(&service, &request).await;
    let cors = service.rpc().cors.clone();
    respond(
        StatusCode::OK,
        answer.to_string(),
        (!cors.is_empty()).then_some(cors.as_str()),
    )
}

/// `makeGenericErrorReponse`.
fn generic_error(response: &mut Map<String, Value>, message: &str, code: i64) {
    response
        .entry("error")
        .or_insert_with(|| json!({ "code": code, "message": message }));
}

/// Whether `password` hashes to the configured secret (`cn_slow_hash_v0`, run off the async
/// threads).
async fn password_matches(password: String, secret: Hash) -> bool {
    tokio::task::spawn_blocking(move || {
        let hash = xkr_crypto::cn_slow_hash_v0(password.as_bytes());
        hash.0
            .iter()
            .zip(secret.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    })
    .await
    .unwrap_or(false)
}

/// `PaymentServiceJsonRpcServer::processJsonRpcRequest` for a parsed request.
pub async fn process<T: Transport + 'static>(service: &Service<T>, request: &Value) -> Value {
    let mut response = Map::new();
    let Some(fields) = request.as_object() else {
        // `req.contains("id")` throws on anything but an object
        generic_error(&mut response, "JsonValue type is not OBJECT", ERR_GENERIC);
        return Value::Object(response);
    };
    if let Some(id) = fields.get("id") {
        response.insert("id".into(), id.clone());
    }
    response.insert("jsonrpc".into(), json!("2.0"));

    if !service.rpc().legacy_security {
        let ok = if let Some(Value::String(p)) = fields.get("password") {
            password_matches(p.clone(), service.rpc().secret).await
        } else {
            false
        };
        if !ok {
            generic_error(
                &mut response,
                "Invalid or no rpc password",
                ERR_INVALID_PASSWORD,
            );
            return Value::Object(response);
        }
    }

    let Some(Value::String(method)) = fields.get("method") else {
        tracing::warn!("Field \"method\" is missing or not a string: {request}");
        generic_error(&mut response, "Invalid Request", ERR_NO_METHOD);
        return Value::Object(response);
    };
    if !methods::is_method(method) {
        tracing::warn!("Requested method not found: {method}");
        generic_error(&mut response, "Method not found", ERR_METHOD_NOT_FOUND);
        return Value::Object(response);
    }
    tracing::debug!("{method} request came");
    let empty = Value::Object(Map::new());
    let params = fields.get("params").unwrap_or(&empty);
    match methods::call(service, method, params).await {
        Ok(result) => {
            response.insert("result".into(), result);
        }
        Err(Failure::InvalidRequest) => {
            generic_error(&mut response, "Invalid Request", ERR_INVALID_REQUEST);
        }
        Err(Failure::App(e)) => {
            tracing::warn!("{method}: {e}");
            response.insert(
                "error".into(),
                json!({
                    "code": ERR_PARSE,
                    "message": e.message,
                    "data": { "application_code": e.code },
                }),
            );
        }
    }
    Value::Object(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_body_is_the_cpp_one() {
        let text = parse_error().to_string();
        assert_eq!(
            text,
            r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#
        );
        assert_eq!(text.len(), 75);
    }
}
