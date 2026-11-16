//! RPC answers on a really pruned database: a testnet chain built in process (fixed difficulty, a
//! valid merge-mining tag, fixed timestamps, as in `tx_v2_walletsync/node.rs`) with a spend in
//! block 3 and one in block 68, pruned completely below 64 by the storage call behind
//! `xkrd db prune --all` (testnet's prune bound is 0, so `Chain::prune` never prunes there).
//! Requests go straight into the router, no socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{AddBlockOutcome, Chain, ChainConfig};
use xkr_consensus::Network;
use xkr_consensus::reward::get_block_reward;
use xkr_crypto::{generate_key_image, generate_keys, generate_ring_signature, os_rng};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState, router};
use xkr_serialization::{
    aux_block_header_hash, block_hash, encode_block, encode_transaction, transaction_hash,
    transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_storage::{PruneMode, PruningSeed};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_types::{
    Block, BlockHeader, Hash, ParentBlock, PublicKey, RawBlock, SecretKey, Transaction,
    TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

/// Timestamp of block 0 + 30 s per block.
const BASE_TIMESTAMP: u64 = 1_750_000_000;
/// Fee of the two spends.
const FEE: u64 = 1_000;
/// The C++-shaped details status and the JSON-RPC message for the pruned block 3.
const PRUNED_STATUS: &str =
    "Error: block 3 is pruned on this node (its ring signatures were dropped).";
const PRUNED_MESSAGE: &str = "block 3 is pruned on this node (its ring signatures were dropped)";

/// A database file under the temp dir, removed on drop.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-rpc-pruned-{}-{}.redb",
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

/// A testnet node over `db` (genesis only).
fn testnet_state(db: &TempDb) -> Arc<RpcState> {
    let store = xkr_storage::Store::open(&db.0).expect("store");
    let pool = Arc::new(TxPool::new(PoolConfig::default()));
    let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).expect("chain");
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
        info: NodeInfo {
            version: "rpc-pruned-test".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: String::new(),
            fee_amount: 0,
        },
    })
}

/// Adds testnet blocks (major 5, fixed difficulty 1, a valid merge-mining tag).
struct Miner {
    tip: Hash,
    height: u64,
    coins: u64,
}

impl Miner {
    fn new(chain: &Chain) -> Self {
        let tip = chain.tip();
        Self {
            tip: tip.hash,
            height: tip.height,
            coins: tip.already_generated_coins,
        }
    }

    /// Adds the next block with `txs`, paying `miner` the base reward plus `fee`; returns its
    /// hash and miner transaction.
    fn mine(
        &mut self,
        chain: &Chain,
        miner: PublicKey,
        txs: &[Vec<u8>],
        fee: u64,
    ) -> (Hash, Transaction) {
        let height = self.height + 1;
        let base = get_block_reward(5, 0, 100, self.coins, 0)
            .expect("reward")
            .0;
        let miner_tx = Transaction {
            prefix: TransactionPrefix {
                version: 1,
                unlock_time: height + 1,
                inputs: vec![TxInput::Coinbase {
                    height: height as u32,
                }],
                outputs: vec![TxOutput {
                    amount: base + fee,
                    target: TxOutputTarget::Key(miner),
                }],
                extra: Vec::new(),
            },
            signatures: Vec::new(),
            clsag_signatures: Vec::new(),
        };
        let mut parent_tx = Transaction::default();
        parent_tx.prefix.version = 1;
        let mut block = Block {
            header: BlockHeader {
                major_version: 5,
                minor_version: 0,
                timestamp: BASE_TIMESTAMP + height * 30,
                prev_hash: self.tip,
                nonce: 0,
            },
            parent_block: Some(ParentBlock {
                major_version: 1,
                minor_version: 0,
                prev_hash: Hash::ZERO,
                transaction_count: 1,
                base_transaction_branch: Vec::new(),
                base_transaction: parent_tx,
                blockchain_branch: Vec::new(),
            }),
            miner_tx: miner_tx.clone(),
            tx_hashes: txs
                .iter()
                .map(|t| transaction_hash_from_blob(t.as_slice()))
                .collect(),
        };
        let aux = aux_block_header_hash(&block).expect("aux hash");
        let mut mm_tag = vec![0x03, 33, 0x00];
        mm_tag.extend_from_slice(&aux.0);
        if let Some(parent) = block.parent_block.as_mut() {
            parent.base_transaction.prefix.extra = mm_tag;
        }
        let hash = block_hash(&block).expect("block hash");
        let raw = RawBlock {
            block: encode_block(&block).expect("block encodes"),
            transactions: txs.to_vec(),
        };
        let added = chain.add_block(raw).expect("add_block");
        assert_eq!(
            added.outcome,
            AddBlockOutcome::AddedToMain,
            "block {height}"
        );
        self.tip = hash;
        self.height = height;
        self.coins += base;
        (hash, miner_tx)
    }
}

fn random_key() -> PublicKey {
    generate_keys(&mut os_rng()).0
}

/// A version-1 transaction spending output `(amount, index)` owned by `(public, secret)` to a
/// fresh key, paying [`FEE`].
fn spend(public: PublicKey, secret: &SecretKey, amount: u64, index: u32) -> Vec<u8> {
    let mut rng = os_rng();
    let key_image = generate_key_image(&public, secret);
    let prefix = TransactionPrefix {
        version: 1,
        unlock_time: 0,
        inputs: vec![TxInput::Key {
            amount,
            output_indexes: vec![index],
            key_image,
        }],
        outputs: vec![TxOutput {
            amount: amount - FEE,
            target: TxOutputTarget::Key(random_key()),
        }],
        extra: Vec::new(),
    };
    let signatures = generate_ring_signature(
        &transaction_prefix_hash(&prefix),
        &key_image,
        &[public],
        secret,
        0,
        &mut rng,
    )
    .expect("ring signature");
    encode_transaction(&Transaction {
        prefix,
        signatures: vec![signatures],
        clsag_signatures: Vec::new(),
    })
    .expect("transaction encodes")
}

/// Global index of the miner output of the block at `height`.
fn coinbase_index(chain: &Chain, height: u64) -> u32 {
    let reader = chain.reader().expect("reader");
    chain
        .with_compressor(|c| reader.tx_global_indexes(height, 0, c))
        .expect("compressor")
        .expect("indexes")
        .expect("block")[0]
}

/// POSTs `body` to `path`; the status and the JSON answer.
async fn post_json(app: &Router, path: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).expect("json")))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("json answer"))
}

/// A JSON-RPC call; the whole answer (`result` or `error`).
async fn rpc(app: &Router, method: &str, params: Value) -> Value {
    let request = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let (status, v) = post_json(app, "/json_rpc", &request).await;
    assert_eq!(status, StatusCode::OK, "{method}");
    v
}

/// On a database whose block 3 lost its signatures, `/gettransactions` puts only the pruned
/// transaction (and an unknown hash) in `missed_tx` and answers the rest (the miner transaction of
/// block 3, a transaction above the prune height); the details routes answer the C++-shaped "Error:
/// block 3 is pruned ..." status for a request that needs block 3's signatures and answer pruned
/// blocks without transactions; the explorer methods return error -1 for it; wallet sync is
/// unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruned_blocks_through_the_routes() {
    let db = TempDb::new();
    let st = testnet_state(&db);
    let chain = &st.chain;
    // Chunks are sealed 5 blocks behind the tip, so block 3 is in the sealed chunk 0 when it is
    // pruned (the usual case on a real node).
    chain.store().set_seal_lag(5).expect("seal lag");
    let mut miner = Miner::new(chain);
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());
    let (late_pub, late_sec) = generate_keys(&mut os_rng());

    // Block 1 pays `owner`, block 3 spends it; block 66 pays `late`, block 68 spends it.
    let mut block_hashes = vec![chain.genesis_hash()];
    let mut coinbases = vec![Transaction::default()];
    let mut spends = Vec::new();
    for height in 1..=70_u64 {
        let paid_from = match height {
            3 => Some((1_usize, owner_pub, &owner_sec)),
            68 => Some((66, late_pub, &late_sec)),
            _ => None,
        };
        let txs: Vec<Vec<u8>> = paid_from.map_or_else(Vec::new, |(from, public, secret)| {
            let amount = coinbases[from].prefix.outputs[0].amount;
            vec![spend(
                public,
                secret,
                amount,
                coinbase_index(chain, from as u64),
            )]
        });
        let payee = match height {
            1 => owner_pub,
            66 => late_pub,
            _ => random_key(),
        };
        let fee = if txs.is_empty() { 0 } else { FEE };
        let (hash, coinbase) = miner.mine(chain, payee, &txs, fee);
        spends.extend(txs);
        block_hashes.push(hash);
        coinbases.push(coinbase);
    }
    let early_blob = &spends[0];
    let late_blob = &spends[1];
    let early = transaction_hash_from_blob(early_blob).to_string();
    let late = transaction_hash_from_blob(late_blob).to_string();
    let miner_tx3 = transaction_hash(&coinbases[3]).expect("hash").to_string();
    let miner_tx3_hex = hex::encode(encode_transaction(&coinbases[3]).expect("encodes"));
    let unknown = Hash([0x42; 32]).to_string();
    let block3 = block_hashes[3].to_string();
    let app = router(st.clone(), &[]);

    // Before pruning, block 3's transaction is served whole.
    let (status, v) = post_json(&app, "/gettransactions", &json!({ "txs_hashes": [early] })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["txs_as_hex"], json!([hex::encode(early_blob)]), "{v}");

    // Prune everything below 64: chunk 0 (sealed) loses its signature parts.
    let report = chain
        .store()
        .prune(PruneMode::All, 64, &mut |_| {})
        .expect("prune");
    assert_eq!(report.state.pruned_below, 64);
    assert_eq!(report.state.seed, PruningSeed::RECENT_ONLY);
    assert_eq!((report.chunks, report.transactions), (1, 1));
    {
        let reader = chain.reader().expect("reader");
        assert!(reader.sealed_below() >= 64);
        assert!(!reader.has_full_block(3) && !reader.has_full_block(2));
        assert!(reader.has_full_block(64) && reader.has_full_block(68));
    }

    // `/gettransactions`: the pruned one and the unknown one are missed, in request order; the
    // miner transaction of block 3 and the spend in block 68 are answered.
    let (status, v) = post_json(
        &app,
        "/gettransactions",
        &json!({ "txs_hashes": [early, miner_tx3, late, unknown] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(
        v["txs_as_hex"],
        json!([miner_tx3_hex, hex::encode(late_blob)]),
        "{v}"
    );
    assert_eq!(v["missed_tx"], json!([early, unknown]), "{v}");

    // Block details: block 3 needs its signatures, block 2 (pruned, no transactions) and block
    // 68 do not.
    let details = |height: u64| json!({ "blockHeight": height });
    let (status, v) = post_json(&app, "/get_block_details_by_height", &details(3)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], PRUNED_STATUS, "{v}");
    assert_eq!(v["block"], Value::Null);
    for (height, transactions) in [(2, 1), (68, 2)] {
        let (status, v) = post_json(&app, "/get_block_details_by_height", &details(height)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["status"], "OK", "block {height}: {v}");
        assert_eq!(
            v["block"]["transactions"].as_array().map(Vec::len),
            Some(transactions),
            "block {height}"
        );
    }
    let (status, v) = post_json(
        &app,
        "/get_blocks_details_by_heights",
        &json!({ "blockHeights": [68, 3] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], PRUNED_STATUS, "{v}");
    assert_eq!(v["blocks"], json!([]));
    let (status, v) = post_json(
        &app,
        "/get_blocks_details_by_hashes",
        &json!({ "blockHashes": [block3] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], PRUNED_STATUS, "{v}");
    assert_eq!(v["blocks"], json!([]));

    // Transaction details: the pruned spend fails the request, the others are answered.
    let (status, v) = post_json(
        &app,
        "/get_transaction_details_by_hashes",
        &json!({ "transactionHashes": [late, early] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], PRUNED_STATUS, "{v}");
    assert_eq!(v["transactions"], json!([]));
    let (status, v) = post_json(
        &app,
        "/get_transaction_details_by_hashes",
        &json!({ "transactionHashes": [late, miner_tx3] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["transactions"].as_array().map(Vec::len), Some(2));

    // Explorer JSON-RPC: error -1 for block 3 (by hash and by height) and its spend.
    for params in [json!({ "hash": block3 }), json!({ "hash": "3" })] {
        let v = rpc(&app, "f_block_json", params).await;
        assert_eq!(v["error"]["code"], -1, "{v}");
        assert_eq!(v["error"]["message"], PRUNED_MESSAGE, "{v}");
        assert!(v.get("result").is_none(), "{v}");
    }
    let v = rpc(&app, "f_transaction_json", json!({ "hash": early })).await;
    assert_eq!(v["error"]["code"], -1, "{v}");
    assert_eq!(v["error"]["message"], PRUNED_MESSAGE, "{v}");
    let v = rpc(&app, "f_block_json", json!({ "hash": "2" })).await;
    assert_eq!(v["result"]["status"], "OK", "{v}");
    let v = rpc(&app, "f_transaction_json", json!({ "hash": late })).await;
    assert_eq!(v["result"]["status"], "OK", "{v}");

    // Wallet sync reads prefixes only: block 3 and its spend are still there.
    let (status, v) = post_json(
        &app,
        "/getwalletsyncdata",
        &json!({ "startHeight": 3, "blockCount": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "OK", "{v}");
    assert_eq!(v["items"][0]["blockHeight"], 3, "{v}");
    assert_eq!(v["items"][0]["transactions"][0]["hash"], early, "{v}");
}
