//! The calls a wallet makes (C++ `src/nigel/nigel.cpp`), plus `/gettransactions`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};
use xkr_types::Hash;

use crate::types::{TransactionsWire, hex_field};
use crate::{
    DaemonClient, DaemonError, Info, NodeFee, RandomOuts, STATUS_OK, SendOutcome, Transactions,
    TransactionsStatus, Transport, WalletBlock, WalletSyncRequest, field, hex_strings, parse,
    require_ok,
};

/// One `{ "key": hash, "value": [indexes] }` of `/get_global_indexes_for_range` (the C++
/// serialization of `std::unordered_map<crypto::Hash, std::vector<uint64_t>>`).
#[derive(Debug, Deserialize)]
struct IndexEntry {
    #[serde(with = "hex_field")]
    key: Hash,
    value: Vec<u64>,
}

impl<T: Transport> DaemonClient<T> {
    /// `GET /info` (`Nigel::getDaemonInfo`; `/getinfo` is the same handler). The answer must
    /// have `status` `"OK"`. [`Info::tx_versions`] is `None` on a C++ node.
    pub async fn info(&self) -> Result<Info, DaemonError> {
        let answer = self.get("/info").await?;
        require_ok(&answer)?;
        parse("/info", answer)
    }

    /// `GET /fee` (`Nigel::getFeeInfo`): the node operator's fee as the node states it; see
    /// [`NodeFee::payable`] for the fee a transaction pays.
    pub async fn fee(&self) -> Result<NodeFee, DaemonError> {
        let answer = self.get("/fee").await?;
        require_ok(&answer)?;
        parse("/fee", answer)
    }

    /// `/getwalletsyncdata` (`Nigel::getWalletSyncData`): up to 100 blocks after the first
    /// checkpoint on the node's main chain, from `start_height` / `start_timestamp` on. Sends
    /// `blockHashCheckpoints`, `startHeight`, `startTimestamp` and, when set, `blockCount`.
    /// [`DaemonError::Status`] with `"Failed to perform query"` when no checkpoint is on the
    /// node's main chain.
    pub async fn wallet_sync_data(
        &self,
        request: &WalletSyncRequest,
    ) -> Result<Vec<WalletBlock>, DaemonError> {
        let mut body = json!({
            "blockHashCheckpoints": hex_strings(&request.checkpoints),
            "startHeight": request.start_height,
            "startTimestamp": request.start_timestamp,
        });
        if let Some(count) = request.block_count {
            body["blockCount"] = json!(count);
        }
        let answer = self.post_ok("/getwalletsyncdata", &body).await?;
        parse("/getwalletsyncdata", field(answer, "items"))
    }

    /// `/get_global_indexes_for_range` (`Nigel::getGlobalIndexesForRange`): the global output
    /// indexes of every transaction in the blocks `start_height..end_height` (end exclusive).
    /// xkrd answers at most 10 000 blocks from `start_height`; ask again for the rest.
    pub async fn global_indexes_for_range(
        &self,
        start_height: u64,
        end_height: u64,
    ) -> Result<HashMap<Hash, Vec<u64>>, DaemonError> {
        let body = json!({ "startHeight": start_height, "endHeight": end_height });
        let answer = self.post_ok("/get_global_indexes_for_range", &body).await?;
        let entries: Vec<IndexEntry> =
            parse("/get_global_indexes_for_range", field(answer, "indexes"))?;
        Ok(entries.into_iter().map(|e| (e.key, e.value)).collect())
    }

    /// `/getrandom_outs` (`Nigel::getRandomOutsByAmounts`): up to `outs_count` unlocked outputs
    /// per amount, for ring members. [`DaemonError::Status`] (`"Failed"`) when an amount has no
    /// usable outputs; a short list is not an error (the caller checks the counts, as C++
    /// `getRingParticipants` does).
    pub async fn random_outs(
        &self,
        amounts: &[u64],
        outs_count: u16,
    ) -> Result<Vec<RandomOuts>, DaemonError> {
        let body = json!({ "amounts": amounts, "outs_count": outs_count });
        let answer = self.post_ok("/getrandom_outs", &body).await?;
        parse("/getrandom_outs", field(answer, "outs"))
    }

    /// `/sendrawtransaction` (`Nigel::sendTransaction`): hands a transaction blob to the node's
    /// pool. A refusal is [`SendOutcome::Failed`]; only an unreachable node or a malformed
    /// answer is an error (C++ `connectionError`).
    pub async fn send_raw_transaction(&self, blob: &[u8]) -> Result<SendOutcome, DaemonError> {
        let body = json!({ "tx_as_hex": hex::encode(blob) });
        let answer = self.post("/sendrawtransaction", &body).await?;
        let status = answer
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| DaemonError::Parse("/sendrawtransaction: no status".to_owned()))?;
        if status != STATUS_OK {
            let reason = answer
                .get("error")
                .and_then(Value::as_str)
                .filter(|e| !e.is_empty())
                .unwrap_or(status);
            return Ok(SendOutcome::Failed(reason.to_owned()));
        }
        let hash: Hash = match answer.get("transactionHash").and_then(Value::as_str) {
            Some(text) => text.trim().parse().map_err(|e| {
                DaemonError::Parse(format!("/sendrawtransaction: transactionHash: {e}"))
            })?,
            None => xkr_serialization::transaction_hash_from_blob(blob),
        };
        Ok(SendOutcome::Ok(hash))
    }

    /// `/get_transactions_status` (`Nigel::getTransactionsStatus`): which of `hashes` are in the
    /// pool, in a block or unknown.
    pub async fn transactions_status(
        &self,
        hashes: &[Hash],
    ) -> Result<TransactionsStatus, DaemonError> {
        let body = json!({ "transactionHashes": hex_strings(hashes) });
        let answer = self.post_ok("/get_transactions_status", &body).await?;
        parse("/get_transactions_status", answer)
    }

    /// `/gettransactions` (`RpcServer::on_get_transactions`): the blobs of `hashes` from the
    /// pool or the chain, and the hashes the node does not have.
    pub async fn transactions(&self, hashes: &[Hash]) -> Result<Transactions, DaemonError> {
        let body = json!({ "txs_hashes": hex_strings(hashes) });
        let answer = self.post_ok("/gettransactions", &body).await?;
        let wire: TransactionsWire = parse("/gettransactions", answer)?;
        let blobs = wire
            .txs_as_hex
            .iter()
            .map(|h| hex::decode(h.trim()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| DaemonError::Parse(format!("/gettransactions: txs_as_hex: {e}")))?;
        Ok(Transactions {
            blobs,
            missed: wire.missed_tx,
        })
    }
}
