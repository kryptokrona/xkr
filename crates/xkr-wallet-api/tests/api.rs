//! The routes end to end: authentication, CORS and `OPTIONS`, routing, wallet states, every
//! handler, and the pinned behaviour of the C++ quirks (unconfirmed transactions per address,
//! `publicKey` transfers on `/transactions/hash`, `/balances` as `null` when empty, 200 for
//! `/transactions/send/advanced`, 400 with an empty body for a bad height). Wallets talk to an
//! in-process xkrd on a fresh testnet chain.

mod common;

use common::{
    A_SPEND, A_VIEW, Api, CORS, DUMMY_ADDR, K_SPEND, K_VIEW, KEY, PID1, SEED_A, TestNode,
    UNKNOWN_HASH, wallet_params,
};
use serde_json::{Value, json};
use xkr_wallet::validate::create_integrated_address;
use xkr_wallet::{ErrorCode, WalletError};
use xkr_wallet_api::cors::{ALLOWED_HEADERS, ALLOWED_METHODS};
use xkr_wallet_api::respond::{dump, error_body};
use xkr_wallet_core::{ADDRESS_PREFIX_ALT, Address, WalletKeys, secret_from_mnemonic};

/// Wallet A's address (seed "skulls woozy ...").
fn address_a() -> String {
    let spend = secret_from_mnemonic(SEED_A).expect("seed A");
    WalletKeys::from_spend_secret(&spend)
        .expect("keys")
        .address()
        .to_string()
}

/// The same keys under the `Xkr` prefix.
fn xkr_form(address: &str) -> String {
    let mut a = Address::parse(address).expect("address");
    a.prefix = ADDRESS_PREFIX_ALT;
    a.encode()
}

/// The exact error body of `code` with its default text.
fn error_text(code: ErrorCode) -> String {
    dump(&error_body(&WalletError::new(code)))
}

/// Routes that need an open wallet, with a body for the ones that read one.
fn open_routes(a: &str) -> Vec<(&'static str, String)> {
    let h = UNKNOWN_HASH;
    vec![
        ("DELETE", "/wallet".into()),
        ("POST", "/addresses/create".into()),
        ("POST", "/addresses/import".into()),
        ("POST", "/addresses/import/view".into()),
        ("DELETE", format!("/addresses/{a}")),
        ("PUT", "/save".into()),
        ("PUT", "/reset".into()),
        ("PUT", "/node".into()),
        ("GET", "/node".into()),
        ("GET", "/keys".into()),
        ("GET", format!("/keys/{a}")),
        ("GET", format!("/keys/mnemonic/{a}")),
        ("GET", "/status".into()),
        ("GET", "/addresses".into()),
        ("GET", "/addresses/primary".into()),
        ("GET", format!("/addresses/{a}/{PID1}")),
        ("GET", "/transactions".into()),
        ("GET", "/transactions/unconfirmed".into()),
        ("GET", format!("/transactions/unconfirmed/{a}")),
        ("GET", "/transactions/0".into()),
        ("GET", "/transactions/0/10".into()),
        ("GET", format!("/transactions/address/{a}/0")),
        ("GET", format!("/transactions/address/{a}/0/10")),
        ("GET", format!("/transactions/privatekey/{h}")),
        ("GET", format!("/transactions/hash/{h}")),
        ("GET", "/balance".into()),
        ("GET", format!("/balance/{a}")),
        ("GET", "/balances".into()),
        ("POST", "/transactions/send/basic".into()),
        ("POST", "/transactions/send/advanced".into()),
        ("POST", "/transactions/send/fusion/basic".into()),
        ("POST", "/transactions/send/fusion/advanced".into()),
    ]
}

const CLOSED_ROUTES: [&str; 5] = [
    "/wallet/open",
    "/wallet/import/key",
    "/wallet/import/seed",
    "/wallet/import/view",
    "/wallet/create",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_options_cors_and_routing() {
    let node = TestNode::new();
    let api = Api::new(&node, None);
    let cors = Api::new(&node, Some(CORS));

    // X-API-KEY: missing or wrong is 401 with an empty body
    let r = api.request("GET", "/status", &[], None).await;
    assert_eq!((r.status, r.text.as_str()), (401, ""));
    assert_eq!(r.header("content-type"), None);
    let r = api
        .request("GET", "/status", &[("X-API-KEY", "wrong")], None)
        .await;
    assert_eq!(r.status, 401);
    let r = api
        .request(
            "POST",
            "/wallet/open",
            &[("x-api-key", "wrong"), ("Content-Type", "application/json")],
            Some(r#"{"filename":"x","password":"y"}"#),
        )
        .await;
    assert_eq!(r.status, 401);
    // the right key and no wallet: 403
    let r = api.get("/status").await;
    assert_eq!((r.status, r.text.as_str()), (403, ""));
    // with --enable-cors every answer past the router carries the origin, 401 and 403 too
    let r = cors
        .request(
            "GET",
            "/status",
            &[("Origin", CORS), ("X-API-KEY", "wrong")],
            None,
        )
        .await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("access-control-allow-origin"), Some(CORS));
    let r = cors.get("/status").await;
    assert_eq!(r.status, 403);
    assert_eq!(r.header("access-control-allow-origin"), Some(CORS));
    assert_eq!(
        api.get("/status")
            .await
            .header("access-control-allow-origin"),
        None
    );

    // OPTIONS: any path, no key needed
    let r = api.request("OPTIONS", "/balance", &[], None).await;
    assert_eq!((r.status, r.text.as_str()), (200, ""));
    assert_eq!(r.header("allow"), Some(""));
    assert_eq!(r.header("access-control-allow-origin"), None);
    let preflight = [("Origin", CORS), ("Access-Control-Request-Method", "POST")];
    let r = api
        .request("OPTIONS", "/wallet/open", &preflight, None)
        .await;
    assert_eq!(r.header("access-control-allow-methods"), Some(""));
    assert_eq!(r.header("allow"), None);
    let r = cors
        .request("OPTIONS", "/wallet/open", &preflight, None)
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("access-control-allow-methods"),
        Some(ALLOWED_METHODS)
    );
    assert_eq!(r.header("access-control-allow-origin"), Some(CORS));
    assert_eq!(
        r.header("access-control-allow-headers"),
        Some(ALLOWED_HEADERS)
    );
    let r = cors
        .request("OPTIONS", "/no/such/route", &[("Origin", CORS)], None)
        .await;
    assert_eq!(r.header("allow"), Some(ALLOWED_METHODS));

    // no route: 404 without the CORS header; an unknown method: 400
    let r = cors.get("/no/such/route").await;
    assert_eq!((r.status, r.text.as_str()), (404, ""));
    assert_eq!(r.header("access-control-allow-origin"), None);
    assert_eq!(api.get("/transactions/12x").await.status, 404);
    assert_eq!(api.get("/status/").await.status, 404);
    assert_eq!(api.call("POST", "/status", None).await.status, 404);
    assert_eq!(
        api.request("PATCH", "/status", &[("X-API-KEY", KEY)], None)
            .await
            .status,
        400
    );
    // HEAD uses the GET routes; the path is percent-decoded
    assert_eq!(api.call("HEAD", "/status", None).await.status, 403);
    assert_eq!(api.get("/sta%74us").await.status, 403);

    // every route that needs a wallet: 403 while none is open
    let a = address_a();
    for (method, path) in open_routes(&a) {
        let body = json!({});
        let body = matches!(method, "POST" | "PUT").then_some(&body);
        let r = api.call(method, &path, body).await;
        assert_eq!((r.status, r.text.as_str()), (403, ""), "{method} {path}");
    }
    // a body that is not JSON reads as null: the required filename is missing
    let r = api
        .request(
            "POST",
            "/wallet/open",
            &[("X-API-KEY", KEY), ("Content-Type", "application/json")],
            Some("this is not json"),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallets_open_import_create_close() {
    let node = TestNode::new();
    let api = Api::new(&node, None);
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let a = address_a();

    // create, then every closed-wallet route is 403
    let r = api
        .post(
            "/wallet/create",
            &wallet_params(d, "c.wallet", "pw", &json!({})),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (200, ""), "{r:?}");
    for path in CLOSED_ROUTES {
        let r = api
            .post(path, &wallet_params(d, "other.wallet", "x", &json!({})))
            .await;
        assert_eq!(r.status, 403, "{path}");
    }
    let created = api.get("/addresses/primary").await.json()["address"]
        .as_str()
        .unwrap()
        .to_owned();
    let seed = api.get(&format!("/keys/mnemonic/{created}")).await;
    assert_eq!(seed.status, 200);
    assert_eq!(
        seed.json()["mnemonicSeed"]
            .as_str()
            .unwrap()
            .split(' ')
            .count(),
        25
    );
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 200);
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 403);

    // open errors
    let r = api
        .post(
            "/wallet/open",
            &wallet_params(d, "c.wallet", "nope", &json!({})),
        )
        .await;
    assert_eq!(r.text, error_text(ErrorCode::WrongPassword));
    assert_eq!(r.header("content-type"), Some("application/json"));
    let r = api
        .post(
            "/wallet/open",
            &wallet_params(d, "missing.wallet", "pw", &json!({})),
        )
        .await;
    assert_eq!(r.error_code(), 1);
    let r = api
        .post("/wallet/open", &json!({ "filename": d.join("c.wallet") }))
        .await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
    let r = api
        .post(
            "/wallet/create",
            &wallet_params(d, "c.wallet", "pw", &json!({})),
        )
        .await;
    assert_eq!(r.text, error_text(ErrorCode::WalletFileAlreadyExists));
    // wrong types are 400 with an empty body
    let mut p = wallet_params(d, "c.wallet", "pw", &json!({}));
    p["daemonHost"] = json!(5);
    assert_eq!(api.post("/wallet/open", &p).await.text, "");

    // seed A
    let r = api
        .post(
            "/wallet/import/seed",
            &wallet_params(
                d,
                "a.wallet",
                "pass-a",
                &json!({ "mnemonicSeed": SEED_A, "scanHeight": 0 }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        api.get("/addresses/primary").await.json(),
        json!({ "address": a })
    );
    let keys = api.get(&format!("/keys/{a}")).await.json();
    assert_eq!(keys["privateSpendKey"], json!(A_SPEND));
    assert_eq!(
        api.get("/keys").await.json(),
        json!({ "privateViewKey": A_VIEW })
    );
    assert_eq!(
        api.get(&format!("/keys/mnemonic/{a}")).await.json(),
        json!({ "mnemonicSeed": SEED_A })
    );
    // an address that is valid but not the wallet's: ADDRESS_NOT_IN_WALLET with its own text
    assert_eq!(
        api.get(&format!("/keys/{DUMMY_ADDR}")).await.text,
        error_text(ErrorCode::AddressNotInWallet)
    );
    assert_eq!(
        api.get(&format!("/keys/mnemonic/{DUMMY_ADDR}")).await.text,
        error_text(ErrorCode::AddressNotInWallet)
    );
    let status = api.get("/status").await.json();
    assert_eq!(status["isViewWallet"], json!(false));
    assert_eq!(status["subWalletCount"], json!(1));
    for key in [
        "walletBlockCount",
        "localDaemonBlockCount",
        "networkBlockCount",
        "peerCount",
        "hashrate",
    ] {
        assert!(status[key].is_u64(), "{key}: {status}");
    }
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 200);

    // seed errors carry the C++ codes
    let bad = SEED_A.replace("skulls", "zzzzzz");
    let r = api
        .post(
            "/wallet/import/seed",
            &wallet_params(d, "bad.wallet", "x", &json!({ "mnemonicSeed": bad })),
        )
        .await;
    assert_eq!(r.error_code(), 33);
    let r = api
        .post(
            "/wallet/import/seed",
            &wallet_params(
                d,
                "bad.wallet",
                "x",
                &json!({ "mnemonicSeed": "skulls woozy" }),
            ),
        )
        .await;
    assert_eq!(r.error_code(), 34);
    assert_eq!(
        api.post(
            "/wallet/import/seed",
            &wallet_params(d, "bad.wallet", "x", &json!({}))
        )
        .await
        .text,
        ""
    );

    // keys K: not deterministic
    let r = api
        .post(
            "/wallet/import/key",
            &wallet_params(
                d,
                "k.wallet",
                "pass-k",
                &json!({ "privateViewKey": K_VIEW, "privateSpendKey": K_SPEND }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let k = api.get("/addresses/primary").await.json()["address"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        api.get(&format!("/keys/mnemonic/{k}")).await.text,
        error_text(ErrorCode::KeysNotDeterministic)
    );
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 200);
    let r = api
        .post(
            "/wallet/import/key",
            &wallet_params(
                d,
                "k2.wallet",
                "x",
                &json!({ "privateViewKey": K_VIEW, "privateSpendKey": "zz" }),
            ),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
    // any 64 hex characters are a key here: one at or above the group order is used reduced
    let r = api
        .post(
            "/wallet/import/key",
            &wallet_params(
                d,
                "k3.wallet",
                "x",
                &json!({ "privateViewKey": K_VIEW, "privateSpendKey": "ff".repeat(32) }),
            ),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (200, ""), "{r:?}");
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 200);

    // a view wallet: an address that does not parse is 500 (C++ throws inside the import)
    let r = api
        .post(
            "/wallet/import/view",
            &wallet_params(
                d,
                "v0.wallet",
                "x",
                &json!({ "privateViewKey": A_VIEW, "address": "SEKRnotanaddress" }),
            ),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (500, ""));
    let r = api
        .post(
            "/wallet/import/view",
            &wallet_params(
                d,
                "v.wallet",
                "pass-view",
                &json!({ "privateViewKey": A_VIEW, "address": a, "scanHeight": 0 }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api.get("/status").await.json()["isViewWallet"], json!(true));
    let banned = error_text(ErrorCode::IllegalViewWalletOperation);
    for (method, path, body) in [
        ("POST", "/addresses/create".to_owned(), Some(json!({}))),
        ("POST", "/addresses/import".to_owned(), Some(json!({}))),
        (
            "POST",
            "/transactions/send/basic".to_owned(),
            Some(json!({})),
        ),
        (
            "POST",
            "/transactions/send/advanced".to_owned(),
            Some(json!({})),
        ),
        (
            "POST",
            "/transactions/send/fusion/basic".to_owned(),
            Some(json!({})),
        ),
        (
            "POST",
            "/transactions/send/fusion/advanced".to_owned(),
            Some(json!({})),
        ),
        ("GET", format!("/keys/{a}"), None),
        ("GET", format!("/keys/mnemonic/{a}"), None),
        (
            "GET",
            format!("/transactions/privatekey/{UNKNOWN_HASH}"),
            None,
        ),
    ] {
        let r = api.call(method, &path, body.as_ref()).await;
        assert_eq!((r.status, &r.text), (400, &banned), "{method} {path}");
    }
    assert_eq!(
        api.get("/keys").await.json(),
        json!({ "privateViewKey": A_VIEW })
    );
    assert_eq!(api.get("/balance").await.status, 200);
    assert_eq!(api.call("DELETE", "/wallet", None).await.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn addresses_keys_node_save_reset() {
    let node = TestNode::new();
    let api = Api::new(&node, None);
    let dir = tempfile::tempdir().unwrap();
    let a = address_a();
    let r = api
        .post(
            "/wallet/import/seed",
            &wallet_params(
                dir.path(),
                "a.wallet",
                "pw",
                &json!({ "mnemonicSeed": SEED_A }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    let r = api.post("/addresses/create", &json!({})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let created = r.json();
    let random = created["address"].as_str().unwrap().to_owned();
    assert_eq!(created["privateSpendKey"].as_str().unwrap().len(), 64);
    assert_eq!(r.text, dump(&created));

    let sub_key = "ab".repeat(31) + "0a";
    let r = api
        .post(
            "/addresses/import",
            &json!({ "privateSpendKey": sub_key, "scanHeight": 0 }),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let sub = r.json()["address"].as_str().unwrap().to_owned();
    let r = api
        .post("/addresses/import", &json!({ "privateSpendKey": sub_key }))
        .await;
    assert_eq!(r.text, error_text(ErrorCode::SubwalletAlreadyExists));
    let r = api
        .post("/addresses/import", &json!({ "privateSpendKey": "zz" }))
        .await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
    assert_eq!(api.post("/addresses/import", &json!({})).await.text, "");
    let r = api
        .post(
            "/addresses/import/view",
            &json!({ "publicSpendKey": "cd".repeat(32) }),
        )
        .await;
    assert_eq!(r.text, error_text(ErrorCode::IllegalNonViewWalletOperation));

    let list = api.get("/addresses").await.json();
    assert_eq!(list, json!({ "addresses": [a, random, sub] }));
    let balances = api.get("/balances").await;
    assert_eq!(balances.status, 200);
    let rows = balances.json();
    assert_eq!(rows.as_array().map(Vec::len), Some(3));
    assert_eq!(rows[0], json!({ "address": a, "unlocked": 0, "locked": 0 }));
    assert_eq!(
        api.get(&format!("/balance/{sub}")).await.json(),
        json!({ "unlocked": 0, "locked": 0 })
    );
    let r = api.get(&format!("/balance/{DUMMY_ADDR}")).await;
    assert_eq!(r.error_code(), 10);

    // deleting
    let r = api.call("DELETE", &format!("/addresses/{a}"), None).await;
    assert_eq!(r.text, error_text(ErrorCode::CannotDeletePrimaryAddress));
    let r = api
        .call("DELETE", &format!("/addresses/{DUMMY_ADDR}"), None)
        .await;
    assert_eq!(r.text, error_text(ErrorCode::AddressNotInWallet));
    let r = api
        .call("DELETE", &format!("/addresses/{random}"), None)
        .await;
    assert_eq!((r.status, r.text.as_str()), (200, ""));
    assert_eq!(
        api.get("/addresses").await.json(),
        json!({ "addresses": [a, sub] })
    );

    // integrated addresses; the routes take the Xkr form of an address, never an integrated one
    let integrated = create_integrated_address(&a, PID1).unwrap();
    assert_eq!(
        api.get(&format!("/addresses/{a}/{PID1}")).await.json(),
        json!({ "integratedAddress": integrated })
    );
    let xa = xkr_form(&a);
    assert!(xa.starts_with("Xkr") && xa.len() == 98, "{xa}");
    assert_eq!(
        api.get(&format!("/addresses/{xa}/{PID1}")).await.json(),
        json!({ "integratedAddress": integrated })
    );
    assert_eq!(
        api.get(&format!("/addresses/{integrated}/{PID1}"))
            .await
            .status,
        404
    );
    assert_eq!(
        api.get(&format!("/addresses/{a}/{}", &PID1[1..]))
            .await
            .status,
        404
    );
    assert_eq!(api.get(&format!("/balance/{integrated}")).await.status, 404);
    assert_eq!(
        api.get(&format!("/transactions/address/{integrated}/0"))
            .await
            .status,
        404
    );
    assert_eq!(api.get(&format!("/balance/{xa}")).await.status, 200);
    assert_eq!(
        api.get(&format!("/keys/{xa}")).await.json()["privateSpendKey"],
        json!(A_SPEND)
    );
    let typo = format!("{}{}", &a[..98], if a.ends_with('a') { "b" } else { "a" });
    assert_eq!(
        api.get(&format!("/addresses/{typo}/{PID1}"))
            .await
            .error_code(),
        15
    );

    // node, save, reset
    assert_eq!(
        api.get("/node").await.json(),
        json!({ "daemonHost": "127.0.0.1", "daemonPort": 24899, "nodeFee": 0, "nodeAddress": "" })
    );
    let r = api
        .call(
            "PUT",
            "/node",
            Some(&json!({ "daemonHost": "localhost", "daemonPort": 11899 })),
        )
        .await;
    assert_eq!((r.status, r.text.as_str()), (202, ""));
    assert_eq!(
        api.get("/node").await.json(),
        json!({ "daemonHost": "localhost", "daemonPort": 11899, "nodeFee": 0, "nodeAddress": "" })
    );
    let r = api
        .call("PUT", "/node", Some(&json!({ "daemonHost": "127.0.0.1" })))
        .await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
    assert_eq!(api.call("PUT", "/save", None).await.status, 200);
    assert_eq!(
        api.call("PUT", "/reset", Some(&json!({}))).await.status,
        200
    );
    assert_eq!(
        api.call("PUT", "/reset", Some(&json!({ "scanHeight": 5 })))
            .await
            .status,
        200
    );
    assert_eq!(
        api.call("PUT", "/reset", Some(&json!({ "scanHeight": "5" })))
            .await
            .status,
        400
    );
    assert!(api.app.close_wallet().await);
    assert!(!api.app.close_wallet().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactions_sends_and_pinned_quirks() {
    let node = TestNode::new();
    let api = Api::new(&node, None);
    let dir = tempfile::tempdir().unwrap();
    let a = address_a();
    let r = api
        .post(
            "/wallet/import/seed",
            &wallet_params(
                dir.path(),
                "a.wallet",
                "pw",
                &json!({ "mnemonicSeed": SEED_A, "scanHeight": 0 }),
            ),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    node.mine_to(&a, 20).await;
    node.mine_to(DUMMY_ADDR, 80).await;
    api.wait_synced(&node).await;

    let balance = api.get("/balance").await.json();
    let unlocked = balance["unlocked"].as_u64().unwrap();
    assert!(unlocked > 0, "{balance}");

    // every block 1..=20 paid A
    let all = api.get("/transactions").await;
    assert_eq!(all.text, dump(&all.json()));
    let txs = all.json()["transactions"].as_array().unwrap().clone();
    assert_eq!(txs.len(), 20);
    let first = &txs[0];
    assert_eq!(first["isCoinbaseTransaction"], json!(true));
    assert_eq!(first["transfers"][0]["address"], json!(a));
    assert!(first["transfers"][0]["amount"].as_i64().unwrap() > 0);
    let mut keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "blockHeight",
            "fee",
            "hash",
            "isCoinbaseTransaction",
            "paymentID",
            "timestamp",
            "transfers",
            "unlockTime"
        ]
    );
    let heights = |v: &Value| -> Vec<u64> {
        v["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["blockHeight"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(
        heights(&api.get("/transactions/5").await.json()),
        (5..=20).collect::<Vec<_>>()
    );
    assert_eq!(
        heights(&api.get("/transactions/5/7").await.json()),
        vec![5, 6]
    );
    assert_eq!(
        heights(
            &api.get(&format!("/transactions/address/{a}/0/5"))
                .await
                .json()
        ),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        heights(
            &api.get(&format!("/transactions/address/{a}/18"))
                .await
                .json()
        ),
        vec![18, 19, 20]
    );
    // a valid address that is not the wallet's lists nothing; the Xkr form lists nothing either
    // (the subwallet's address is compared as text)
    assert!(
        heights(
            &api.get(&format!("/transactions/address/{DUMMY_ADDR}/0"))
                .await
                .json()
        )
        .is_empty()
    );
    assert!(
        heights(
            &api.get(&format!("/transactions/address/{}/0", xkr_form(&a)))
                .await
                .json()
        )
        .is_empty()
    );
    let typo = format!("{}{}", &a[..98], if a.ends_with('a') { "b" } else { "a" });
    assert_eq!(
        api.get(&format!("/transactions/address/{typo}/0"))
            .await
            .error_code(),
        15
    );
    // bad heights: 400 with an empty body
    for path in [
        "/transactions/7/7".to_owned(),
        "/transactions/8/7".to_owned(),
        "/transactions/99999999999999999999".to_owned(),
        "/transactions/0/99999999999999999999".to_owned(),
        format!("/transactions/address/{a}/99999999999999999999"),
    ] {
        let r = api.get(&path).await;
        assert_eq!((r.status, r.text.as_str()), (400, ""), "{path}");
    }
    // a start so large that start + 1000 wraps lists nothing
    assert!(
        heights(
            &api.get(&format!("/transactions/{}", u64::MAX - 5))
                .await
                .json()
        )
        .is_empty()
    );

    // /transactions/hash names transfers by public spend key
    let hash = first["hash"].as_str().unwrap().to_owned();
    let one = api.get(&format!("/transactions/hash/{hash}")).await.json();
    let a_public = Address::parse(&a).unwrap().spend.to_hex();
    assert_eq!(
        one["transaction"]["transfers"],
        json!([{ "publicKey": a_public, "amount": first["transfers"][0]["amount"] }])
    );
    assert_eq!(one["transaction"]["blockHeight"], first["blockHeight"]);
    let r = api.get(&format!("/transactions/hash/{UNKNOWN_HASH}")).await;
    assert_eq!((r.status, r.text.as_str()), (404, ""));
    assert_eq!(
        api.get(&format!("/transactions/privatekey/{UNKNOWN_HASH}"))
            .await
            .text,
        error_text(ErrorCode::TxPrivateKeyNotFound)
    );

    // sends: basic answers 201, advanced 200
    let r = api
        .post(
            "/transactions/send/basic",
            &json!({ "destination": DUMMY_ADDR, "amount": 1000 }),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let basic = r.json()["transactionHash"].as_str().unwrap().to_owned();
    let r = api
        .post(
            "/transactions/send/advanced",
            &json!({ "destinations": [{ "address": DUMMY_ADDR, "amount": 2000 }], "mixin": 0, "fee": 10 }),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let advanced = r.json()["transactionHash"].as_str().unwrap().to_owned();
    assert_eq!(node.pool_len(), 2);

    let pending = api.get("/transactions/unconfirmed").await.json();
    let mut hashes: Vec<String> = pending["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            assert_eq!(t["blockHeight"], json!(0));
            assert_eq!(t["transfers"][0]["address"], json!(a));
            t["hash"].as_str().unwrap().to_owned()
        })
        .collect();
    hashes.sort();
    let mut want = vec![basic.clone(), advanced.clone()];
    want.sort();
    assert_eq!(hashes, want);
    // per address: the unconfirmed transactions with a transfer of that address
    assert_eq!(
        api.get(&format!("/transactions/unconfirmed/{a}"))
            .await
            .json(),
        pending
    );
    assert_eq!(
        api.get(&format!("/transactions/unconfirmed/{DUMMY_ADDR}"))
            .await
            .json(),
        json!({ "transactions": [] })
    );
    // unconfirmed transactions are not found by hash
    assert_eq!(
        api.get(&format!("/transactions/hash/{basic}")).await.status,
        404
    );
    let key = api.get(&format!("/transactions/privatekey/{basic}")).await;
    assert_eq!(key.status, 200);
    assert_eq!(
        key.json()["transactionPrivateKey"].as_str().unwrap().len(),
        64
    );

    // send errors
    let r = api
        .post(
            "/transactions/send/advanced",
            &json!({ "destinations": [{ "address": DUMMY_ADDR, "amount": 2000 }], "fee": 1 }),
        )
        .await;
    assert_eq!(r.error_code(), 17);
    let r = api
        .post(
            "/transactions/send/advanced",
            &json!({ "destinations": [] }),
        )
        .await;
    assert_eq!(r.error_code(), 18);
    // a payment ID must be 64 hex characters
    let r = api
        .post(
            "/transactions/send/advanced",
            &json!({
                "destinations": [{ "address": DUMMY_ADDR, "amount": 2000 }],
                "paymentID": format!("zz{}", &PID1[2..]),
            }),
        )
        .await;
    assert_eq!(r.error_code(), 24);
    let r = api.post("/transactions/send/advanced", &json!({})).await;
    assert_eq!((r.status, r.text.as_str()), (400, ""));
    let r = api
        .post(
            "/transactions/send/basic",
            &json!({ "destination": DUMMY_ADDR, "amount": 0 }),
        )
        .await;
    assert_eq!(r.error_code(), 19);
    let r = api
        .post(
            "/transactions/send/basic",
            &json!({ "destination": DUMMY_ADDR, "amount": 9_000_000_000_000_000u64 }),
        )
        .await;
    assert_eq!(r.error_code(), 11);
    let r = api
        .post(
            "/transactions/send/fusion/advanced",
            &json!({ "destination": DUMMY_ADDR }),
        )
        .await;
    assert_eq!(r.error_code(), 10);

    // mined: confirmed, found by hash, nothing pending
    node.mine(DUMMY_ADDR, 12).await;
    api.wait_synced(&node).await;
    assert_eq!(
        api.get("/transactions/unconfirmed").await.json(),
        json!({ "transactions": [] })
    );
    let sent = api
        .get(&format!("/transactions/hash/{advanced}"))
        .await
        .json();
    assert_eq!(sent["transaction"]["fee"], json!(10));
    assert_eq!(sent["transaction"]["isCoinbaseTransaction"], json!(false));
    assert_eq!(
        sent["transaction"]["transfers"][0]["publicKey"],
        json!(a_public)
    );
    assert_eq!(sent["transaction"]["transfers"][0]["amount"], json!(-2010));
    let status = api.get("/status").await.json();
    assert_eq!(status["walletBlockCount"], json!(node.top()));
    assert_eq!(status["networkBlockCount"], json!(node.top()));
    assert!(api.app.close_wallet().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_wallet_at_a_time() {
    let node = TestNode::new();
    let api = Api::new(&node, None);
    let dir = tempfile::tempdir().unwrap();
    let p1 = wallet_params(dir.path(), "one.wallet", "pw", &json!({}));
    let p2 = wallet_params(dir.path(), "two.wallet", "pw", &json!({}));
    let (r1, r2) = tokio::join!(
        api.post("/wallet/create", &p1),
        api.post("/wallet/create", &p2)
    );
    let mut statuses = [r1.status, r2.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 403]);
    // requests share the open wallet
    let (x, y) = tokio::join!(api.get("/addresses"), api.get("/addresses"));
    assert_eq!(x.json(), y.json());
    assert!(api.app.is_open().await);
    assert!(api.app.close_wallet().await);
    assert!(!api.app.is_open().await);
}
