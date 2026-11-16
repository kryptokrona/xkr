//! walletd over its router with a wallet on an in-process xkrd: authentication in both modes,
//! the JSON-RPC envelope and transport, parameter checks, block and transaction queries,
//! sending, delayed transactions, export and tracking containers.

#[path = "../../xkr-wallet-core/tests/vectors/mod.rs"]
mod vectors;

#[allow(dead_code)]
#[path = "../../xkr-rpc/tests/tx_v2_walletsync/node.rs"]
mod node;

mod common;

use std::sync::Arc;

use axum::Router;
use common::{Chain, RouterTransport, http, rpc, wait_synced, wallet_config};
use serde_json::{Value, json};
use vectors::secret;
use xkr_consensus::Network;
use xkr_types::{Hash, PublicKey};
use xkr_wallet::{Node, Wallet};
use xkr_wallet_core::{WalletKeys, mnemonic_from_secret};
use xkr_walletd::{RpcSettings, Service, router};

const PASSWORD: &str = "fixture-rpc-pass";
const CORS: &str = "https://fixture.example";

struct Setup {
    chain: Chain,
    wallet: Arc<Wallet<RouterTransport>>,
    /// Password checked.
    router: Router,
    /// `--rpc-legacy-security --enable-cors https://fixture.example`.
    legacy: Router,
    me: WalletKeys,
    other: WalletKeys,
    dir: tempfile::TempDir,
}

fn keys(label: &str) -> WalletKeys {
    WalletKeys::from_spend_secret(&secret(label)).expect("reduced secret")
}

/// A chain with `blocks` blocks mined to a deterministic wallet, which walletd serves.
async fn setup(blocks: u64) -> Setup {
    let mut chain = Chain::new();
    let me = keys("walletd handlers wallet");
    let other = keys("walletd handlers other");
    chain.mine_many(&me.address(), blocks);
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::import_keys(
        dir.path().join("walletd.wallet"),
        "container-pass",
        me.spend_secret,
        me.view_secret,
        0,
        chain.node(),
        wallet_config(),
    )
    .await
    .unwrap();
    wait_synced(&wallet, chain.tip).await;
    let wallet = Arc::new(wallet);
    let service = |settings| {
        router(Arc::new(Service::new(
            Arc::clone(&wallet),
            chain.client.clone(),
            Network::Testnet,
            settings,
        )))
    };
    let router = service(RpcSettings::with_password(PASSWORD, ""));
    let legacy = service(RpcSettings::legacy(CORS));
    Setup {
        chain,
        wallet,
        router,
        legacy,
        me,
        other,
        dir,
    }
}

fn request(method: &str, params: Value) -> Value {
    let mut r = json!({ "jsonrpc": "2.0", "id": 1, "password": PASSWORD, "method": method });
    r["params"] = params;
    r
}

async fn call(router: &Router, method: &str, params: Value) -> Value {
    rpc(router, &request(method, params)).await
}

/// The result of a call that must succeed.
async fn ok(router: &Router, method: &str, params: Value) -> Value {
    let answer = call(router, method, params).await;
    assert!(answer.get("error").is_none(), "{method}: {answer}");
    answer["result"].clone()
}

/// `(code, application_code, message)` of a call that must fail.
async fn fail(router: &Router, method: &str, params: Value) -> (i64, Option<i64>, String) {
    let answer = call(router, method, params).await;
    let error = &answer["error"];
    assert!(error.is_object(), "{method}: {answer}");
    (
        error["code"].as_i64().unwrap(),
        error["data"]["application_code"].as_i64(),
        error["message"].as_str().unwrap().to_owned(),
    )
}

/// The application code of a failed call (`-32700` errors).
async fn app_code(router: &Router, method: &str, params: Value) -> i64 {
    let (code, app, _) = fail(router, method, params).await;
    assert_eq!(code, -32700);
    app.expect("application code")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_envelope_and_transport() {
    let s = setup(2).await;
    let me = s.me.address().to_string();

    // password mode
    let no_password = json!({ "jsonrpc": "2.0", "id": 7, "method": "getAddresses" });
    let invalid = json!({
        "jsonrpc": "2.0", "id": 7,
        "error": { "code": -32604, "message": "Invalid or no rpc password" }
    });
    assert_eq!(rpc(&s.router, &no_password).await, invalid);
    let mut wrong = no_password.clone();
    wrong["password"] = json!("wrong");
    assert_eq!(rpc(&s.router, &wrong).await, invalid);
    wrong["password"] = json!(5);
    assert_eq!(rpc(&s.router, &wrong).await, invalid);
    assert_eq!(
        ok(&s.router, "getAddresses", json!({})).await,
        json!({ "addresses": [me] })
    );
    // legacy mode: the password is not looked at
    assert_eq!(
        rpc(&s.legacy, &no_password).await["result"],
        json!({ "addresses": [me] })
    );
    assert_eq!(
        rpc(&s.legacy, &wrong).await["result"]["addresses"][0],
        json!(me)
    );

    // ids are echoed whatever their type, and left out when not sent
    for id in [json!("abc"), json!(null), json!({ "a": [1] }), json!(1.5)] {
        let mut r = request("getAddresses", json!({}));
        r["id"] = id.clone();
        assert_eq!(rpc(&s.router, &r).await["id"], id);
    }
    let mut r = request("getAddresses", json!({}));
    r.as_object_mut().unwrap().remove("id");
    let answer = rpc(&s.router, &r).await;
    assert!(answer.get("id").is_none());
    assert_eq!(answer["jsonrpc"], json!("2.0"));

    // method and params
    let mut r = request("getAddresses", json!({}));
    r.as_object_mut().unwrap().remove("method");
    assert_eq!(
        rpc(&s.router, &r).await["error"],
        json!({ "code": -3600, "message": "Invalid Request" })
    );
    r["method"] = json!(5);
    assert_eq!(rpc(&s.router, &r).await["error"]["code"], json!(-3600));
    assert_eq!(
        call(&s.router, "noSuchMethod", json!({})).await["error"],
        json!({ "code": -32601, "message": "Method not found" })
    );
    assert_eq!(
        call(&s.router, "getStatus", json!([])).await["error"],
        json!({ "code": -32600, "message": "Invalid Request" })
    );
    assert_eq!(
        call(&s.router, "getStatus", json!(null)).await["error"]["code"],
        json!(-32600)
    );
    let mut r = request("getStatus", json!({}));
    r.as_object_mut().unwrap().remove("params");
    assert!(rpc(&s.router, &r).await["result"].is_object());

    // raw bodies
    let json_headers = [("Content-Type".to_owned(), "application/json".to_owned())];
    let parse_error =
        r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#;
    let bodies: [&[u8]; 3] = [b"{\"jsonrpc\": ", b"", b"garbage"];
    for body in bodies {
        let a = http(&s.legacy, "POST", "/json_rpc", &json_headers, body.to_vec()).await;
        assert_eq!((a.status, a.body.as_slice()), (200, parse_error.as_bytes()));
        assert!(a.headers.get("access-control-allow-origin").is_none());
        assert_eq!(a.headers["content-length"], "75");
    }
    let a = http(&s.legacy, "GET", "/json_rpc", &[], Vec::new()).await;
    assert_eq!(a.body, parse_error.as_bytes());
    let a = http(
        &s.legacy,
        "POST",
        "/json_rpc",
        &json_headers,
        b"[1]".to_vec(),
    )
    .await;
    assert_eq!(
        a.json(),
        json!({ "error": { "code": -32001, "message": "JsonValue type is not OBJECT" } })
    );
    // what follows the JSON value is ignored
    let mut body = serde_json::to_vec(&request("getAddresses", json!({}))).unwrap();
    body.extend_from_slice(b" trailing");
    let a = http(&s.legacy, "POST", "/json_rpc", &json_headers, body).await;
    assert_eq!(a.json()["result"]["addresses"][0], json!(me));
    assert_eq!(a.headers["access-control-allow-origin"], CORS);
    assert_eq!(a.headers["server"], "CryptoNote-based HTTP server");
    assert_eq!(a.headers["connection"], "keep-alive");
    assert!(a.headers.get("content-type").is_none());
    // no CORS header without --enable-cors
    let a = http(
        &s.router,
        "POST",
        "/json_rpc",
        &json_headers,
        serde_json::to_vec(&request("getAddresses", json!({}))).unwrap(),
    )
    .await;
    assert!(a.headers.get("access-control-allow-origin").is_none());
    // other URLs
    for path in ["/other", "/json_rpc/", "/json_rpc?x=1", "/"] {
        let a = http(&s.router, "POST", path, &json_headers, b"{}".to_vec()).await;
        assert_eq!(a.status, 404, "{path}");
        assert_eq!(a.body, b"Requested url is not found\n");
        assert_eq!(a.headers["content-length"], "27");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallet_block_and_transaction_queries() {
    let s = setup(4).await;
    let r = &s.router;
    let me = s.me.address().to_string();
    let other = s.other.address().to_string();
    let hex = |h: &Hash| h.to_hex();

    let status = ok(r, "getStatus", json!({})).await;
    assert_eq!(status["blockCount"], json!(5));
    assert_eq!(status["knownBlockCount"], json!(5));
    assert_eq!(status["localDaemonBlockCount"], json!(5));
    assert_eq!(status["lastBlockHash"], json!(hex(&s.chain.tip)));
    assert!(status["peerCount"].is_u64());

    let all: Vec<String> = s.chain.hashes.iter().map(hex).collect();
    assert_eq!(
        ok(
            r,
            "getBlockHashes",
            json!({ "firstBlockIndex": 0, "blockCount": 10 })
        )
        .await,
        json!({ "blockHashes": all })
    );
    assert_eq!(
        ok(
            r,
            "getBlockHashes",
            json!({ "firstBlockIndex": 2, "blockCount": 2 })
        )
        .await,
        json!({ "blockHashes": [all[2], all[3]] })
    );
    assert_eq!(
        ok(
            r,
            "getBlockHashes",
            json!({ "firstBlockIndex": 9, "blockCount": 1 })
        )
        .await,
        json!({ "blockHashes": [] })
    );
    assert_eq!(
        fail(r, "getBlockHashes", json!({ "blockCount": 1 }))
            .await
            .0,
        -32600
    );

    // every block with a wallet transaction, listed with the hash of the block before it
    let items = ok(
        r,
        "getTransactions",
        json!({ "firstBlockIndex": 1, "blockCount": 100 }),
    )
    .await;
    let items = items["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 4);
    for (i, item) in items.iter().enumerate() {
        let height = i + 1;
        assert_eq!(item["blockHash"], json!(all[height - 1]));
        let txs = item["transactions"].as_array().unwrap();
        assert_eq!(txs.len(), 1);
        let tx = &txs[0];
        assert_eq!(tx["isBase"], json!(true));
        assert_eq!(tx["blockIndex"], json!(height));
        assert_eq!(tx["state"], json!(0));
        assert_eq!(tx["fee"], json!(0));
        assert_eq!(tx["paymentId"], json!(""));
        assert!(tx["extra"].as_str().unwrap().starts_with("01"));
        let transfers = tx["transfers"].as_array().unwrap();
        assert_eq!(transfers.len(), 1, "{tx}");
        assert_eq!(transfers[0]["address"], json!(me));
        assert_eq!(transfers[0]["type"], json!(0));
        assert_eq!(transfers[0]["amount"], tx["amount"]);
    }
    let hashes = ok(
        r,
        "getTransactionHashes",
        json!({ "blockHash": all[2], "blockCount": 2 }),
    )
    .await;
    let hashes = hashes["items"].as_array().unwrap();
    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes[0]["blockHash"], json!(all[1]));
    assert_eq!(hashes[0]["transactionHashes"].as_array().unwrap().len(), 1);
    // blocks whose transactions are all filtered out stay listed, empty
    let filtered = ok(
        r,
        "getTransactions",
        json!({ "firstBlockIndex": 1, "blockCount": 2, "addresses": [other] }),
    )
    .await;
    assert_eq!(
        filtered,
        json!({ "items": [
            { "blockHash": all[0], "transactions": [] },
            { "blockHash": all[1], "transactions": [] },
        ]})
    );
    let pid = "ab".repeat(32);
    let by_pid = ok(
        r,
        "getTransactionHashes",
        json!({ "firstBlockIndex": 1, "blockCount": 1, "paymentId": pid }),
    )
    .await;
    assert_eq!(by_pid["items"][0]["transactionHashes"], json!([]));
    let q = |params: Value| async move { app_code(r, "getTransactions", params).await };
    assert_eq!(
        q(json!({ "firstBlockIndex": 0, "blockCount": 5 })).await,
        22
    );
    assert_eq!(
        q(json!({ "firstBlockIndex": 1, "blockCount": 0 })).await,
        22
    );
    assert_eq!(q(json!({ "firstBlockIndex": 5, "blockCount": 1 })).await, 4);
    assert_eq!(
        q(json!({ "blockHash": "ab".repeat(32), "blockCount": 1 })).await,
        4
    );
    assert_eq!(q(json!({ "blockHash": "zz", "blockCount": 1 })).await, 3);
    assert_eq!(q(json!({ "blockHash": all[0], "blockCount": 1 })).await, 22);
    assert_eq!(
        q(json!({ "firstBlockIndex": 1, "blockCount": 1, "paymentId": "zz" })).await,
        2
    );
    assert_eq!(
        q(json!({ "firstBlockIndex": 1, "blockCount": 1, "addresses": ["x"] })).await,
        7
    );
    for params in [
        json!({ "blockHash": all[1], "firstBlockIndex": 1, "blockCount": 1 }),
        json!({ "blockCount": 1 }),
        json!({ "firstBlockIndex": 1 }),
        json!({ "firstBlockIndex": "1", "blockCount": 1 }),
    ] {
        assert_eq!(fail(r, "getTransactions", params).await.0, -32600);
    }

    let tx_hash = items[0]["transactions"][0]["transactionHash"].clone();
    let one = ok(r, "getTransaction", json!({ "transactionHash": tx_hash })).await;
    assert_eq!(one["transaction"], items[0]["transactions"][0]);
    assert_eq!(
        fail(
            r,
            "getTransaction",
            json!({ "transactionHash": "ab".repeat(32) })
        )
        .await,
        (-32700, Some(23), "Object not found".to_owned())
    );
    assert_eq!(
        fail(r, "getTransaction", json!({ "transactionHash": "abc" })).await,
        (-32700, Some(3), "Wrong block id format".to_owned())
    );
    assert_eq!(
        ok(r, "getUnconfirmedTransactionHashes", json!({})).await,
        json!({ "transactionHashes": [] })
    );

    // keys and addresses
    assert_eq!(
        ok(r, "getViewKey", json!({})).await,
        json!({ "viewSecretKey": s.me.view_secret.to_hex() })
    );
    assert_eq!(
        ok(r, "getSpendKeys", json!({ "address": me })).await,
        json!({
            "spendPublicKey": s.me.address().spend.to_hex(),
            "spendSecretKey": s.me.spend_secret.to_hex(),
        })
    );
    assert_eq!(
        ok(r, "getMnemonicSeed", json!({ "address": me })).await,
        json!({ "mnemonicSeed": mnemonic_from_secret(&s.me.spend_secret) })
    );
    assert_eq!(
        app_code(r, "getSpendKeys", json!({ "address": other })).await,
        23
    );
    assert_eq!(
        app_code(r, "getSpendKeys", json!({ "address": "SEKRnope" })).await,
        7
    );
    assert_eq!(
        fail(r, "getBalance", json!({ "address": other })).await,
        (-32700, Some(24), "Requested wallet not found".to_owned())
    );
    let balance = ok(r, "getBalance", json!({})).await;
    let mine = ok(r, "getBalance", json!({ "address": me })).await;
    assert_eq!(balance, mine);
    assert!(balance["availableBalance"].as_u64().unwrap() > 0);
    assert_eq!(
        ok(r, "validateAddress", json!({ "address": me })).await,
        json!({ "isValid": true })
    );
    assert_eq!(
        ok(r, "validateAddress", json!({ "address": format!(" {me}") })).await,
        json!({ "isValid": false })
    );
    let integrated = ok(
        r,
        "createIntegratedAddress",
        json!({ "address": other, "paymentId": pid }),
    )
    .await;
    let integrated = integrated["integratedAddress"].as_str().unwrap().to_owned();
    let parsed = xkr_wallet_core::Address::parse(&integrated).unwrap();
    assert_eq!(parsed.spend, s.other.address().spend);
    assert_eq!(parsed.payment_id, Some(Hash([0xab; 32])));
    assert_eq!(
        ok(r, "validateAddress", json!({ "address": integrated })).await,
        json!({ "isValid": false })
    );
    assert_eq!(
        app_code(
            r,
            "createIntegratedAddress",
            json!({ "address": me, "paymentId": "zz" })
        )
        .await,
        2
    );
    assert_eq!(
        app_code(
            r,
            "createIntegratedAddress",
            json!({ "address": integrated, "paymentId": pid })
        )
        .await,
        7
    );
    assert_eq!(
        ok(r, "getFeeInfo", json!({})).await,
        json!({ "address": "", "amount": 0 })
    );
    assert_eq!(
        ok(r, "getNodeFeeInfo", json!({})).await,
        json!({ "address": "", "amount": 0 })
    );

    // address management
    let created = ok(r, "createAddress", json!({})).await["address"].clone();
    let sub = keys("walletd handlers sub");
    let sub_address = xkr_wallet_core::Address::new(sub.address().spend, s.me.address().view);
    assert_eq!(
        ok(
            r,
            "createAddress",
            json!({ "spendSecretKey": sub.spend_secret.to_hex() })
        )
        .await,
        json!({ "address": sub_address.to_string() })
    );
    assert_eq!(
        app_code(
            r,
            "createAddress",
            json!({ "spendSecretKey": sub.spend_secret.to_hex() })
        )
        .await,
        20
    );
    assert_eq!(
        app_code(
            r,
            "createAddress",
            json!({ "spendPublicKey": PublicKey([9; 32]).to_hex() })
        )
        .await,
        22
    );
    assert_eq!(
        app_code(r, "createAddress", json!({ "spendSecretKey": "zz" })).await,
        1
    );
    assert_eq!(
        fail(
            r,
            "createAddress",
            json!({ "spendSecretKey": sub.spend_secret.to_hex(), "spendPublicKey": "aa" })
        )
        .await
        .0,
        -32600
    );
    assert_eq!(
        fail(
            r,
            "createAddress",
            json!({ "newAddress": true, "scanHeight": 1 })
        )
        .await
        .0,
        -32600
    );
    let dup = sub.spend_secret.to_hex();
    assert_eq!(
        app_code(
            r,
            "createAddressList",
            json!({ "spendSecretKeys": [dup, dup] })
        )
        .await,
        5
    );
    assert_eq!(
        app_code(
            r,
            "getMnemonicSeed",
            json!({ "address": sub_address.to_string() })
        )
        .await,
        6
    );
    let addresses = ok(r, "getAddresses", json!({})).await;
    assert_eq!(
        addresses,
        json!({ "addresses": [me, created, sub_address.to_string()] })
    );
    assert_eq!(
        ok(r, "deleteAddress", json!({ "address": created })).await,
        json!({})
    );
    assert_eq!(
        app_code(r, "deleteAddress", json!({ "address": other })).await,
        23
    );
    assert_eq!(fail(r, "deleteAddress", json!({})).await.0, -32600);
    wait_synced(&s.wallet, s.chain.tip).await;
    assert_eq!(
        ok(r, "getAddresses", json!({})).await,
        json!({ "addresses": [me, sub_address.to_string()] })
    );

    // createAddressList: every key is checked first, then the list is imported at once
    let view = s.me.address().view;
    let (a, b) = (
        keys("walletd handlers list a"),
        keys("walletd handlers list b"),
    );
    let a_address = xkr_wallet_core::Address::new(a.address().spend, view).to_string();
    let b_address = xkr_wallet_core::Address::new(b.address().spend, view).to_string();
    let (a_hex, b_hex) = (a.spend_secret.to_hex(), b.spend_secret.to_hex());
    assert_eq!(
        app_code(
            r,
            "createAddressList",
            json!({ "spendSecretKeys": [a_hex, sub.spend_secret.to_hex()] })
        )
        .await,
        20
    );
    assert_eq!(
        ok(r, "getAddresses", json!({})).await,
        json!({ "addresses": [me, sub_address.to_string()] })
    );
    assert_eq!(
        ok(
            r,
            "createAddressList",
            json!({ "spendSecretKeys": [a_hex, b_hex], "scanHeight": 0 })
        )
        .await,
        json!({ "addresses": [a_address, b_address] })
    );
    wait_synced(&s.wallet, s.chain.tip).await;

    // deleteAddress takes the first and the last address too
    for address in [&me, &sub_address.to_string(), &a_address, &b_address] {
        assert_eq!(
            ok(r, "deleteAddress", json!({ "address": address })).await,
            json!({})
        );
    }
    assert_eq!(
        ok(r, "getAddresses", json!({})).await,
        json!({ "addresses": [] })
    );
    let status = ok(r, "getStatus", json!({})).await;
    assert_eq!(status["blockCount"], json!(1));
    assert_eq!(
        ok(r, "getBalance", json!({})).await,
        json!({ "availableBalance": 0, "lockedAmount": 0 })
    );
    // without addresses the container takes either kind; a tracking address makes it a
    // tracking container
    let tracked = keys("walletd handlers tracked").address();
    let added = ok(
        r,
        "createAddress",
        json!({ "spendPublicKey": tracked.spend.to_hex(), "scanHeight": 0 }),
    )
    .await;
    let tracked_address = xkr_wallet_core::Address::new(tracked.spend, view).to_string();
    assert_eq!(added, json!({ "address": tracked_address }));
    assert_eq!(
        app_code(
            r,
            "createAddress",
            json!({ "spendSecretKey": sub.spend_secret.to_hex() })
        )
        .await,
        22
    );
    assert_eq!(
        ok(r, "getSpendKeys", json!({ "address": tracked_address })).await["spendSecretKey"],
        json!("00".repeat(32))
    );
    assert!(s.wallet.is_view_wallet());
    wait_synced(&s.wallet, s.chain.tip).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sending_delayed_transactions_export_and_reset() {
    let mut s = setup(12).await;
    let r = s.router.clone();
    let r = &r;
    let me = s.me.address().to_string();
    let other = s.other.address().to_string();
    let send = |amount: u64| json!({ "transfers": [{ "address": other, "amount": amount }], "fee": 10, "anonymity": 0 });

    // parameter and wallet checks, in the C++ order
    let mut v = send(1000);
    v["fee"] = json!(1);
    assert_eq!(
        fail(r, "sendTransaction", v).await,
        (-32700, Some(17), "Transaction fee is too small".to_owned())
    );
    assert_eq!(app_code(r, "sendTransaction", send(0)).await, 11);
    assert_eq!(
        app_code(r, "sendTransaction", send(9_000_000_000_000_000)).await,
        9
    );
    let mut v = send(1000);
    v["transfers"][0]["address"] = json!("SEKRnope");
    assert_eq!(app_code(r, "sendTransaction", v).await, 7);
    let mut v = send(1000);
    v["paymentId"] = json!("ab".repeat(31));
    assert_eq!(app_code(r, "sendTransaction", v).await, 29);
    let mut v = send(1000);
    v["extra"] = json!("zz");
    assert_eq!(app_code(r, "sendTransaction", v.clone()).await, 30);
    assert_eq!(app_code(r, "createDelayedTransaction", v).await, 5);
    let mut v = send(1000);
    v["extra"] = json!("00");
    v["paymentId"] = json!("ab".repeat(32));
    assert_eq!(fail(r, "sendTransaction", v).await.0, -32600);
    let mut v = send(1000);
    v.as_object_mut().unwrap().remove("fee");
    assert_eq!(fail(r, "sendTransaction", v).await.0, -32600);
    let mut v = send(1000);
    v["changeAddress"] = json!(other);
    assert_eq!(app_code(r, "sendTransaction", v).await, 26);
    let mut v = send(1000);
    v["addresses"] = json!([other]);
    assert_eq!(app_code(r, "sendTransaction", v).await, 7);
    let mut v = send(1000);
    v["anonymity"] = json!(500);
    assert_eq!(
        fail(r, "sendTransaction", v).await,
        (-32700, Some(6), "MixIn count is too big".to_owned())
    );

    // a plain send: pending, with destination, change and input
    let hash = ok(r, "sendTransaction", send(1000)).await["transactionHash"].clone();
    assert_eq!(
        ok(r, "getUnconfirmedTransactionHashes", json!({})).await,
        json!({ "transactionHashes": [hash] })
    );
    assert_eq!(
        ok(
            r,
            "getUnconfirmedTransactionHashes",
            json!({ "addresses": [other] })
        )
        .await,
        json!({ "transactionHashes": [hash] })
    );
    let pending =
        ok(r, "getTransaction", json!({ "transactionHash": hash })).await["transaction"].clone();
    assert_eq!(pending["blockIndex"], json!(4_294_967_295_u32));
    assert_eq!(pending["timestamp"], json!(0));
    assert_eq!(pending["state"], json!(0));
    assert_eq!(pending["amount"], json!(-1010));
    let transfers = pending["transfers"].as_array().unwrap();
    assert_eq!(transfers.len(), 3, "{pending}");
    assert_eq!(
        transfers[0],
        json!({ "address": other, "amount": 1000, "type": 0 })
    );
    assert_eq!(
        (&transfers[1]["address"], &transfers[1]["type"]),
        (&json!(me), &json!(2))
    );
    assert_eq!(
        (&transfers[2]["address"], &transfers[2]["type"]),
        (&json!(me), &json!(0))
    );
    let sum: i64 = transfers
        .iter()
        .map(|t| t["amount"].as_i64().unwrap())
        .sum();
    assert_eq!(sum - 1000, -1010);

    // delayed: create, list, delete
    let delayed = ok(r, "createDelayedTransaction", send(2000)).await["transactionHash"].clone();
    assert_eq!(
        ok(r, "getDelayedTransactionHashes", json!({})).await,
        json!({ "transactionHashes": [delayed] })
    );
    let created = ok(r, "getTransaction", json!({ "transactionHash": delayed })).await;
    assert_eq!(created["transaction"]["state"], json!(3));
    assert_eq!(
        created["transaction"]["blockIndex"],
        json!(4_294_967_295_u32)
    );
    assert_eq!(
        ok(
            r,
            "deleteDelayedTransaction",
            json!({ "transactionHash": delayed })
        )
        .await,
        json!({})
    );
    assert_eq!(
        ok(r, "getDelayedTransactionHashes", json!({})).await,
        json!({ "transactionHashes": [] })
    );
    assert_eq!(
        app_code(r, "getTransaction", json!({ "transactionHash": delayed })).await,
        23
    );
    // a sent transaction is not delayed; unknown and upper-case hashes are not found
    assert_eq!(
        app_code(
            r,
            "deleteDelayedTransaction",
            json!({ "transactionHash": hash })
        )
        .await,
        12
    );
    assert_eq!(
        app_code(
            r,
            "sendDelayedTransaction",
            json!({ "transactionHash": hash })
        )
        .await,
        15
    );
    let upper = hash.as_str().unwrap().to_uppercase();
    assert_eq!(
        app_code(
            r,
            "sendDelayedTransaction",
            json!({ "transactionHash": upper })
        )
        .await,
        4
    );
    assert_eq!(
        fail(
            r,
            "deleteDelayedTransaction",
            json!({ "transactionHash": "ab".repeat(32) })
        )
        .await,
        (-32700, Some(4), "Requested object not found".to_owned())
    );
    assert_eq!(
        app_code(
            r,
            "sendDelayedTransaction",
            json!({ "transactionHash": "z" })
        )
        .await,
        3
    );
    // delayed: create and send
    let delayed = ok(r, "createDelayedTransaction", send(3000)).await["transactionHash"].clone();
    assert_eq!(
        ok(
            r,
            "sendDelayedTransaction",
            json!({ "transactionHash": delayed })
        )
        .await,
        json!({})
    );
    let unconfirmed = ok(r, "getUnconfirmedTransactionHashes", json!({})).await;
    assert_eq!(unconfirmed, json!({ "transactionHashes": [hash, delayed] }));

    // fusion checks
    let fusion = ok(r, "estimateFusion", json!({ "threshold": 1_000_000_000 })).await;
    assert!(fusion["totalOutputCount"].as_u64().unwrap() > 0);
    assert!(fusion["fusionReadyCount"].is_u64());
    assert_eq!(
        app_code(
            r,
            "estimateFusion",
            json!({ "threshold": 1, "addresses": [other] })
        )
        .await,
        7
    );
    assert_eq!(
        fail(r, "sendFusionTransaction", json!({ "threshold": 1 })).await,
        (-32700, Some(5), "Internal error occurred".to_owned())
    );
    assert_eq!(
        app_code(
            r,
            "sendFusionTransaction",
            json!({ "threshold": 100_000_000, "destinationAddress": other })
        )
        .await,
        28
    );

    // confirmed
    s.chain.mine_pending(&s.wallet, &s.other.address()).await;
    wait_synced(&s.wallet, s.chain.tip).await;
    let height = s.chain.height;
    let confirmed =
        ok(r, "getTransaction", json!({ "transactionHash": hash })).await["transaction"].clone();
    assert_eq!(confirmed["blockIndex"], json!(height));
    assert_eq!(confirmed["transfers"], pending["transfers"]);
    let block = ok(
        r,
        "getTransactionHashes",
        json!({ "firstBlockIndex": height, "blockCount": 1 }),
    )
    .await;
    let in_block: Vec<Value> = block["items"][0]["transactionHashes"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        in_block.contains(&hash) && in_block.contains(&delayed),
        "{block}"
    );
    assert_eq!(
        ok(r, "getUnconfirmedTransactionHashes", json!({})).await,
        json!({ "transactionHashes": [] })
    );

    // several addresses: a change address is required
    ok(r, "createAddress", json!({})).await;
    assert_eq!(
        fail(r, "sendTransaction", send(1000)).await,
        (-32700, Some(25), "Change address required".to_owned())
    );
    let mut v = send(1000);
    v["changeAddress"] = json!(me);
    ok(r, "sendTransaction", v).await;

    // export: a new container next to the wallet, never over an existing file
    assert_eq!(
        ok(r, "export", json!({ "fileName": "exported.wallet" })).await,
        json!({})
    );
    assert_eq!(
        fail(r, "export", json!({ "fileName": "exported.wallet" })).await,
        (-32700, Some(17), file_exists_text())
    );
    assert_eq!(fail(r, "export", json!({})).await.0, -32600);
    let exported = Wallet::open(
        s.dir.path().join("exported.wallet"),
        "container-pass",
        Node::new(s.chain.client.clone(), "router", 0),
        common::wallet_config(),
    )
    .await
    .unwrap();
    assert_eq!(exported.addresses(), s.wallet.addresses());
    // the container carries the history: the confirmed transactions are there before a scan
    let confirmed = |w: &Wallet<RouterTransport>| -> std::collections::BTreeSet<Hash> {
        w.transactions(None, None)
            .unwrap()
            .iter()
            .map(|t| t.hash)
            .collect()
    };
    assert!(!confirmed(&exported).is_empty());
    assert_eq!(confirmed(&exported), confirmed(&s.wallet));
    exported.close().await.unwrap();

    // save and reset: the wallet scans again and finds the same confirmed history
    s.chain.mine_pending(&s.wallet, &s.other.address()).await;
    wait_synced(&s.wallet, s.chain.tip).await;
    assert_eq!(ok(r, "save", json!({})).await, json!({}));
    let before = ok(r, "getBalance", json!({ "address": me })).await;
    assert_eq!(ok(r, "reset", json!({})).await, json!({}));
    wait_synced(&s.wallet, s.chain.tip).await;
    assert_eq!(ok(r, "getBalance", json!({ "address": me })).await, before);
    assert_eq!(
        fail(r, "reset", json!({ "scanHeight": "x" })).await.0,
        -32600
    );
}

/// `strerror(EEXIST)` as the system words it.
fn file_exists_text() -> String {
    xkr_walletd::errors::AppError::system(17).message
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracking_container() {
    let mut chain = Chain::new();
    let me = keys("walletd tracking wallet");
    chain.mine_many(&me.address(), 3);
    let dir = tempfile::tempdir().unwrap();
    let wallet = Wallet::import_view(
        dir.path().join("tracking.wallet"),
        "pässwörd ✓",
        me.view_secret,
        &me.address().to_string(),
        0,
        chain.node(),
        wallet_config(),
    )
    .await
    .unwrap();
    wait_synced(&wallet, chain.tip).await;
    let r = router(Arc::new(Service::new(
        Arc::new(wallet),
        chain.client.clone(),
        Network::Testnet,
        RpcSettings::with_password(PASSWORD, ""),
    )));
    let r = &r;
    let address = me.address().to_string();
    assert_eq!(
        ok(r, "getSpendKeys", json!({ "address": address })).await,
        json!({
            "spendPublicKey": me.address().spend.to_hex(),
            "spendSecretKey": "00".repeat(32),
        })
    );
    assert_eq!(
        fail(r, "getMnemonicSeed", json!({ "address": address })).await,
        (-32700, Some(6), "Keys not deterministic".to_owned())
    );
    let send = json!({ "transfers": [{ "address": address, "amount": 1000 }], "fee": 10 });
    assert_eq!(
        fail(r, "sendTransaction", send.clone()).await,
        (
            -32700,
            Some(21),
            "The wallet is in tracking mode".to_owned()
        )
    );
    assert_eq!(app_code(r, "createDelayedTransaction", send).await, 21);
    assert_eq!(
        app_code(r, "getDelayedTransactionHashes", json!({})).await,
        21
    );
    assert_eq!(app_code(r, "createAddress", json!({})).await, 22);
    let secret_key = keys("walletd tracking sub").spend_secret.to_hex();
    assert_eq!(
        app_code(r, "createAddress", json!({ "spendSecretKey": secret_key })).await,
        22
    );
    let tracked = keys("walletd tracking other").address();
    let added = ok(
        r,
        "createAddress",
        json!({ "spendPublicKey": tracked.spend.to_hex(), "scanHeight": 0 }),
    )
    .await;
    assert_eq!(
        added["address"],
        json!(xkr_wallet_core::Address::new(tracked.spend, me.address().view).to_string())
    );
    assert_eq!(
        ok(r, "estimateFusion", json!({ "threshold": 100_000_000 })).await,
        json!({ "fusionReadyCount": 0, "totalOutputCount": 0 })
    );
    let balance = ok(r, "getBalance", json!({ "address": address })).await;
    assert!(
        balance["availableBalance"].as_u64().unwrap() + balance["lockedAmount"].as_u64().unwrap()
            > 0
    );
}
