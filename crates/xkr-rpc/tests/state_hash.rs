//! JSON-RPC `getstatehash` and `/status.state` on a testnet chain with one stable span: the
//! genesis block plus 5 096 blocks with one miner output each, written straight into the store
//! (they never pass consensus; storage and the state hashes only read them), so span 0 is
//! `max_reorg_depth` (1 000) below the tip. Requests go straight into the router, no socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{Chain, ChainConfig, StateHashes};
use xkr_consensus::Network;
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState, router};
use xkr_serialization::encode_block;
use xkr_storage::{STRIPE_SPAN, Store, VerifiedBlock, ops};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_types::{
    Block, BlockHeader, Hash, PublicKey, Transaction, TransactionPrefix, TxInput, TxOutput,
    TxOutputTarget,
};

/// A database file under the temp dir, removed on drop.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-rpc-state-{}-{}.redb",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Top block: span 0 is stable, span 1 is not.
const TIP: u64 = STRIPE_SPAN + 1_000;

/// A block for `height` with only a miner transaction, consistent for storage.
fn stored_block(height: u64) -> VerifiedBlock {
    let bytes = |tag: u8| {
        let mut b = [tag; 32];
        b[..8].copy_from_slice(&height.to_le_bytes());
        b
    };
    let miner_tx = Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time: height + 1,
            inputs: vec![TxInput::Coinbase {
                height: height as u32,
            }],
            outputs: vec![TxOutput {
                amount: 100,
                target: TxOutputTarget::Key(PublicKey(bytes(1))),
            }],
            extra: vec![],
        },
        signatures: vec![],
        clsag_signatures: vec![],
    };
    let block = Block {
        header: BlockHeader {
            major_version: 1,
            minor_version: 0,
            timestamp: 1_600_000_000 + height * 30,
            prev_hash: Hash(bytes(6)),
            nonce: 0,
        },
        parent_block: None,
        miner_tx,
        tx_hashes: vec![],
    };
    VerifiedBlock {
        height,
        hash: Hash(bytes(7)),
        block_blob: encode_block(&block).expect("block encodes"),
        block,
        miner_tx_hash: Hash(bytes(8)),
        difficulty: 1,
        already_generated_coins: 1_000_000 * (height + 1),
        block_size: 200,
        txs: vec![],
    }
}

/// The testnet database: genesis, then [`stored_block`]s up to [`TIP`].
fn build(db: &TempDb) {
    let pool = Arc::new(TxPool::new(PoolConfig::default()));
    let store = Store::open(&db.0).expect("store");
    Chain::open(store, ChainConfig::new(Network::Testnet), pool)
        .expect("chain")
        .close()
        .expect("close");
    let store = Store::open(&db.0).expect("store");
    let mut comp = store.compressor().expect("compressor");
    let heights: Vec<u64> = (1..=TIP).collect();
    for batch in heights.chunks(2_048) {
        let blocks: Vec<VerifiedBlock> = batch.iter().map(|h| stored_block(*h)).collect();
        let txn = store.begin_write().expect("write transaction");
        ops::push_blocks(&txn, &blocks, &mut comp).expect("push");
        txn.commit().expect("commit");
    }
    store.close().expect("close");
}

/// The node state over `db` with `builtin` as the compiled-in state hashes.
fn node(db: &TempDb, builtin: StateHashes) -> Arc<RpcState> {
    let store = Store::open(&db.0).expect("store");
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.state_hashes = builtin;
    let pool = Arc::new(TxPool::new(PoolConfig::default()));
    let chain = Chain::open(store, cfg, pool).expect("chain");
    let (_net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
    let (_mining_tx, mining) = watch::channel(Arc::new(MiningStatus::default()));
    let (commands, _commands_rx) = mpsc::unbounded_channel();
    Arc::new(RpcState {
        chain: Arc::new(chain),
        net,
        commands,
        mining,
        hugin: None,
        p2pool: None,
        explorer: false,
        info: NodeInfo {
            version: "rpc-state-test".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: String::new(),
            fee_amount: 0,
        },
    })
}

async fn post_json(app: &Router, path: &str, body: &Value) -> Value {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("json")))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
}

async fn state_hash(app: &Router, params: Value) -> Value {
    post_json(
        app,
        "/json_rpc",
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "getstatehash", "params": params }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn getstatehash_and_status_state() {
    let db = TempDb::new();
    build(&db);

    // No built-in list: the hash is served, `builtin_match` is null.
    let s = node(&db, StateHashes::default());
    let app = router(s.clone(), &[]);
    let status = post_json(&app, "/status", &json!({})).await;
    assert_eq!(
        status["state"],
        json!({ "span": null, "hash": null, "mismatch": null })
    );
    assert_eq!(
        state_hash(&app, json!({ "span": 0 })).await["error"]["code"],
        -2
    );

    let update = s.chain.update_state_hashes(8).expect("update");
    assert_eq!((update.computed, update.pending), (1, 0));
    let s0 = s.chain.state_view().get(0).expect("span 0");
    let v = state_hash(&app, json!({ "span": 0 })).await;
    assert_eq!(
        v["result"],
        json!({
            "span": 0,
            "height_end": STRIPE_SPAN,
            "hash": s0.to_string(),
            "builtin_match": null,
            "status": "OK",
        }),
        "{v}"
    );
    let by_height = state_hash(&app, json!({ "height": STRIPE_SPAN - 1 })).await;
    assert_eq!(by_height["result"], v["result"]);
    let unstable = state_hash(&app, json!({ "height": STRIPE_SPAN })).await;
    assert_eq!(unstable["error"]["code"], -2, "{unstable}");
    let no_params = state_hash(&app, json!({})).await;
    assert_eq!(no_params["error"]["code"], -1, "{no_params}");
    let status = post_json(&app, "/status", &json!({})).await;
    assert_eq!(
        status["state"],
        json!({ "span": 0, "hash": s0.to_string(), "mismatch": null })
    );
    s.chain.close().expect("close");
    drop((app, s));

    // The built-in list has span 0 with this hash.
    let s = node(&db, StateHashes::from_list([(0, s0)]));
    let app = router(s.clone(), &[]);
    let v = state_hash(&app, json!({ "span": 0 })).await;
    assert_eq!(v["result"]["builtin_match"], json!(true), "{v}");
    assert_eq!(
        post_json(&app, "/status", &json!({})).await["state"]["mismatch"],
        Value::Null
    );
    s.chain.close().expect("close");
    drop((app, s));

    // ... with another hash: `builtin_match` false and `/status.state.mismatch` names it.
    let wrong = Hash([0x5a; 32]);
    let s = node(&db, StateHashes::from_list([(0, wrong)]));
    let app = router(s.clone(), &[]);
    let v = state_hash(&app, json!({ "span": 0 })).await;
    assert_eq!(v["result"]["builtin_match"], json!(false), "{v}");
    let status = post_json(&app, "/status", &json!({})).await;
    assert_eq!(
        status["state"]["mismatch"],
        json!({ "span": 0, "expected": wrong.to_string(), "found": s0.to_string() })
    );
    s.chain.close().expect("close");
}
