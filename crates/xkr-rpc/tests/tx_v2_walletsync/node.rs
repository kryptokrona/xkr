//! The in-process fork-14 testnet node of vector E: database, chain configuration, router
//! calls, a block builder (a port of the `xkr-chain` test builder: fixed testnet difficulty, a
//! valid merge-mining tag, fixed timestamps) and deterministic transfers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tower::ServiceExt;
use xkr_chain::{AddBlockOutcome, Chain, ChainConfig};
use xkr_consensus::reward::get_block_reward;
use xkr_consensus::{Fork14Params, Network, RingSizeRule};
use xkr_crypto::{derive_public_key, generate_key_derivation, view_tag};
use xkr_rpc::{MiningStatus, NetStatus, NodeInfo, RpcState};
use xkr_serialization::{aux_block_header_hash, block_hash, encode_block, transaction_hash};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_types::{
    Block, BlockHeader, Hash, ParentBlock, PublicKey, RawBlock, Transaction, TransactionPrefix,
    TxInput, TxOutput, TxOutputTarget,
};
use xkr_wallet_core::{
    Address, BuiltTransaction, Destination, FoundOutput, InputSource, TransferOptions, TxVersion,
    WalletKeys, build_transaction, scan_transaction,
};

use crate::vectors::{ScriptedRng, public, scalar, secret};

/// Fork-13 height of the chain (blocks above it are major 6).
pub const FORK13: u32 = 2;
/// Fork-14 height `F` (blocks above it are major 7 with version-2 coinbases).
pub const F: u32 = 4;
/// Fork-14 grace: blocks `F + 1 ..= F + GRACE` still accept version 1.
pub const GRACE: u64 = 3;
/// Timestamp of block 0 + 30 s per block (fixed so block hashes are reproducible).
pub const BASE_TIMESTAMP: u64 = 1_750_000_000;

/// One recorded RPC call.
#[derive(Debug, Serialize, Deserialize)]
pub struct Exchange {
    pub path: String,
    pub request: Value,
    pub response: Value,
}

/// A temporary database file, removed on drop.
pub struct TempDb(PathBuf);

impl TempDb {
    /// A fresh path under the temp dir.
    pub fn create() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = format!("xkr-walletsync-vector-{}-{n}.redb", std::process::id());
        Self(std::env::temp_dir().join(name))
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The fork-14 testnet node behind the router.
pub fn node(db: &TempDb) -> Arc<RpcState> {
    let mut cfg = ChainConfig::new(Network::Testnet);
    cfg.fork13_height = Some(FORK13);
    cfg.fork14_height = Some(F);
    cfg.fork14 = Fork14Params {
        grace_blocks: GRACE,
        v2_ring_size: RingSizeRule::MixinLimits,
    };
    let store = xkr_storage::Store::open(&db.0).expect("store");
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
        clients: None,
        explorer: true,
        info: NodeInfo {
            version: "xkr-vectors".into(),
            start_time: 0,
            data_dir: std::env::temp_dir(),
            db_path: db.0.clone(),
            fee_address: String::new(),
            fee_amount: 0,
        },
    })
}

/// POSTs `request` to `path` and records the exchange.
pub async fn exchange(app: &Router, path: &str, request: Value) -> Exchange {
    let body = Body::from(serde_json::to_vec(&request).expect("json"));
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK, "{path}");
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .expect("body");
    Exchange {
        path: path.to_string(),
        request,
        response: serde_json::from_slice(&bytes).expect("json answer"),
    }
}

/// Global output indexes per transaction hash.
pub type Indexes = HashMap<String, Vec<u64>>;

/// [`Indexes`] from a `/get_global_indexes_for_range` answer.
pub fn index_map(answer: &Value) -> Indexes {
    let rows = answer["indexes"].as_array().expect("indexes");
    rows.iter()
        .map(|r| {
            let hash = r["key"].as_str().expect("key").to_string();
            let values = r["value"].as_array().expect("value");
            (
                hash,
                values.iter().map(|v| v.as_u64().expect("index")).collect(),
            )
        })
        .collect()
}

/// Builds the chain block by block (port of the `xkr-chain` test builder).
pub struct Miner {
    state: Arc<RpcState>,
    /// Top block id.
    pub tip: Hash,
    height: u64,
    coins: u64,
    /// `(height, transaction)` in sync order: each coinbase, then the block's transactions.
    pub mined: Vec<(u64, Transaction)>,
    /// Coinbase transactions by height.
    pub coinbases: HashMap<u64, Transaction>,
}

impl Miner {
    pub fn new(state: Arc<RpcState>) -> Self {
        let tip = state.chain.tip();
        Self {
            tip: tip.hash,
            height: tip.height,
            coins: tip.already_generated_coins,
            state,
            mined: Vec::new(),
            coinbases: HashMap::new(),
        }
    }

    /// Adds the next block, paying `miner` the base reward plus the fees of `txs`.
    pub fn mine(&mut self, miner: &Address, txs: &[&BuiltTransaction]) {
        let height = self.height + 1;
        let base = get_block_reward(5, 0, 100, self.coins, 0)
            .expect("reward")
            .0;
        let fees: u64 = txs.iter().map(|t| t.fee).sum();
        let v2 = height > u64::from(F);
        let r = secret(&format!("walletsync coinbase {height}"));
        let d = generate_key_derivation(&miner.view, &r).expect("miner view key");
        let key = derive_public_key(&d, 0, &miner.spend).expect("miner spend key");
        let target = if v2 {
            TxOutputTarget::TaggedKey {
                key,
                view_tag: view_tag(&d, 0),
            }
        } else {
            TxOutputTarget::Key(key)
        };
        let mut extra = vec![0x01];
        extra.extend_from_slice(&public(&r).0);
        let miner_tx = Transaction {
            prefix: TransactionPrefix {
                version: if v2 { 2 } else { 1 },
                unlock_time: height + 1,
                inputs: vec![TxInput::Coinbase {
                    height: height as u32,
                }],
                outputs: vec![TxOutput {
                    amount: base + fees,
                    target,
                }],
                extra,
            },
            signatures: Vec::new(),
            clsag_signatures: Vec::new(),
        };
        let major_version = if v2 {
            7
        } else if height > u64::from(FORK13) {
            6
        } else {
            5
        };
        let mut parent_tx = Transaction::default();
        parent_tx.prefix.version = 1;
        let mut block = Block {
            header: BlockHeader {
                major_version,
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
            tx_hashes: txs.iter().map(|t| t.hash).collect(),
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
            transactions: txs.iter().map(|t| t.blob.clone()).collect(),
        };
        let added = self.state.chain.add_block(raw).expect("add_block");
        assert_eq!(
            added.outcome,
            AddBlockOutcome::AddedToMain,
            "block {height}"
        );
        self.mined.push((height, miner_tx.clone()));
        for t in txs {
            self.mined.push((height, t.tx.clone()));
        }
        self.coinbases.insert(height, miner_tx);
        self.tip = hash;
        self.height = height;
        self.coins += base;
    }
}

/// The one output `keys` owns in `tx` (mined at `height`) and its global index.
pub async fn input(
    app: &Router,
    keys: &WalletKeys,
    tx: &Transaction,
    height: u64,
) -> (FoundOutput, u32) {
    let mut found = scan_transaction(keys, &tx.prefix);
    assert_eq!(found.len(), 1, "one output at height {height}");
    let f = found.remove(0);
    let request = json!({ "startHeight": height, "endHeight": height + 1 });
    let answer = exchange(app, "/get_global_indexes_for_range", request).await;
    let hash = transaction_hash(tx).expect("hash").to_hex();
    let global = index_map(&answer.response)[&hash][f.index];
    (f, u32::try_from(global).expect("small index"))
}

/// A transfer with randomness from `label`: `r`, then `alpha` and the decoy responses.
#[allow(clippy::too_many_arguments)]
pub fn transfer(
    label: &str,
    version: TxVersion,
    spent: &FoundOutput,
    global_index: u32,
    decoys: Vec<(u32, PublicKey)>,
    to: &Address,
    amount: u64,
    change: &Address,
) -> BuiltTransaction {
    let mut draws = vec![scalar(&format!("walletsync {label} tx key"))];
    let ring_size = decoys.len() + 1;
    match version {
        TxVersion::V2 => {
            draws.push(scalar(&format!("walletsync {label} alpha")));
            for k in 1..ring_size {
                draws.push(scalar(&format!("walletsync {label} decoy {k}")));
            }
        }
        TxVersion::V1 => {
            assert_eq!(ring_size, 1, "version-1 transfers here have no decoys");
            draws.push(scalar(&format!("walletsync {label} k")));
        }
    }
    let source = InputSource {
        amount: spent.amount,
        global_index,
        key: spent.key,
        secret: spent.secret,
        key_image: spent.key_image,
        decoys,
    };
    let opts = TransferOptions {
        version,
        fee: 10,
        ..TransferOptions::default()
    };
    let destination = Destination {
        address: *to,
        amount,
    };
    let mut rng = ScriptedRng::new(&draws);
    let built = build_transaction(&[source], &[destination], change, &opts, &mut rng)
        .expect("transfer builds");
    rng.assert_consumed();
    built
}
