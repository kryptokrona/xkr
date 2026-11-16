//! The HTTP API (`xkr-rpc`) end to end: a fuzzer-chosen path and body go through the axum
//! router (body limit, JSON parsing, every handler) against a genesis-only chain. Never panics
//! and never answers 5xx (a node failure on bad input is a bug).
#![no_main]

use std::sync::{Arc, OnceLock};

use arbitrary::Unstructured;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use libfuzzer_sys::fuzz_target;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig};
use xkr_consensus::Network;
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState, router};
use xkr_txpool::{PoolConfig, TxPool};

const PATHS: [&str; 22] = [
    "/json_rpc",
    "/getwalletsyncdata",
    "/get_global_indexes_for_range",
    "/get_transactions_status",
    "/get_o_indexes",
    "/gettransactions",
    "/sendrawtransaction",
    "/getrandom_outs",
    "/get_pool_changes_lite",
    "/get_pool_changes",
    "/get_pool",
    "/get_block_details_by_height",
    "/get_blocks_details_by_heights",
    "/get_blocks_details_by_hashes",
    "/get_transaction_details_by_hashes",
    "/get_transaction_hashes_by_payment_id",
    "/getinfo",
    "/getheight",
    "/fee",
    "/peers",
    "/status",
    "/nonexistent",
];

const METHODS: [&str; 15] = [
    "getblockcount",
    "on_getblockhash",
    "getlastblockheader",
    "getblockheaderbyhash",
    "getblockheaderbyheight",
    "get_info",
    "getcurrencyid",
    "getblocktemplate",
    "submitblock",
    "f_blocks_list_json",
    "f_block_json",
    "f_transaction_json",
    "f_on_transactions_pool_json",
    "get_miner_data",
    "get_block_headers_range",
];

struct Env {
    rt: Runtime,
    app: Router,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let db = std::env::temp_dir().join(format!("xkr-fuzz-rpc-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let store = xkr_storage::Store::open(&db).expect("store");
        let chain = Chain::open(
            store,
            ChainConfig::new(Network::Mainnet),
            Arc::new(TxPool::new(PoolConfig::default())),
        )
        .expect("chain");
        let (net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
        // keep the senders alive for the process lifetime
        std::mem::forget(net_tx);
        std::mem::forget(mining_tx);
        let (commands, commands_rx) = mpsc::unbounded_channel();
        // dropping the receiver makes submitblock fall back to adding the block directly
        drop(commands_rx);
        let state = Arc::new(RpcState {
            chain: Arc::new(chain),
            net,
            commands,
            mining,
            hugin: None,
            info: NodeInfo {
                version: "fuzz".into(),
                start_time: 0,
                data_dir: std::env::temp_dir(),
                db_path: db,
                fee_address: String::new(),
                fee_amount: 0,
            },
        });
        Env {
            rt,
            app: router(state, vec!["*".into()]),
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(kind), Ok(pick), Ok(get)) = (u.arbitrary::<u8>(), u.arbitrary::<u8>(), u.arbitrary::<bool>())
    else {
        return;
    };
    let rest = u.take_rest();
    let (path, method, body) = if kind & 1 == 0 {
        // a raw body to any path
        (
            PATHS[usize::from(pick) % PATHS.len()],
            if get { "GET" } else { "POST" },
            rest.to_vec(),
        )
    } else {
        // a JSON-RPC envelope around a known method and fuzzer-chosen params text
        let params = String::from_utf8_lossy(rest);
        (
            "/json_rpc",
            "POST",
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"{}","params":{params}}}"#,
                METHODS[usize::from(pick) % METHODS.len()]
            )
            .into_bytes(),
        )
    };
    let _ = serde_json::from_slice::<serde_json::Value>(&body);
    if let Ok(text) = std::str::from_utf8(&body) {
        let _ = xkr_rpc::parse_miner_address(text);
    }
    let env = env();
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("origin", "http://fuzz")
        .body(Body::from(body))
        .expect("request");
    let status = env.rt.block_on(async {
        let resp = env.app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let _ = axum::body::to_bytes(resp.into_body(), 64 << 20).await;
        status
    });
    assert!(!status.is_server_error(), "{method} {path}: {status}");
});
