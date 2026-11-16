//! `/json_rpc` calls for miners, pools and block explorers' headers (`src/rpc/rpc_server.cpp`
//! `processJsonRpcRequest`; xkrd `crates/xkr-rpc/src/jsonrpc.rs` and `mining.rs`).
//!
//! `getblocktemplate` with `reserve_size` and `reserved_offset` is described in
//! `docs/mining.md`; a pool writes its extra nonce into the `reserve_size` bytes at
//! `reserved_offset` of [`BlockTemplate::blob`].

use serde::Deserialize;
use serde_json::{Value, json};
use xkr_types::Hash;
use xkr_wallet_core::Address;

use crate::types::BlockTemplateWire;
use crate::{BlockHeader, BlockTemplate, DaemonClient, DaemonError, Transport, field, parse};

/// JSON-RPC error codes (`src/rpc/core_rpc_server_error_codes.h`, `src/rpc/json_rpc.h`), as
/// [`DaemonError::Rpc`] carries them.
pub mod codes {
    /// `CORE_RPC_ERROR_CODE_WRONG_PARAM`.
    pub const WRONG_PARAM: i64 = -1;
    /// `CORE_RPC_ERROR_CODE_TOO_BIG_HEIGHT`.
    pub const TOO_BIG_HEIGHT: i64 = -2;
    /// `CORE_RPC_ERROR_CODE_TOO_BIG_RESERVE_SIZE`.
    pub const TOO_BIG_RESERVE_SIZE: i64 = -3;
    /// `CORE_RPC_ERROR_CODE_WRONG_WALLET_ADDRESS`.
    pub const WRONG_WALLET_ADDRESS: i64 = -4;
    /// `CORE_RPC_ERROR_CODE_INTERNAL_ERROR`.
    pub const INTERNAL_ERROR: i64 = -5;
    /// `CORE_RPC_ERROR_CODE_WRONG_BLOCKBLOB`.
    pub const WRONG_BLOCKBLOB: i64 = -6;
    /// `CORE_RPC_ERROR_CODE_BLOCK_NOT_ACCEPTED`.
    pub const BLOCK_NOT_ACCEPTED: i64 = -7;
    /// `CORE_RPC_ERROR_CODE_CORE_BUSY` (C++ only, while the core is not ready).
    pub const CORE_BUSY: i64 = -9;
    /// `json_rpc::errParseError`.
    pub const PARSE_ERROR: i64 = -32700;
    /// `json_rpc::errInvalidRequest`.
    pub const INVALID_REQUEST: i64 = -32600;
    /// `json_rpc::errMethodNotFound`.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// `json_rpc::errInvalidParams`.
    pub const INVALID_PARAMS: i64 = -32602;
    /// `json_rpc::errInternalError`.
    pub const INTERNAL: i64 = -32603;
}

/// `getblockcount` (`COMMAND_RPC_GETBLOCKCOUNT::response`).
#[derive(Debug, Deserialize)]
struct BlockCount {
    count: u64,
}

/// `BLOCK_HEADER_RESPONSE`.
#[derive(Debug, Deserialize)]
struct HeaderAnswer {
    block_header: BlockHeader,
}

impl<T: Transport> DaemonClient<T> {
    /// `getblockcount` (`RpcServer::on_getblockcount`): top block index + 1.
    pub async fn block_count(&self) -> Result<u64, DaemonError> {
        let r: BlockCount = self.json_rpc_ok("getblockcount", json!({})).await?;
        Ok(r.count)
    }

    /// `getlastblockheader` (`RpcServer::on_get_last_block_header`): the top block.
    pub async fn last_block_header(&self) -> Result<BlockHeader, DaemonError> {
        let r: HeaderAnswer = self.json_rpc_ok("getlastblockheader", json!({})).await?;
        Ok(r.block_header)
    }

    /// `getblockheaderbyheight` (`RpcServer::on_get_block_header_by_height`): the main-chain
    /// block at index `height` (0 = genesis). [`codes::TOO_BIG_HEIGHT`] above the top block.
    pub async fn block_header_by_height(&self, height: u64) -> Result<BlockHeader, DaemonError> {
        let params = json!({ "height": height });
        let r: HeaderAnswer = self.json_rpc_ok("getblockheaderbyheight", params).await?;
        Ok(r.block_header)
    }

    /// `getblockheaderbyhash` (`RpcServer::on_get_block_header_by_hash`).
    pub async fn block_header_by_hash(&self, hash: &Hash) -> Result<BlockHeader, DaemonError> {
        let params = json!({ "hash": hash.to_hex() });
        let r: HeaderAnswer = self.json_rpc_ok("getblockheaderbyhash", params).await?;
        Ok(r.block_header)
    }

    /// `get_block_headers_range` (`RpcServer::on_get_block_headers_range_json`): main-chain
    /// headers `start_height..=end_height` (both at most the top index, at most 1 000 blocks).
    pub async fn block_headers_range(
        &self,
        start_height: u64,
        end_height: u64,
    ) -> Result<Vec<BlockHeader>, DaemonError> {
        let params = json!({ "start_height": start_height, "end_height": end_height });
        let result: Value = self.json_rpc_ok("get_block_headers_range", params).await?;
        parse("get_block_headers_range", field(result, "headers"))
    }

    /// `getblocktemplate` (`RpcServer::on_getblocktemplate`): a block paying `address`, with
    /// `reserve_size` free bytes in the coinbase `extra` for the pool's nonce.
    pub async fn block_template(
        &self,
        address: &Address,
        reserve_size: u8,
    ) -> Result<BlockTemplate, DaemonError> {
        let params = json!({
            "reserve_size": reserve_size,
            "wallet_address": address.to_string(),
        });
        let wire: BlockTemplateWire = self.json_rpc_ok("getblocktemplate", params).await?;
        let blob = hex::decode(wire.blocktemplate_blob.trim()).map_err(|e| {
            DaemonError::Parse(format!("getblocktemplate: blocktemplate_blob: {e}"))
        })?;
        Ok(BlockTemplate {
            blob,
            difficulty: wire.difficulty,
            height: wire.height,
            reserved_offset: wire.reserved_offset,
        })
    }

    /// `submitblock` (`RpcServer::on_submitblock`): a mined block (the template blob with the
    /// nonce and extra nonce filled in). [`codes::BLOCK_NOT_ACCEPTED`] when the node refuses
    /// it, [`codes::WRONG_BLOCKBLOB`] when it does not parse.
    pub async fn submit_block(&self, blob: &[u8]) -> Result<(), DaemonError> {
        let params = json!([hex::encode(blob)]);
        let _: Value = self.json_rpc_ok("submitblock", params).await?;
        Ok(())
    }
}
