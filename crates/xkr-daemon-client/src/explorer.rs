//! Explorer routes (`src/rpc/rpc_server.cpp` `onGetBlockDetailsByHeight`,
//! `onGetBlocksDetailsByHeights`, `onGetBlocksDetailsByHashes`,
//! `onGetTransactionDetailsByHashes`, `onGetTransactionHashesByPaymentId`; xkrd
//! `crates/xkr-rpc/src/details.rs`).
//!
//! Requests are typed; block and transaction details are returned as JSON
//! (`BlockDetails` / `TransactionDetails` of `blockchain_explorer_data_serialization.cpp`, with
//! the xkr additions listed in `crates/xkr-rpc/src/details.rs`) until the explorer job gives
//! them types. A failure is [`DaemonError::Status`] with the node's `"Error: ..."` text.
//! The `f_*` JSON-RPC methods go through [`DaemonClient::json_rpc`].

use serde_json::{Value, json};
use xkr_types::Hash;

use crate::types::hex_list;
use crate::{DaemonClient, DaemonError, Transport, field, hex_strings, parse};

/// `transactionHashes` of `/get_transaction_hashes_by_payment_id`.
#[derive(Debug, serde::Deserialize)]
struct HashList {
    #[serde(
        rename = "transactionHashes",
        deserialize_with = "hex_list::deserialize"
    )]
    hashes: Vec<Hash>,
}

impl<T: Transport> DaemonClient<T> {
    /// `/get_block_details_by_height` `{ blockHeight }`: the `block` object.
    pub async fn get_block_details_by_height(&self, height: u64) -> Result<Value, DaemonError> {
        let body = json!({ "blockHeight": height });
        let answer = self.post_ok("/get_block_details_by_height", &body).await?;
        Ok(field(answer, "block"))
    }

    /// `/get_blocks_details_by_heights` `{ blockHeights }`: the `blocks` list.
    pub async fn get_blocks_details_by_heights(
        &self,
        heights: &[u64],
    ) -> Result<Vec<Value>, DaemonError> {
        let body = json!({ "blockHeights": heights });
        let answer = self
            .post_ok("/get_blocks_details_by_heights", &body)
            .await?;
        parse("/get_blocks_details_by_heights", field(answer, "blocks"))
    }

    /// `/get_blocks_details_by_hashes` `{ blockHashes }`: the `blocks` list.
    pub async fn get_blocks_details_by_hashes(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<Value>, DaemonError> {
        let body = json!({ "blockHashes": hex_strings(hashes) });
        let answer = self.post_ok("/get_blocks_details_by_hashes", &body).await?;
        parse("/get_blocks_details_by_hashes", field(answer, "blocks"))
    }

    /// `/get_transaction_details_by_hashes` `{ transactionHashes }`: the `transactions` list.
    pub async fn get_transaction_details_by_hashes(
        &self,
        hashes: &[Hash],
    ) -> Result<Vec<Value>, DaemonError> {
        let body = json!({ "transactionHashes": hex_strings(hashes) });
        let answer = self
            .post_ok("/get_transaction_details_by_hashes", &body)
            .await?;
        parse(
            "/get_transaction_details_by_hashes",
            field(answer, "transactions"),
        )
    }

    /// `/get_transaction_hashes_by_payment_id` `{ paymentId }`: hashes of the transactions
    /// carrying `payment_id`.
    pub async fn get_transaction_hashes_by_payment_id(
        &self,
        payment_id: &Hash,
    ) -> Result<Vec<Hash>, DaemonError> {
        let body = json!({ "paymentId": payment_id.to_hex() });
        let answer = self
            .post_ok("/get_transaction_hashes_by_payment_id", &body)
            .await?;
        let list: HashList = parse("/get_transaction_hashes_by_payment_id", answer)?;
        Ok(list.hashes)
    }
}
