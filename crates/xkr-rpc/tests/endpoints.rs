//! The C++ routes added after the wallet subset (block and transaction details, `/get_pool`,
//! `/get_pool_changes`, `/get_transaction_hashes_by_payment_id`, JSON-RPC `get_miner_data` and
//! `get_block_headers_range`) and the `/status` `hugin`, `hyper` and `data_dir` fields, on a chain that holds only the
//! genesis block. Requests go straight into the router, no socket.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::StatusCode;
use common::{TempDb, json_body, post, state, state_with_hugin};
use serde_json::{Value, json};
use xkr_rpc::router;

const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Hash of the genesis miner transaction.
fn genesis_coinbase_hash() -> String {
    let blob = hex::decode(xkr_consensus::params::GENESIS_COINBASE_TX_HEX).expect("hex");
    xkr_serialization::transaction_hash_from_blob(&blob).to_string()
}

/// `app` as a client from `addr` reaches it (the remote address `xkr_rpc::serve` passes on).
fn from_addr(app: Router, addr: &str) -> Router {
    app.layer(MockConnectInfo(
        addr.parse::<SocketAddr>().expect("socket address"),
    ))
}

async fn post_json(app: &Router, path: &str, body: &Value) -> (StatusCode, Value) {
    let (status, bytes) = post(app, path, json_body(body)).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn rpc(app: &Router, method: &str, params: Option<Value>) -> Value {
    let mut req = json!({ "jsonrpc": "2.0", "id": 7, "method": method });
    if let Some(p) = params {
        req["params"] = p;
    }
    let (status, v) = post_json(app, "/json_rpc", &req).await;
    assert_eq!(status, StatusCode::OK);
    v
}

/// The `f_*` methods answer only on a node started with `--explorer`
/// ([`xkr_rpc::RpcState::explorer`]); off, they are reported like an unknown method.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explorer_methods_need_the_flag() {
    let (db_on, db_off) = (TempDb::new(), TempDb::new());
    let on = state(&db_on);
    let genesis = on.chain.genesis_hash().to_string();
    let mut off = state(&db_off);
    Arc::get_mut(&mut off).expect("unshared").explorer = false;
    let on = router(on, &[]);
    let off = router(off, &[]);
    for (method, params) in [
        ("f_blocks_list_json", json!({ "height": 0 })),
        ("f_block_json", json!({ "hash": genesis })),
        (
            "f_transaction_json",
            json!({ "hash": genesis_coinbase_hash() }),
        ),
        ("f_on_transactions_pool_json", json!({})),
    ] {
        let v = rpc(&on, method, Some(params.clone())).await;
        assert_eq!(v["result"]["status"], "OK", "{method} on: {v}");
        let v = rpc(&off, method, Some(params)).await;
        assert_eq!(v["error"]["code"], -32601, "{method} off: {v}");
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("--explorer")),
            "{method} off: {v}"
        );
        assert!(v.get("result").is_none(), "{method} off: {v}");
    }
    // other methods are unaffected
    let v = rpc(&off, "getblockcount", None).await;
    assert_eq!(v["result"]["status"], "OK", "{v}");
}

/// `/get_block_details_by_height` with the C++ `BlockDetails` / `TransactionDetails` keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genesis_block_details() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);
    let (status, v) = post_json(
        &app,
        "/get_block_details_by_height",
        &json!({ "blockHeight": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    let b = &v["block"];
    assert_eq!(b["index"], 0);
    assert_eq!(b["hash"], genesis);
    assert_eq!(b["prevBlockHash"], ZERO_HASH);
    let header = rpc(&app, "getblockheaderbyheight", Some(json!({ "height": 0 }))).await;
    let header = &header["result"]["block_header"];
    assert_eq!(b["majorVersion"], header["major_version"]);
    assert_eq!(b["minorVersion"], header["minor_version"]);
    assert_eq!(b["nonce"], header["nonce"]);
    assert_eq!(b["difficulty"], header["difficulty"]);
    assert_eq!(b["reward"], header["reward"]);
    assert_eq!(b["blockSize"], header["block_size"]);
    assert_eq!(b["sizeMedian"], 0);
    assert_eq!(b["totalFeeAmount"], 0);
    assert!(b["reward"].as_u64().is_some_and(|r| r > 0), "{b}");
    assert!(b.get("penalty").is_none() && b.get("isAlternative").is_none());
    assert_eq!(b["transactions"].as_array().map(Vec::len), Some(1));

    let tx = &b["transactions"][0];
    assert_eq!(tx["hash"], genesis_coinbase_hash());
    assert_eq!(tx["inBlockchain"], true);
    assert_eq!(tx["blockHash"], genesis);
    assert_eq!(tx["blockIndex"], 0);
    assert_eq!(tx["timestamp"], b["timestamp"]);
    assert_eq!(tx["fee"], 0);
    assert_eq!(tx["totalInputsAmount"], 0);
    assert_eq!(tx["totalOutputsAmount"], b["reward"]);
    assert_eq!(tx["mixin"], 0);
    assert_eq!(tx["paymentId"], ZERO_HASH);
    assert_eq!(tx["signaturesSize"], 0);
    assert_eq!(tx["signatures"], json!([]));
    assert!(tx.get("clsag_signatures").is_none());
    assert_eq!(tx["extra"]["publicKey"].as_str().map(str::len), Some(64));
    assert_ne!(tx["extra"]["publicKey"], ZERO_HASH);
    assert!(tx["extra"]["nonce"].is_array());
    assert!(
        tx["extra"]["raw"]
            .as_str()
            .is_some_and(|r| r.starts_with("01"))
    );
    let input = &tx["inputs"][0];
    assert_eq!(input["type"], "ff");
    assert_eq!(input["data"]["input"]["height"], 0);
    assert_eq!(input["data"]["amount"], b["reward"]);
    let output = &tx["outputs"][0];
    assert_eq!(output["globalIndex"], 0);
    assert_eq!(output["output"]["target"]["type"], "02");
    assert_eq!(
        output["output"]["target"]["data"]["key"]
            .as_str()
            .map(str::len),
        Some(64)
    );

    // Above the top: the C++ error text, HTTP 200.
    let (status, v) = post_json(
        &app,
        "/get_block_details_by_height",
        &json!({ "blockHeight": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        v["status"],
        "Error: Requested block height wasn't found in blockchain."
    );
    assert_eq!(v["block"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_details_lists() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);

    let (_, v) = post_json(
        &app,
        "/get_blocks_details_by_heights",
        &json!({ "blockHeights": [0, 0] }),
    )
    .await;
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["blocks"].as_array().map(Vec::len), Some(2));
    assert_eq!(v["blocks"][1]["hash"], genesis);

    let (_, v) = post_json(
        &app,
        "/get_blocks_details_by_heights",
        &json!({ "blockHeights": [0, 5] }),
    )
    .await;
    assert_eq!(
        v["status"],
        "Error: Requested block height wasn't found in blockchain."
    );
    assert_eq!(v["blocks"], json!([]));

    // Capped: 101 blocks are refused before any is read.
    let (_, v) = post_json(
        &app,
        "/get_blocks_details_by_heights",
        &json!({ "blockHeights": vec![0u32; 101] }),
    )
    .await;
    assert!(
        v["status"]
            .as_str()
            .is_some_and(|s| s.starts_with("Error: too many")),
        "{v}"
    );
    assert_eq!(v["blocks"], json!([]));

    let (_, v) = post_json(
        &app,
        "/get_blocks_details_by_hashes",
        &json!({ "blockHashes": [genesis] }),
    )
    .await;
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["blocks"][0]["index"], 0);

    let (_, v) = post_json(
        &app,
        "/get_blocks_details_by_hashes",
        &json!({ "blockHashes": [ZERO_HASH] }),
    )
    .await;
    assert_eq!(
        v["status"],
        "Error: Requested hash wasn't found in blockchain."
    );

    let (status, _) = post_json(
        &app,
        "/get_blocks_details_by_hashes",
        &json!({ "blockHashes": ["zz"] }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_details_and_payment_ids() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);
    let coinbase = genesis_coinbase_hash();

    let (_, v) = post_json(
        &app,
        "/get_transaction_details_by_hashes",
        &json!({ "transactionHashes": [coinbase] }),
    )
    .await;
    assert_eq!(v["status"], "OK", "{v}");
    let tx = &v["transactions"][0];
    assert_eq!(tx["hash"], coinbase);
    assert_eq!(tx["blockHash"], genesis);
    assert_eq!(tx["inputs"][0]["type"], "ff");

    let (_, v) = post_json(
        &app,
        "/get_transaction_details_by_hashes",
        &json!({ "transactionHashes": [ZERO_HASH] }),
    )
    .await;
    assert_eq!(v["status"], "Error: Requested transaction wasn't found.");
    assert_eq!(v["transactions"], json!([]));

    let many = vec![ZERO_HASH; 1_001];
    let (_, v) = post_json(
        &app,
        "/get_transaction_details_by_hashes",
        &json!({ "transactionHashes": many }),
    )
    .await;
    assert!(
        v["status"]
            .as_str()
            .is_some_and(|s| s.starts_with("Error: too many")),
        "{v}"
    );

    let (status, v) = post_json(
        &app,
        "/get_transaction_hashes_by_payment_id",
        &json!({ "paymentId": "11".repeat(32) }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["transactionHashes"], json!([]));
    let (status, _) = post_json(
        &app,
        "/get_transaction_hashes_by_payment_id",
        &json!({ "paymentId": "nope" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_listings() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);

    let (status, v) = post_json(&app, "/get_pool", &json!({ "timestampBegin": 0 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!({ "addedTxs": [], "status": "OK" }));

    let known = "ab".repeat(32);
    let (status, v) = post_json(
        &app,
        "/get_pool_changes",
        &json!({ "tailBlockId": genesis, "knownTxsIds": [known] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["isTailBlockActual"], true);
    assert_eq!(v["addedTxs"], json!([]));
    assert_eq!(v["deletedTxsIds"], json!([known]));

    let (_, v) = post_json(&app, "/get_pool_changes", &json!({})).await;
    assert_eq!(v["isTailBlockActual"], false);

    let too_many = vec![String::new(); 100_001];
    let (status, _) = post_json(
        &app,
        "/get_pool_changes",
        &json!({ "knownTxsIds": too_many }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// `get_miner_data` as p2pool sends it (no `params`), in the C++ shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miner_data() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);
    let v = rpc(&app, "get_miner_data", None).await;
    let r = &v["result"];
    assert_eq!(r["status"], "OK", "{v}");
    assert_eq!(r["height"], 1);
    assert_eq!(r["prev_id"], genesis);
    assert!(r["major_version"].as_u64().is_some_and(|m| m >= 1), "{r}");
    assert_eq!(r["seed_hash"], ZERO_HASH);
    let difficulty = r["difficulty"].as_str().expect("hex string");
    assert!(!difficulty.starts_with("0x"));
    assert!(
        u64::from_str_radix(difficulty, 16).is_ok_and(|d| d > 0),
        "{r}"
    );
    assert!(r["median_weight"].as_u64().is_some_and(|w| w > 0), "{r}");
    assert!(r["already_generated_coins"].as_u64().is_some_and(|c| c > 0));
    // height 1 is below the 60-block timestamp window
    assert_eq!(r["median_timestamp"], 0);
    assert_eq!(r["unlock_window"], 20);
    assert_eq!(r["tx_backlog"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_headers_range() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);

    let v = rpc(
        &app,
        "get_block_headers_range",
        Some(json!({ "start_height": 0, "end_height": 0 })),
    )
    .await;
    let r = &v["result"];
    assert_eq!(r["status"], "OK", "{v}");
    assert_eq!(r["untrusted"], false);
    assert_eq!(r["headers"].as_array().map(Vec::len), Some(1));
    let h = &r["headers"][0];
    assert_eq!(h["hash"], genesis);
    assert_eq!(h["height"], 0);
    assert_eq!(h["depth"], 0);
    assert_eq!(h["num_txes"], 1);
    assert_eq!(h["orphan_status"], false);
    assert!(h["block_size"].as_u64().is_some_and(|s| s > 0));

    // The same header as getblockheaderbyheight, except the lightweight block_size.
    let full = rpc(&app, "getblockheaderbyheight", Some(json!({ "height": 0 }))).await;
    let full = &full["result"]["block_header"];
    for key in [
        "hash",
        "prev_hash",
        "nonce",
        "timestamp",
        "reward",
        "difficulty",
    ] {
        assert_eq!(h[key], full[key], "{key}");
    }

    for params in [
        json!({ "start_height": 0, "end_height": 1 }),
        json!({ "start_height": 1, "end_height": 0 }),
    ] {
        let v = rpc(&app, "get_block_headers_range", Some(params)).await;
        assert_eq!(v["error"]["code"], -2, "{v}");
        assert_eq!(v["error"]["message"], "Invalid start/end heights.");
    }
}

/// `/status` `hugin`: `null` without `--hugin-relay`, the counters with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_hugin_object() {
    let db = TempDb::new();
    let app = router(state(&db), &[]);
    let (status, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["hugin"], Value::Null, "{v}");
    assert_eq!(v["p2pool"], Value::Null, "{v}");

    let hugin_stats = Arc::new(xkr_hugin::HuginStats::default());
    hugin_stats.clients.store(3, Ordering::Relaxed);
    hugin_stats.nodes.store(2, Ordering::Relaxed);
    hugin_stats.messages.store(40, Ordering::Relaxed);
    hugin_stats.shares_accepted.store(9, Ordering::Relaxed);
    hugin_stats.shares_submitted.store(5, Ordering::Relaxed);
    hugin_stats
        .shares_rejected_by_pool
        .store(3, Ordering::Relaxed);
    hugin_stats.pool_ready.store(true, Ordering::Relaxed);
    hugin_stats.apps_cap.store(200, Ordering::Relaxed);
    hugin_stats.syncs_running.store(2, Ordering::Relaxed);
    hugin_stats.syncs_queued.store(1, Ordering::Relaxed);
    hugin_stats.queued_bytes.store(4096, Ordering::Relaxed);
    hugin_stats.admission_paused.store(true, Ordering::Relaxed);
    hugin_stats.refused_busy.store(7, Ordering::Relaxed);
    hugin_stats.syncs_limited.store(5, Ordering::Relaxed);
    hugin_stats.apps_on_allowance.store(1, Ordering::Relaxed);
    hugin_stats.set_fee(xkr_hugin::FeeInfo {
        address: Some("SEKRfee".into()),
        donation: true,
    });
    hugin_stats.set_p2pool(xkr_hugin::P2poolInfo {
        host: Some("127.0.0.1:3333".into()),
        connected: true,
    });
    let db_hugin = TempDb::new();
    let app = from_addr(
        router(state_with_hugin(&db_hugin, Some(hugin_stats.clone())), &[]),
        "127.0.0.1:40000",
    );
    let (_, v) = post_json(&app, "/status", &json!({})).await;
    let h = &v["hugin"];
    assert_eq!(h["clients"], 3, "{v}");
    assert_eq!(h["nodes"], 2);
    assert_eq!(h["messages"], 40);
    assert_eq!(h["shares_accepted"], 9);
    assert_eq!(h["shares_submitted"], 5);
    assert_eq!(h["shares_rejected_by_pool"], 3);
    assert_eq!(h["pool_ready"], true);
    assert_eq!(h["apps_cap"], 200);
    assert_eq!(h["syncs_running"], 2);
    assert_eq!(h["syncs_queued"], 1);
    assert_eq!(h["queued_bytes"], 4096);
    assert_eq!(h["admission_paused"], true);
    assert_eq!(h["refused_busy"], 7);
    assert_eq!(h["syncs_limited"], 5);
    assert_eq!(h["apps_on_allowance"], 1);
    assert_eq!(h["fee_address"], "SEKRfee");
    assert_eq!(h["fee_donation"], true);
    assert_eq!(h["p2pool"]["host"], "127.0.0.1:3333");
    assert_eq!(h["p2pool"]["connected"], true);
    // live: a later change shows up in the next answer
    hugin_stats.clients.store(4, Ordering::Relaxed);
    let (_, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(v["hugin"]["clients"], 4);
}

/// `/status` `hyper`: `null` without the Hyperswarm network, else its DHT node with the UDP port
/// and whether the node is firewalled; changes show up in the next answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_hyper_object() {
    let db = TempDb::new();
    let app = router(state(&db), &[]);
    let (status, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["hyper"], Value::Null, "{v}");

    let db_hyper = TempDb::new();
    let Ok(mut inner) = Arc::try_unwrap(state(&db_hyper)) else {
        panic!("one owner");
    };
    let (net_tx, net_rx) = tokio::sync::watch::channel(Arc::new(xkr_rpc::NetStatus {
        hyper_net: Some(xkr_rpc::HyperNetStatus::default()),
        ..xkr_rpc::NetStatus::default()
    }));
    inner.net = net_rx;
    let app = from_addr(router(Arc::new(inner), &[]), "127.0.0.1:40000");
    let (_, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(
        v["hyper"],
        json!({
            "state": "starting",
            "udp_port": null,
            "port_fixed": false,
            "firewalled": null,
            "nat": "unknown",
            "private": false,
        }),
        "{v}"
    );

    net_tx.send_replace(Arc::new(xkr_rpc::NetStatus {
        hyper_net: Some(xkr_rpc::HyperNetStatus {
            state: xkr_rpc::HyperDhtState::Ready,
            udp_port: Some(21_890),
            port_fixed: true,
            firewalled: Some(false),
            nat: xkr_rpc::HyperNat::Consistent,
            private: true,
        }),
        ..xkr_rpc::NetStatus::default()
    }));
    let (_, v) = post_json(&app, "/status", &json!({})).await;
    let h = &v["hyper"];
    assert_eq!(h["state"], "ready", "{v}");
    assert_eq!(h["udp_port"], 21_890);
    assert_eq!(h["port_fixed"], true);
    assert_eq!(h["firewalled"], false);
    assert_eq!(h["nat"], "consistent");
    assert_eq!(h["private"], true);
}

/// `/status` `hugin.fee_address`, `hugin.p2pool.host`, `hyper.udp_port`, `hyper.firewalled`,
/// `hyper.nat`, `p2pool.payout_address` and `p2pool.stratum` only for a client on a loopback
/// address (IPv4, IPv6 or IPv4-mapped); other clients, and requests without a known remote
/// address, get the rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_fields_for_loopback() {
    let hugin_stats = Arc::new(xkr_hugin::HuginStats::default());
    hugin_stats.clients.store(2, Ordering::Relaxed);
    hugin_stats.set_fee(xkr_hugin::FeeInfo {
        address: Some("SEKRfee".into()),
        donation: false,
    });
    hugin_stats.set_p2pool(xkr_hugin::P2poolInfo {
        host: Some("127.0.0.1:3333".into()),
        connected: true,
    });
    let db = TempDb::new();
    let Ok(mut inner) = Arc::try_unwrap(state_with_hugin(&db, Some(hugin_stats))) else {
        panic!("one owner");
    };
    let (_net_tx, net_rx) = tokio::sync::watch::channel(Arc::new(xkr_rpc::NetStatus {
        hyper_net: Some(xkr_rpc::HyperNetStatus {
            state: xkr_rpc::HyperDhtState::Ready,
            udp_port: Some(21_890),
            port_fixed: true,
            firewalled: Some(false),
            nat: xkr_rpc::HyperNat::Consistent,
            private: true,
        }),
        ..xkr_rpc::NetStatus::default()
    }));
    inner.net = net_rx;
    let (_p2pool_tx, p2pool_rx) = tokio::sync::watch::channel(Arc::new(xkr_rpc::P2poolStatus {
        state: "running".into(),
        sidechain: "default".into(),
        height: 29_277,
        peers_hyper: 2,
        payout_address: Some("SEKRpayout".into()),
        payout_donation: true,
        stratum: Some("127.0.0.1:3333".into()),
        stratum_off: Some("127.0.0.1:3334 in use; set --p2pool-stratum".into()),
        ..xkr_rpc::P2poolStatus::default()
    }));
    inner.p2pool = Some(p2pool_rx);
    let state = Arc::new(inner);

    for addr in ["127.0.0.1:40000", "[::1]:40000", "[::ffff:127.0.0.1]:40000"] {
        let app = from_addr(router(state.clone(), &[]), addr);
        let (_, v) = post_json(&app, "/status", &json!({})).await;
        assert_eq!(v["p2pool"]["payout_address"], "SEKRpayout", "{addr}: {v}");
        assert_eq!(v["p2pool"]["stratum"], "127.0.0.1:3333", "{addr}");
        assert_eq!(
            v["p2pool"]["stratum_off"], "127.0.0.1:3334 in use; set --p2pool-stratum",
            "{addr}"
        );
        assert_eq!(v["p2pool"]["height"], 29_277, "{addr}");
        assert_eq!(v["hugin"]["fee_address"], "SEKRfee", "{addr}: {v}");
        assert_eq!(v["hugin"]["p2pool"]["host"], "127.0.0.1:3333", "{addr}");
        assert_eq!(v["hyper"]["udp_port"], 21_890, "{addr}");
        assert_eq!(v["hyper"]["firewalled"], false, "{addr}");
        assert_eq!(v["hyper"]["nat"], "consistent", "{addr}");
    }

    let public = from_addr(router(state.clone(), &[]), "203.0.113.9:40000");
    let unknown = router(state, &[]);
    for app in [public, unknown] {
        let (status, v) = post_json(&app, "/status", &json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let p = &v["p2pool"];
        assert!(p.get("payout_address").is_none(), "{v}");
        assert!(p.get("stratum").is_none(), "{v}");
        assert!(p.get("stratum_off").is_none(), "{v}");
        assert_eq!(p["payout_donation"], true);
        assert_eq!(p["peers_hyper"], 2);
        assert_eq!(p["state"], "running");
        let h = &v["hugin"];
        assert!(h.get("fee_address").is_none(), "{v}");
        assert!(h["p2pool"].get("host").is_none(), "{v}");
        assert_eq!(h["p2pool"]["connected"], true);
        assert_eq!(h["fee_donation"], false);
        assert_eq!(h["clients"], 2);
        assert_eq!(
            v["hyper"],
            json!({ "state": "ready", "port_fixed": true, "private": true }),
            "{v}"
        );
    }
}

/// `/status` `data_dir`: the node's data directory, `null` when the node leaves it empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_data_dir() {
    let db = TempDb::new();
    let app = router(state(&db), &[]);
    let (_, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(
        v["data_dir"],
        std::env::temp_dir().display().to_string(),
        "{v}"
    );

    let db_none = TempDb::new();
    let Ok(mut inner) = Arc::try_unwrap(state(&db_none)) else {
        panic!("one owner");
    };
    inner.info.data_dir = std::path::PathBuf::new();
    let app = router(Arc::new(inner), &[]);
    let (status, v) = post_json(&app, "/status", &json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["data_dir"], Value::Null, "{v}");
}

/// The new routes read their body like the others: over 2 MB is `413`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_routes_have_the_body_limit() {
    let db = TempDb::new();
    let app = router(state(&db), &[]);
    for path in [
        "/get_pool",
        "/get_pool_changes",
        "/get_block_details_by_height",
        "/get_blocks_details_by_heights",
        "/get_blocks_details_by_hashes",
        "/get_transaction_details_by_hashes",
        "/get_transaction_hashes_by_payment_id",
    ] {
        let (status, _) = post(&app, path, vec![b' '; 2 * 1024 * 1024 + 1]).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    }
}
