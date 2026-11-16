//! `POST /json_rpc` (`RpcServer::processJsonRpcRequest`).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::handlers::{ApiError, STATUS_OK, block_header, info_json, parse_hash, top_index};
use crate::{RpcState, explorer, mining};

/// JSON-RPC 2.0 errors (`json_rpc.h`).
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INTERNAL: i64 = -32603;
/// `core_rpc_server_error_codes.h`.
const CORE_RPC_ERROR_CODE_WRONG_PARAM: i64 = -1;
const CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT: i64 = -2;
const CORE_RPC_ERROR_CODE_INTERNAL_ERROR: i64 = -5;

/// A JSON-RPC error object.
pub(crate) struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ApiError> for RpcError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::BadRequest(m) => Self::new(CORE_RPC_ERROR_CODE_WRONG_PARAM, m),
            ApiError::Internal(m) => Self::new(ERR_INTERNAL, m),
            // Not a node failure: the item is pruned here (another node can answer it).
            ApiError::Pruned(h) => {
                Self::new(CORE_RPC_ERROR_CODE_WRONG_PARAM, ApiError::pruned_message(h))
            }
        }
    }
}

fn respond(id: &Value, result: Result<Value, RpcError>) -> Response {
    let body = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": e.code, "message": e.message } })
        }
    };
    Json(body).into_response()
}

/// The JSON-RPC endpoint.
pub async fn json_rpc(State(s): State<Arc<RpcState>>, body: Bytes) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return respond(&Value::Null, Err(RpcError::new(ERR_PARSE, "Parse error"))),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(Value::as_str).map(str::to_owned) else {
        return respond(
            &id,
            Err(RpcError::new(ERR_INVALID_REQUEST, "Invalid request")),
        );
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = tokio::task::spawn_blocking(move || dispatch(&s, &method, &params))
        .await
        .unwrap_or_else(|e| Err(RpcError::new(ERR_INTERNAL, e.to_string())));
    respond(&id, result)
}

fn param_u64(params: &Value, key: &str) -> Option<u64> {
    params.get(key).and_then(Value::as_u64)
}

fn dispatch(s: &RpcState, method: &str, params: &Value) -> Result<Value, RpcError> {
    let chain = &s.chain;
    match method {
        "getblockcount" => Ok(json!({ "count": chain.tip().height + 1, "status": STATUS_OK })),
        "on_getblockhash" => {
            let height = params
                .as_array()
                .filter(|a| a.len() == 1)
                .and_then(|a| a[0].as_u64())
                .ok_or_else(|| {
                    RpcError::new(
                        CORE_RPC_ERROR_CODE_WRONG_PARAM,
                        "Wrong parameters, expected height",
                    )
                })?;
            let reader = chain.reader().map_err(ApiError::from)?;
            let top = top_index(&reader)?;
            let hash = height
                .checked_sub(1)
                .map(|index| reader.block_hash(index))
                .transpose()
                .map_err(ApiError::from)?
                .flatten();
            match hash {
                Some(h) => Ok(json!(h.to_string())),
                None => Err(RpcError::new(
                    CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT,
                    format!(
                        "Too big height: {height}, current blockchain height = {}",
                        top + 1
                    ),
                )),
            }
        }
        "getlastblockheader" => {
            let reader = chain.reader().map_err(ApiError::from)?;
            let top = top_index(&reader)?;
            Ok(
                json!({ "block_header": block_header(chain, &reader, top, top)?, "status": STATUS_OK }),
            )
        }
        "getblockheaderbyhash" => {
            let text = params
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let hash = parse_hash(text).ok_or_else(|| {
                RpcError::new(
                    CORE_RPC_ERROR_CODE_WRONG_PARAM,
                    format!("Failed to parse hex representation of block hash. Hex = {text}."),
                )
            })?;
            let reader = chain.reader().map_err(ApiError::from)?;
            let top = top_index(&reader)?;
            let Some(height) = reader.height_of(&hash).map_err(ApiError::from)? else {
                return Err(RpcError::new(
                    CORE_RPC_ERROR_CODE_INTERNAL_ERROR,
                    format!("Internal error: can't get block by hash. Hash = {text}."),
                ));
            };
            Ok(
                json!({ "block_header": block_header(chain, &reader, height, top)?, "status": STATUS_OK }),
            )
        }
        "getblockheaderbyheight" => {
            let height = param_u64(params, "height").ok_or_else(|| {
                RpcError::new(
                    CORE_RPC_ERROR_CODE_WRONG_PARAM,
                    "Wrong parameters, expected height",
                )
            })?;
            let reader = chain.reader().map_err(ApiError::from)?;
            let top = top_index(&reader)?;
            if height > top {
                return Err(RpcError::new(
                    CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT,
                    format!("To big height: {height}, current blockchain height = {top}"),
                ));
            }
            Ok(
                json!({ "block_header": block_header(chain, &reader, height, top)?, "status": STATUS_OK }),
            )
        }
        "get_info" => Ok(info_json(s)?),
        "getcurrencyid" => Ok(json!({ "currency_id_blob": chain.genesis_hash().to_string() })),
        "getblocktemplate" => mining::get_block_template(s, params),
        "submitblock" => mining::submit_block(s, params),
        "f_blocks_list_json" => explorer::blocks_list(s, params),
        "f_block_json" => explorer::block_details(s, params),
        "f_transaction_json" => explorer::transaction_details(s, params),
        "f_on_transactions_pool_json" => Ok(explorer::pool(s)),
        "get_miner_data" => mining::get_miner_data(s),
        "get_block_headers_range" => mining::block_headers_range(s, params),
        _ => Err(RpcError::new(ERR_METHOD_NOT_FOUND, "Method not found")),
    }
}
