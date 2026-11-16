//! Request-limit tests for the HTTP API: the axum body limit (2 MB, axum's `DefaultBodyLimit`)
//! applies to every handler, JSON nesting is bounded by `serde_json`'s recursion limit (128),
//! garbage bodies are answered, never panic, and `/getrandom_outs` caps the total number of outputs
//! per call, the other request lists are capped, and the `Host` / `Origin` checks refuse DNS
//! rebinding and cross-site requests. Requests go straight into the router
//! (`tower::ServiceExt::oneshot`), no socket.

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TempDb, json_body, post, state};
use serde_json::Value;
use tower::ServiceExt;
use xkr_rpc::{AccessPolicy, router, router_with_access};

fn app(db: &TempDb) -> Router {
    router(state(db), &[])
}

const PATHS: [&str; 10] = [
    "/json_rpc",
    "/getwalletsyncdata",
    "/get_global_indexes_for_range",
    "/get_transactions_status",
    "/get_o_indexes",
    "/gettransactions",
    "/sendrawtransaction",
    "/getrandom_outs",
    "/get_pool_changes_lite",
    "/getinfo",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bodies_over_two_megabytes_are_refused() {
    let db = TempDb::new();
    let app = app(&db);
    for path in PATHS.iter().take(9) {
        let (status, _) = post(&app, path, vec![b' '; 2 * 1024 * 1024 + 1]).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_and_garbage_json_is_answered_quickly() {
    let db = TempDb::new();
    let app = app(&db);
    let bodies: Vec<Vec<u8>> = vec![
        "[".repeat(100_000).into_bytes(),
        "{\"a\":".repeat(50_000).into_bytes(),
        b"\xff\xfe\x00garbage".to_vec(),
        br#"{"jsonrpc":"2.0","id":1,"method":"getblockheaderbyheight","params":{"height":18446744073709551615}}"#.to_vec(),
        br#"{"jsonrpc":"2.0","id":1,"method":"on_getblockhash","params":[18446744073709551615]}"#.to_vec(),
        br#"{"jsonrpc":"2.0","id":1,"method":"f_blocks_list_json","params":{"height":18446744073709551615}}"#.to_vec(),
        br#"{"jsonrpc":"2.0","id":1,"method":"submitblock","params":["zz"]}"#.to_vec(),
        br#"{"startHeight":18446744073709551615,"blockCount":18446744073709551615}"#.to_vec(),
    ];
    for path in PATHS {
        for body in &bodies {
            let t = Instant::now();
            let (status, _) = post(&app, path, body.clone()).await;
            assert!(t.elapsed() < Duration::from_secs(10), "{path}");
            assert!(
                status.is_success() || status.is_client_error(),
                "{path}: {status}"
            );
        }
    }
    // the JSON-RPC endpoint answers too-deep JSON with a parse error
    let (status, body) = post(&app, "/json_rpc", "[".repeat(100_000).into_bytes()).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["error"]["code"], -32700);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_outs_total_is_capped() {
    let db = TempDb::new();
    let app = app(&db);
    let amounts = vec![0u64; 1000];
    let body = serde_json::to_vec(&serde_json::json!({ "amounts": amounts, "outs_count": 65535 }))
        .expect("json");
    let t = Instant::now();
    let (status, body) = post(&app, "/getrandom_outs", body).await;
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "Failed");
    assert_eq!(v["outs"], serde_json::json!([]));
}

/// `/gettransactions` answers each distinct hash once: 25 000 copies of the genesis miner
/// transaction's hash (the most that fit in the 2 MB body) give one blob, not 25 000.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_transactions_answers_each_hash_once() {
    let db = TempDb::new();
    let app = app(&db);
    let blob = hex::decode(xkr_consensus::params::GENESIS_COINBASE_TX_HEX).expect("hex");
    let hash = xkr_serialization::transaction_hash_from_blob(&blob).to_string();
    let hashes = vec![hash; 25_000];
    let body = serde_json::to_vec(&serde_json::json!({ "txs_hashes": hashes })).expect("json");
    assert!(body.len() < 2 * 1024 * 1024);
    let t = Instant::now();
    let (status, body) = post(&app, "/gettransactions", body).await;
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(
        v["txs_as_hex"],
        serde_json::json!([hex::encode(blob)]),
        "{v}"
    );
    assert_eq!(v["missed_tx"], serde_json::json!([]));
}

/// Sends a request with the given headers; returns the status, the response headers and body.
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .expect("body");
    (status, headers, bytes.to_vec())
}

/// The `Host` must be loopback, `localhost` or the bound IP (any IP literal on `0.0.0.0`), so a
/// rebinding DNS name is refused; requests without `Host` (HTTP/1.0, not a browser) pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_host_headers_are_refused() {
    let db = TempDb::new();
    let loopback = router(state(&db), &[]);
    for host in [
        "127.0.0.1:11898",
        "127.0.0.1",
        "127.1.2.3:11898",
        "localhost:11898",
        "LOCALHOST.",
        "[::1]:11898",
        "[::ffff:127.0.0.1]:11898",
    ] {
        let (status, _, _) = send(&loopback, "GET", "/getheight", &[("host", host)]).await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    let (status, _, _) = send(&loopback, "GET", "/getheight", &[]).await;
    assert_eq!(status, StatusCode::OK, "no Host header");
    for host in [
        "other.example:11898",
        "other.example",
        "127.0.0.1.other.example",
        "localhost.other.example",
        "192.168.1.5:11898",
        "127.0.0.1:port",
        "[::1]junk",
        "",
    ] {
        let (status, _, _) = send(&loopback, "GET", "/getheight", &[("host", host)]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{host:?}");
    }

    // Bound to every interface: LAN IP literals are fine, DNS names still are not, unless
    // explicitly allowed.
    let any = AccessPolicy::new(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        &["node.lan".to_owned()],
        &[],
    );
    let db_lan = TempDb::new();
    let lan = router_with_access(state(&db_lan), any);
    for (host, want) in [
        ("192.168.1.5:11898", StatusCode::OK),
        ("10.0.0.1", StatusCode::OK),
        ("Node.LAN:11898", StatusCode::OK),
        ("other.example:11898", StatusCode::FORBIDDEN),
    ] {
        let (status, _, _) = send(&lan, "GET", "/getheight", &[("host", host)]).await;
        assert_eq!(status, want, "{host}");
    }
    // Bound to one LAN address: that address and loopback only.
    let one = AccessPolicy::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), &[], &[]);
    assert!(one.host_allowed("192.168.1.5:11898"));
    assert!(one.host_allowed("127.0.0.1"));
    assert!(!one.host_allowed("192.168.1.6"));
}

/// A request with a web `Origin` is refused unless `--enable-cors` lists it (or `*`), or
/// it is the node's own origin; `null` needs `--enable-cors null`. Preflights of allowed origins
/// are answered by the CORS layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_origins_need_enable_cors() {
    let db = TempDb::new();
    let plain = router(state(&db), &[]);
    let host = ("host", "127.0.0.1:11898");
    for origin in [
        "https://other.example",
        "null",
        "http://localhost:3000",
        "file://",
    ] {
        let (status, _, _) = send(
            &plain,
            "POST",
            "/sendrawtransaction",
            &[host, ("origin", origin)],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
    }
    // Same origin (`http://` + Host) is fine, so is no Origin at all.
    let (status, _, _) = send(
        &plain,
        "GET",
        "/getheight",
        &[host, ("origin", "http://127.0.0.1:11898")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&plain, "GET", "/getheight", &[host]).await;
    assert_eq!(status, StatusCode::OK);

    let db_null = TempDb::new();
    let cors = router(
        state(&db_null),
        &["https://wallet.example".to_owned(), "null".to_owned()],
    );
    for origin in ["https://wallet.example", "null"] {
        let (status, headers, _) =
            send(&cors, "GET", "/getheight", &[host, ("origin", origin)]).await;
        assert_eq!(status, StatusCode::OK, "{origin}");
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(origin)
        );
        let (status, headers, _) = send(
            &cors,
            "OPTIONS",
            "/getheight",
            &[
                host,
                ("origin", origin),
                ("access-control-request-method", "POST"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "preflight {origin}");
        assert!(headers.contains_key("access-control-allow-methods"));
    }
    let (status, _, _) = send(
        &cors,
        "OPTIONS",
        "/getheight",
        &[host, ("origin", "https://other.example")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "preflight of a foreign origin"
    );

    let db_star = TempDb::new();
    let star = router(state(&db_star), &["*".to_owned()]);
    let (status, headers, _) = send(
        &star,
        "GET",
        "/getheight",
        &[host, ("origin", "https://anything.example")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
    // A rebinding name with an allowed origin is still refused by the Host check.
    let (status, _, _) = send(
        &star,
        "GET",
        "/getheight",
        &[
            ("host", "other.example:11898"),
            ("origin", "http://other.example:11898"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// `/get_transactions_status` takes at most 10 000 hashes and `/get_pool_changes_lite`
/// at most 100 000 known ids (checked before parsing them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_hash_lists_are_capped() {
    let db = TempDb::new();
    let app = app(&db);
    let many: Vec<String> = (0..10_001u32).map(|i| format!("{i:064x}")).collect();
    let (status, body) = post(
        &app,
        "/get_transactions_status",
        json_body(&serde_json::json!({ "transactionHashes": many })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert!(
        v["status"]
            .as_str()
            .is_some_and(|s| s.starts_with("Too many")),
        "{v}"
    );
    let fine: Vec<String> = many[..10_000].to_vec();
    let (status, body) = post(
        &app,
        "/get_transactions_status",
        json_body(&serde_json::json!({ "transactionHashes": fine })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        v["transactionsUnknown"].as_array().map(Vec::len),
        Some(10_000)
    );

    // 10 001 distinct hashes for /gettransactions: refused without a lookup.
    let (status, body) = post(
        &app,
        "/gettransactions",
        json_body(&serde_json::json!({ "txs_hashes": many })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert!(
        v["status"]
            .as_str()
            .is_some_and(|s| s.starts_with("Failed")),
        "{v}"
    );
    assert_eq!(v["txs_as_hex"], serde_json::json!([]));

    let known = vec![String::new(); 100_001];
    let (status, body) = post(
        &app,
        "/get_pool_changes_lite",
        json_body(&serde_json::json!({ "knownTxsIds": known })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert!(
        v["status"]
            .as_str()
            .is_some_and(|s| s.starts_with("Too many")),
        "{v}"
    );
}

/// `/getwalletsyncdata` looks at the first 10 000 checkpoints only: the genesis hash at
/// position 10 000 (0-based 9 999) is found, one position later it is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallet_sync_checkpoints_are_capped() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);
    for (unknown, found) in [(9_999u32, true), (10_000, false)] {
        let mut checkpoints: Vec<String> = (1..=unknown).map(|i| format!("{i:064x}")).collect();
        checkpoints.push(genesis.clone());
        let (status, body) = post(
            &app,
            "/getwalletsyncdata",
            json_body(&serde_json::json!({ "blockHashCheckpoints": checkpoints })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_slice(&body).expect("json");
        if found {
            assert_eq!(v["status"], "OK", "{v}");
        } else {
            assert_eq!(v["status"], "Failed to perform query", "{v}");
        }
    }
}

/// `/get_global_indexes_for_range` walks the range in chunks with one decoded block at
/// a time; a range far above the top answers what exists (the genesis miner transaction).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_indexes_for_a_long_range() {
    let db = TempDb::new();
    let app = app(&db);
    let blob = hex::decode(xkr_consensus::params::GENESIS_COINBASE_TX_HEX).expect("hex");
    let hash = xkr_serialization::transaction_hash_from_blob(&blob).to_string();
    let (status, body) = post(
        &app,
        "/get_global_indexes_for_range",
        json_body(&serde_json::json!({ "startHeight": 0, "endHeight": 1_000_000 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(
        v["indexes"],
        serde_json::json!([{ "key": hash, "value": [0] }]),
        "{v}"
    );
    // f_on_transactions_pool_json on an empty pool
    let (status, body) = post(
        &app,
        "/json_rpc",
        br#"{"jsonrpc":"2.0","id":1,"method":"f_on_transactions_pool_json","params":{}}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["result"]["transactions"], serde_json::json!([]), "{v}");
}

/// `/get_transaction_details_by_hashes` and `/get_blocks_details_by_*` answer one entry per
/// requested hash or height, in request order (the C++ shape), with a repeated item written at
/// each of its positions, and the whole answer stays within the response size limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detail_routes_answer_each_item_once() {
    let db = TempDb::new();
    let st = state(&db);
    let genesis = st.chain.genesis_hash().to_string();
    let app = router(st, &[]);
    let blob = hex::decode(xkr_consensus::params::GENESIS_COINBASE_TX_HEX).expect("hex");
    let tx = xkr_serialization::transaction_hash_from_blob(&blob).to_string();

    let (status, body) = post(
        &app,
        "/get_transaction_details_by_hashes",
        json_body(&serde_json::json!({ "transactionHashes": vec![tx.clone(); 1000] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.len() <= xkr_rpc::MAX_DETAILS_ANSWER_BYTES,
        "{}",
        body.len()
    );
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "OK", "{v}");
    let txs = v["transactions"].as_array().expect("list");
    assert_eq!(txs.len(), 1000);
    assert!(txs.iter().all(|t| t["hash"] == tx.as_str()));
    assert!(txs.iter().all(|t| t == &txs[0]));

    let (status, body) = post(
        &app,
        "/get_blocks_details_by_heights",
        json_body(&serde_json::json!({ "blockHeights": vec![0; 100] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.len() <= xkr_rpc::MAX_DETAILS_ANSWER_BYTES,
        "{}",
        body.len()
    );
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "OK", "{v}");
    let blocks = v["blocks"].as_array().expect("list");
    assert_eq!(blocks.len(), 100);
    assert!(blocks.iter().all(|b| b["hash"] == genesis.as_str()));
    assert!(blocks.iter().all(|b| b == &blocks[0]));

    let (status, body) = post(
        &app,
        "/get_blocks_details_by_hashes",
        json_body(&serde_json::json!({ "blockHashes": vec![genesis.clone(); 100] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.len() <= xkr_rpc::MAX_DETAILS_ANSWER_BYTES,
        "{}",
        body.len()
    );
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["status"], "OK", "{v}");
    let blocks = v["blocks"].as_array().expect("list");
    assert_eq!(blocks.len(), 100);
    assert!(blocks.iter().all(|b| b["hash"] == genesis.as_str()));

    // a mixed list keeps the request order
    let (_, body) = post(
        &app,
        "/get_blocks_details_by_heights",
        json_body(&serde_json::json!({ "blockHeights": [0, 0, 0] })),
    )
    .await;
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["blocks"].as_array().map(Vec::len), Some(3), "{v}");
    assert_eq!(v["blocks"][2]["index"], 0);

    // over the list cap: an error status and an empty list
    let (_, body) = post(
        &app,
        "/get_blocks_details_by_heights",
        json_body(&serde_json::json!({ "blockHeights": vec![0; 101] })),
    )
    .await;
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert!(
        v["status"].as_str().is_some_and(|s| s.starts_with("Error")),
        "{v}"
    );
    assert_eq!(v["blocks"], serde_json::json!([]));
}

/// A request counts against the concurrency limit until its response body has been read: with
/// every permit held by an unread response the next request gets 503, and reading one frees a
/// permit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_responses_hold_the_request_permit() {
    let db = TempDb::new();
    let app = app(&db);
    let get = || {
        Request::builder()
            .method("GET")
            .uri("/getheight")
            .body(Body::empty())
            .expect("request")
    };
    let mut held = Vec::new();
    for _ in 0..xkr_rpc::MAX_CONCURRENT_REQUESTS {
        let resp = app.clone().oneshot(get()).await.expect("infallible");
        assert_eq!(resp.status(), StatusCode::OK);
        held.push(resp);
    }
    let resp = app.clone().oneshot(get()).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let first = held.pop().expect("one");
    let body = axum::body::to_bytes(first.into_body(), 1 << 20)
        .await
        .expect("body");
    let v: Value = serde_json::from_slice(&body).expect("json");
    assert!(v["height"].is_number(), "{v}");
    let resp = app.clone().oneshot(get()).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("content-length"));
    drop(held);
    let resp = app.clone().oneshot(get()).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Browser requests a web page makes without CORS (`<img>`, `<script>`, links) carry
/// `Sec-Fetch-Site: cross-site` or `same-site` and no `Origin`: refused unless an allowed
/// `Origin` comes with them. Address-bar requests (`none`), same-origin requests and clients
/// without fetch metadata are served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_from_other_sites_need_an_allowed_origin() {
    let db = TempDb::new();
    let plain = router(state(&db), &[]);
    let host = ("host", "127.0.0.1:11898");
    for site in ["cross-site", "same-site"] {
        for path in ["/get_pool", "/get_pool_changes", "/getheight"] {
            let (status, _, _) = send(
                &plain,
                "GET",
                path,
                &[
                    host,
                    ("sec-fetch-site", site),
                    ("sec-fetch-mode", "no-cors"),
                    ("sec-fetch-dest", "image"),
                ],
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{site} {path}");
        }
    }
    for site in ["none", "same-origin"] {
        let (status, _, _) = send(
            &plain,
            "GET",
            "/get_pool",
            &[host, ("sec-fetch-site", site)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{site}");
    }
    let (status, _, _) = send(&plain, "GET", "/get_pool", &[host]).await;
    assert_eq!(status, StatusCode::OK);

    let db_cors = TempDb::new();
    let cors = router(state(&db_cors), &["https://wallet.example".to_owned()]);
    let (status, _, _) = send(
        &cors,
        "GET",
        "/get_pool",
        &[
            host,
            ("sec-fetch-site", "cross-site"),
            ("origin", "https://wallet.example"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        &cors,
        "GET",
        "/get_pool",
        &[host, ("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
