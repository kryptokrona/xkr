//! The client against answers in the exact shape of the C++ daemon (`src/rpc/rpc_server.cpp`,
//! `core_rpc_server_commands_definitions.h`): no `tx_versions`, no view tags, a bare
//! `{"status":"OK"}` from `/sendrawtransaction`. Also checks that requests carry the keys the
//! C++ wallet sends (`src/nigel/nigel.cpp`) and how failures map to [`DaemonError`].

mod common;

use common::{Fake, walletsync_fixture};
use serde_json::{Value, json};
use xkr_daemon_client::mining::codes;
use xkr_daemon_client::{
    DaemonClient, DaemonError, Method, SendOutcome, TransportError, WalletSyncRequest,
};
use xkr_types::Hash;
use xkr_wallet_core::Address;

fn client() -> DaemonClient<Fake> {
    DaemonClient::new(Fake::default())
}

fn fake(c: &DaemonClient<Fake>) -> &Fake {
    c.transport()
}

/// The keys of a JSON object, sorted.
fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
    k.sort();
    k
}

/// `COMMAND_RPC_GET_INFO::response` of a C++ node, every field and nothing else.
fn cpp_info() -> Value {
    json!({
        "status": "OK",
        "height": 1_500_001,
        "difficulty": 2_400_000,
        "tx_count": 750_000,
        "tx_pool_size": 2,
        "alt_blocks_count": 1,
        "outgoing_connections_count": 8,
        "incoming_connections_count": 3,
        "white_peerlist_size": 40,
        "grey_peerlist_size": 300,
        "last_known_block_index": 1_500_000,
        "network_height": 1_500_001,
        "upgrade_heights": [187_000, 350_000, 440_000, 620_000, 700_000, 800_000, 1_000_000,
            1_200_000, 1_300_000, 1_400_000, 1_600_000, 1_800_000, 2_200_000],
        "supported_height": 2_200_000,
        "hashrate": 26_666,
        "major_version": 6,
        "minor_version": 0,
        "start_time": 1_700_000_000,
        "synced": true,
        "testnet": false,
        "version": "1.1.4",
    })
}

#[tokio::test]
async fn cpp_info_has_no_tx_versions() {
    let c = client();
    fake(&c).answer("/info", 200, &cpp_info());
    let info = c.info().await.expect("info");
    assert_eq!(
        fake(&c).last(),
        (Method::Get, "/info".to_owned(), Value::Null)
    );
    assert_eq!(info.height, 1_500_001);
    assert_eq!(info.local_block_count(), 1_500_000);
    assert_eq!(info.network_block_count(), 1_500_000);
    assert_eq!(info.peer_count(), 11);
    assert_eq!(info.upgrade_heights.len(), 13);
    assert_eq!(info.version, "1.1.4");
    assert_eq!(info.tx_versions, None);
    assert_eq!(info.hyper_connections_count, None);
    assert_eq!(info.fork_readiness, None);
    assert_eq!(info.pruning_seed, None);

    // An old node with only what Nigel reads.
    fake(&c).answer(
        "/info",
        200,
        &json!({
            "status": "OK", "height": 10, "network_height": 12, "difficulty": 300,
            "incoming_connections_count": 0, "outgoing_connections_count": 2,
        }),
    );
    let info = c.info().await.expect("minimal info");
    assert_eq!(
        (info.height, info.network_height, info.peer_count()),
        (10, 12, 2)
    );
    assert!(matches!(info.upgrade_heights.as_slice(), []));

    // Without the fields Nigel needs it is not an answer.
    fake(&c).answer("/info", 200, &json!({ "status": "OK", "height": 10 }));
    assert!(matches!(c.info().await, Err(DaemonError::Parse(_))));
    // xkrd's fork_readiness is null when no fork is pending.
    let mut xkr = cpp_info();
    xkr["tx_versions"] = json!([1, 2]);
    xkr["fork_readiness"] = Value::Null;
    xkr["pruning_seed"] = json!(0);
    fake(&c).answer("/info", 200, &xkr);
    let info = c.info().await.expect("xkrd info");
    assert_eq!(info.tx_versions, Some(vec![1, 2]));
    assert_eq!(info.fork_readiness, None);
    assert_eq!(info.pruning_seed, Some(0));
}

#[tokio::test]
async fn node_fee_follows_nigel() {
    let fixture = walletsync_fixture();
    let text = fixture["subwallets"][0]["address"]
        .as_str()
        .expect("address");
    let address = Address::parse(text).expect("fixture address");
    let c = client();

    // A C++ node without a fee: `on_get_fee_info` leaves address and amount empty.
    fake(&c).answer(
        "/fee",
        200,
        &json!({ "address": "", "amount": 0, "status": "OK" }),
    );
    let fee = c.fee().await.expect("fee");
    assert_eq!(fake(&c).last().0, Method::Get);
    assert_eq!(fake(&c).last().1, "/fee");
    assert_eq!(fee.payable(), None);

    fake(&c).answer(
        "/fee",
        200,
        &json!({ "address": text, "amount": 5000, "status": "OK" }),
    );
    let fee = c.fee().await.expect("fee");
    assert_eq!(fee.payable(), Some((address, 5000)));

    // Zero amount, or an integrated address: no fee is paid.
    fake(&c).answer(
        "/fee",
        200,
        &json!({ "address": text, "amount": 0, "status": "OK" }),
    );
    assert_eq!(c.fee().await.expect("fee").payable(), None);
    let integrated = address.integrated(Hash([7; 32])).to_string();
    fake(&c).answer(
        "/fee",
        200,
        &json!({ "address": integrated, "amount": 5000, "status": "OK" }),
    );
    assert_eq!(c.fee().await.expect("fee").payable(), None);
}

#[tokio::test]
async fn wallet_sync_requests_and_cpp_answers() {
    let fixture = walletsync_fixture();
    // Blocks 1..=4 of vector E are before fork 14: exactly what a C++ node sends.
    let items: Vec<Value> = fixture["getwalletsyncdata"]["response"]["items"]
        .as_array()
        .expect("items")[..4]
        .to_vec();
    assert!(items.iter().all(|i| !i.to_string().contains("view_tag")));
    let c = client();
    fake(&c).answer(
        "/getwalletsyncdata",
        200,
        &json!({ "status": "OK", "items": items }),
    );
    let checkpoint = Hash([3; 32]);
    let request = WalletSyncRequest {
        checkpoints: vec![checkpoint],
        start_height: 0,
        start_timestamp: 1_600_000_000,
        block_count: None,
    };
    let blocks = c.wallet_sync_data(&request).await.expect("sync");
    let (method, path, body) = fake(&c).last();
    assert_eq!(
        (method, path.as_str()),
        (Method::Post, "/getwalletsyncdata")
    );
    // Nigel's request: no blockCount.
    assert_eq!(
        body,
        json!({
            "blockHashCheckpoints": [checkpoint.to_hex()],
            "startHeight": 0,
            "startTimestamp": 1_600_000_000u64,
        })
    );
    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks[0].height, 1);
    assert!(
        blocks
            .iter()
            .flat_map(|b| &b.coinbase.outputs)
            .all(|o| o.view_tag.is_none())
    );

    let request = WalletSyncRequest {
        block_count: Some(50),
        ..request
    };
    c.wallet_sync_data(&request).await.expect("sync");
    assert_eq!(fake(&c).last().2["blockCount"], 50);

    fake(&c).answer(
        "/getwalletsyncdata",
        200,
        &json!({ "status": "Failed to perform query", "items": [] }),
    );
    let e = c.wallet_sync_data(&request).await.unwrap_err();
    assert!(matches!(e, DaemonError::Status(ref s) if s == "Failed to perform query"));
}

#[tokio::test]
async fn indexes_outs_status_and_transactions_in_cpp_shape() {
    let c = client();
    let (a, b) = (Hash([1; 32]), Hash([2; 32]));
    fake(&c).answer(
        "/get_global_indexes_for_range",
        200,
        &json!({
            "indexes": [{ "key": a.to_hex(), "value": [4, 5] }, { "key": b.to_hex(), "value": [] }],
            "status": "OK",
        }),
    );
    let indexes = c.global_indexes_for_range(10, 20).await.expect("indexes");
    assert_eq!(
        fake(&c).last().2,
        json!({ "startHeight": 10, "endHeight": 20 })
    );
    assert_eq!(indexes[&a], vec![4, 5]);
    assert!(matches!(indexes[&b].as_slice(), []));

    let key = "ab".repeat(32);
    fake(&c).answer(
        "/getrandom_outs",
        200,
        &json!({
            "outs": [{ "amount": 100, "outs": [
                { "global_amount_index": 7, "out_key": key },
                { "global_amount_index": 9, "out_key": key },
            ] }],
            "status": "OK",
        }),
    );
    let outs = c.random_outs(&[100], 4).await.expect("outs");
    assert_eq!(
        fake(&c).last().2,
        json!({ "amounts": [100], "outs_count": 4 })
    );
    assert_eq!(outs[0].amount, 100);
    let indexes: Vec<u64> = outs[0].outs.iter().map(|o| o.global_amount_index).collect();
    assert_eq!(indexes, vec![7, 9]);
    assert_eq!(outs[0].outs[0].out_key.to_hex(), key);

    fake(&c).answer(
        "/get_transactions_status",
        200,
        &json!({
            "status": "OK",
            "transactionsInPool": [a.to_hex()],
            "transactionsInBlock": [],
            "transactionsUnknown": [b.to_hex()],
        }),
    );
    let status = c.transactions_status(&[a, b]).await.expect("status");
    assert_eq!(
        fake(&c).last().2,
        json!({ "transactionHashes": [a.to_hex(), b.to_hex()] })
    );
    assert!(status.in_pool.contains(&a) && status.unknown.contains(&b));
    assert!(status.in_block.is_empty());

    fake(&c).answer(
        "/gettransactions",
        200,
        &json!({ "txs_as_hex": ["0102ff"], "missed_tx": [b.to_hex()], "status": "OK" }),
    );
    let txs = c.transactions(&[a, b]).await.expect("transactions");
    assert_eq!(
        fake(&c).last().2,
        json!({ "txs_hashes": [a.to_hex(), b.to_hex()] })
    );
    assert_eq!(txs.blobs, vec![vec![1, 2, 0xff]]);
    assert_eq!(txs.missed, vec![b]);

    fake(&c).answer(
        "/gettransactions",
        200,
        &json!({
            "txs_as_hex": [],
            "missed_tx": [],
            "status": "Failed to parse hex representation of transaction hash",
        }),
    );
    assert!(matches!(
        c.transactions(&[a]).await,
        Err(DaemonError::Status(_))
    ));
    fake(&c).answer(
        "/gettransactions",
        200,
        &json!({ "txs_as_hex": ["zz"], "missed_tx": [], "status": "OK" }),
    );
    assert!(matches!(
        c.transactions(&[a]).await,
        Err(DaemonError::Parse(_))
    ));
}

#[tokio::test]
async fn sending_with_cpp_and_xkrd_answers() {
    let c = client();
    let blob = vec![1u8, 0, 1, 2, 3];
    // C++ `on_send_raw_tx`: only a status; the hash is the blob's `cn_fast_hash`.
    fake(&c).answer("/sendrawtransaction", 200, &json!({ "status": "OK" }));
    let sent = c.send_raw_transaction(&blob).await.expect("answer");
    assert_eq!(
        fake(&c).last().2,
        json!({ "tx_as_hex": hex::encode(&blob) })
    );
    assert_eq!(
        sent,
        SendOutcome::Ok(xkr_crypto::keccak::cn_fast_hash(&blob))
    );

    // xkrd names the hash.
    let hash = Hash([9; 32]);
    fake(&c).answer(
        "/sendrawtransaction",
        200,
        &json!({ "status": "OK", "transactionHash": hash.to_hex() }),
    );
    assert_eq!(
        c.send_raw_transaction(&blob).await.expect("answer"),
        SendOutcome::Ok(hash)
    );

    // Refusals are outcomes, not errors.
    fake(&c).answer("/sendrawtransaction", 200, &json!({ "status": "Failed" }));
    assert_eq!(
        c.send_raw_transaction(&blob).await.expect("answer"),
        SendOutcome::Failed("Failed".to_owned())
    );
    fake(&c).answer(
        "/sendrawtransaction",
        200,
        &json!({ "status": "Failed", "error": "transaction version not allowed" }),
    );
    assert_eq!(
        c.send_raw_transaction(&blob).await.expect("answer"),
        SendOutcome::Failed("transaction version not allowed".to_owned())
    );

    // No answer at all is an error (Nigel's connectionError).
    let e = client().send_raw_transaction(&blob).await.unwrap_err();
    assert!(e.is_unreachable());
    assert!(matches!(
        e,
        DaemonError::Transport(TransportError::Connection(_))
    ));
}

#[tokio::test]
async fn json_rpc_in_cpp_shape() {
    let c = client();
    let header = json!({
        "major_version": 6, "minor_version": 0, "timestamp": 1_700_000_000u64,
        "prev_hash": "11".repeat(32), "nonce": 42, "orphan_status": false, "height": 1_500_000,
        "depth": 0, "hash": "22".repeat(32), "difficulty": 2_400_000, "reward": 4_768_371,
        "num_txes": 3, "block_size": 1_234,
    });
    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({ "jsonrpc": "2.0", "id": 0, "result": { "block_header": header, "status": "OK" } }),
    );
    let h = c.last_block_header().await.expect("header");
    let (method, path, body) = fake(&c).last();
    assert_eq!((method, path.as_str()), (Method::Post, "/json_rpc"));
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "getlastblockheader");
    assert_eq!(keys(&body), ["id", "jsonrpc", "method", "params"]);
    assert_eq!((h.height, h.nonce, h.num_txes), (1_500_000, 42, 3));
    assert_eq!(h.hash, Hash([0x22; 32]));

    c.block_header_by_height(1_500_000)
        .await
        .expect("by height");
    let body = fake(&c).last().2;
    assert_eq!(body["method"], "getblockheaderbyheight");
    assert_eq!(body["params"], json!({ "height": 1_500_000 }));

    // getblocktemplate / submitblock as the C++ miner uses them.
    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({ "jsonrpc": "2.0", "id": 0, "result": {
            "difficulty": 2_400_000, "height": 1_500_001, "reserved_offset": 130,
            "blocktemplate_blob": "0607aabb", "status": "OK",
        } }),
    );
    let fixture = walletsync_fixture();
    let miner = Address::parse(
        fixture["subwallets"][0]["address"]
            .as_str()
            .expect("address"),
    )
    .expect("address");
    let t = c.block_template(&miner, 8).await.expect("template");
    let body = fake(&c).last().2;
    assert_eq!(body["method"], "getblocktemplate");
    assert_eq!(
        body["params"],
        json!({ "reserve_size": 8, "wallet_address": miner.to_string() })
    );
    assert_eq!(t.blob, vec![6, 7, 0xaa, 0xbb]);
    assert_eq!((t.height, t.reserved_offset), (1_500_001, 130));

    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({ "jsonrpc": "2.0", "id": 0, "result": { "status": "OK" } }),
    );
    c.submit_block(&[0xde, 0xad]).await.expect("accepted");
    let body = fake(&c).last().2;
    assert_eq!(body["method"], "submitblock");
    assert_eq!(body["params"], json!(["dead"]));

    // Error objects.
    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": -7, "message": "Block not accepted" },
        }),
    );
    let e = c.submit_block(&[0xde, 0xad]).await.unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::BLOCK_NOT_ACCEPTED));
    assert!(matches!(e, DaemonError::Rpc { ref message, .. } if message == "Block not accepted"));
    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({ "jsonrpc": "2.0", "id": 0, "error": { "code": -9, "message": "Core is busy" } }),
    );
    let e = c.block_count().await.unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::CORE_BUSY));
    // A result whose status is not OK, and an answer with neither result nor error.
    fake(&c).answer(
        "/json_rpc",
        200,
        &json!({ "jsonrpc": "2.0", "id": 0, "result": { "count": 1, "status": "Busy" } }),
    );
    assert!(matches!(c.block_count().await, Err(DaemonError::Status(_))));
    fake(&c).answer("/json_rpc", 200, &json!({ "jsonrpc": "2.0", "id": 0 }));
    assert!(matches!(c.block_count().await, Err(DaemonError::Parse(_))));
}

#[tokio::test]
async fn http_failures_map_to_errors() {
    let c = client();
    // C++ `processRequest` while the core is not ready.
    fake(&c).answer_raw("/info", 500, b"Core is busy");
    let e = c.info().await.unwrap_err();
    let busy =
        matches!(&e, DaemonError::Http { status: 500, message } if message == "Core is busy");
    assert!(busy, "{e}");
    assert!(!e.is_unreachable());
    // xkrd's JSON error bodies give their status text.
    fake(&c).answer(
        "/fee",
        400,
        &json!({ "status": "Failed to parse request: expected value" }),
    );
    let e = c.fee().await.unwrap_err();
    let parse_failure = matches!(
        &e,
        DaemonError::Http { status: 400, message } if message.starts_with("Failed to parse")
    );
    assert!(parse_failure, "{e}");
    // A 200 that is not JSON (a C++ handler that could not read the request answers nothing).
    fake(&c).answer_raw("/get_transactions_status", 200, b"");
    let e = c.transactions_status(&[]).await.unwrap_err();
    assert!(matches!(e, DaemonError::Parse(_)));
    // Unreachable.
    let e = c.random_outs(&[1], 1).await.unwrap_err();
    assert!(e.is_unreachable(), "{e}");
}

#[tokio::test]
async fn explorer_requests_are_cpp_shaped() {
    let c = client();
    fake(&c).answer(
        "/get_block_details_by_height",
        200,
        &json!({ "status": "OK", "block": { "index": 5 } }),
    );
    let block = c.get_block_details_by_height(5).await.expect("block");
    assert_eq!(fake(&c).last().2, json!({ "blockHeight": 5 }));
    assert_eq!(block["index"], 5);

    let pid = Hash([4; 32]);
    let tx = Hash([5; 32]);
    fake(&c).answer(
        "/get_transaction_hashes_by_payment_id",
        200,
        &json!({ "status": "OK", "transactionHashes": [tx.to_hex()] }),
    );
    let hashes = c
        .get_transaction_hashes_by_payment_id(&pid)
        .await
        .expect("hashes");
    assert_eq!(fake(&c).last().2, json!({ "paymentId": pid.to_hex() }));
    assert_eq!(hashes, vec![tx]);

    fake(&c).answer(
        "/get_blocks_details_by_heights",
        200,
        &json!({ "status": "Error: too many blocks", "blocks": [] }),
    );
    let e = c.get_blocks_details_by_heights(&[1, 2]).await.unwrap_err();
    assert!(matches!(e, DaemonError::Status(ref s) if s.starts_with("Error:")));
    assert_eq!(fake(&c).last().2, json!({ "blockHeights": [1, 2] }));
}
