//! An in-process xkrd for the walletd tests: the fork-14 test chain of
//! `crates/xkr-rpc/tests/tx_v2_walletsync/node.rs` behind the `xkr-rpc` router, a block
//! builder whose coinbases are split into denominations (as C++ miners pay, so fusion and ring
//! members have outputs to work with), and helpers to talk to a walletd router.
#![allow(dead_code)] // each test file uses a different part

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use xkr_chain::AddBlockOutcome;
use xkr_consensus::Network;
use xkr_consensus::reward::get_block_reward;
use xkr_crypto::{derive_public_key, generate_key_derivation, view_tag};
use xkr_daemon_client::{
    DaemonClient, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use xkr_rpc::RpcState;
use xkr_serialization::{aux_block_header_hash, block_hash, decode_transaction, encode_block};
use xkr_types::{
    Block, BlockHeader, Hash, ParentBlock, RawBlock, Transaction, TransactionPrefix, TxInput,
    TxOutput, TxOutputTarget,
};
use xkr_wallet::{FileFormat, Node, SyncConfig, Wallet, WalletConfig};
use xkr_wallet_core::Address;
use xkr_wallet_core::select::split_amount_into_denominations;

use crate::node::{self, TempDb};
use crate::vectors::{public, secret};

/// Hands daemon requests to the `xkr-rpc` router in process.
#[derive(Clone)]
pub struct RouterTransport(pub Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = Request::builder()
            .method(method)
            .uri(&request.path)
            .header("content-type", "application/json")
            .body(Body::from(request.body))
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        let Ok(resp) = self.0.clone().oneshot(req).await;
        let status = resp.status().as_u16();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }
}

/// A pool transaction to put in a block.
#[derive(Debug, Clone)]
pub struct PoolTx {
    pub hash: Hash,
    pub blob: Vec<u8>,
    pub fee: u64,
}

/// The test chain and its node.
pub struct Chain {
    pub state: Arc<RpcState>,
    pub client: DaemonClient<RouterTransport>,
    pub tip: Hash,
    pub height: u64,
    coins: u64,
    /// Block hashes by height.
    pub hashes: Vec<Hash>,
    _db: TempDb,
}

impl Chain {
    /// The genesis block only.
    pub fn new() -> Self {
        let db = TempDb::create();
        let state = node::node(&db);
        let client = DaemonClient::new(RouterTransport(xkr_rpc::router(state.clone(), &[])));
        let tip = state.chain.tip();
        Self {
            client,
            tip: tip.hash,
            height: tip.height,
            coins: tip.already_generated_coins,
            hashes: vec![tip.hash],
            state,
            _db: db,
        }
    }

    /// A node for a wallet.
    pub fn node(&self) -> Node<RouterTransport> {
        Node::new(self.client.clone(), "router", 0)
    }

    /// Adds the next block: the coinbase pays `miner` the base reward plus the fees, split
    /// into denominations; `txs` are included.
    pub fn mine(&mut self, miner: &Address, txs: &[PoolTx]) {
        let height = self.height + 1;
        let base = get_block_reward(5, 0, 100, self.coins, 0)
            .expect("reward")
            .0;
        let fees: u64 = txs.iter().map(|t| t.fee).sum();
        let v2 = height > u64::from(node::F);
        let r = secret(&format!("walletd coinbase {height}"));
        let d = generate_key_derivation(&miner.view, &r).expect("miner view key");
        let outputs: Vec<TxOutput> = split_amount_into_denominations(base + fees)
            .into_iter()
            .enumerate()
            .map(|(i, amount)| {
                let i = i as u64;
                let key = derive_public_key(&d, i, &miner.spend).expect("miner spend key");
                let target = if v2 {
                    TxOutputTarget::TaggedKey {
                        key,
                        view_tag: view_tag(&d, i),
                    }
                } else {
                    TxOutputTarget::Key(key)
                };
                TxOutput { amount, target }
            })
            .collect();
        let mut extra = vec![0x01];
        extra.extend_from_slice(&public(&r).0);
        let miner_tx = Transaction {
            prefix: TransactionPrefix {
                version: if v2 { 2 } else { 1 },
                unlock_time: height + 1,
                inputs: vec![TxInput::Coinbase {
                    height: height as u32,
                }],
                outputs,
                extra,
            },
            signatures: Vec::new(),
            clsag_signatures: Vec::new(),
        };
        let major_version = if v2 {
            7
        } else if height > u64::from(node::FORK13) {
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
                timestamp: node::BASE_TIMESTAMP + height * 30,
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
            miner_tx,
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
        self.tip = hash;
        self.height = height;
        self.coins += base;
        self.hashes.push(hash);
    }

    /// Mines `count` empty blocks to `miner`.
    pub fn mine_many(&mut self, miner: &Address, count: u64) {
        for _ in 0..count {
            self.mine(miner, &[]);
        }
    }

    /// The pool transactions `hashes`, with their fees.
    pub async fn pool_txs(&self, hashes: &[Hash]) -> Vec<PoolTx> {
        if hashes.is_empty() {
            return Vec::new();
        }
        let answer = self
            .client
            .transactions(hashes)
            .await
            .expect("gettransactions");
        assert!(
            answer.missed.is_empty(),
            "not in the pool: {:?}",
            answer.missed
        );
        hashes
            .iter()
            .zip(answer.blobs)
            .map(|(hash, blob)| {
                let tx = decode_transaction(&blob).expect("decodes");
                let inputs: u64 = tx
                    .prefix
                    .inputs
                    .iter()
                    .map(|i| match i {
                        TxInput::Key { amount, .. } => *amount,
                        TxInput::Coinbase { .. } => 0,
                    })
                    .sum();
                let outputs: u64 = tx.prefix.outputs.iter().map(|o| o.amount).sum();
                PoolTx {
                    hash: *hash,
                    blob,
                    fee: inputs - outputs,
                }
            })
            .collect()
    }

    /// Mines the wallet's pending transactions into the next block.
    pub async fn mine_pending<T: Transport + 'static>(
        &mut self,
        wallet: &Wallet<T>,
        miner: &Address,
    ) {
        let hashes: Vec<Hash> = wallet
            .unconfirmed(None)
            .expect("unconfirmed")
            .iter()
            .map(|t| t.hash)
            .collect();
        let txs = self.pool_txs(&hashes).await;
        self.mine(miner, &txs);
    }
}

/// Engine settings for the tests: walletd containers on the test network, fast sync.
pub fn wallet_config() -> WalletConfig {
    WalletConfig {
        network: Network::Testnet,
        format: FileFormat::WalletGreen,
        sync: SyncConfig {
            poll_interval: Duration::from_millis(15),
            info_interval: Duration::from_millis(15),
            block_count: 100,
            queue_blocks: 1000,
        },
        start_sync: true,
    }
}

/// Waits (at most 60 s) until the wallet has processed the block `tip`.
pub async fn wait_synced<T: Transport + 'static>(wallet: &Wallet<T>, tip: Hash) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while wallet.status().last_block_hash != Some(tip) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the wallet to reach {tip}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An HTTP answer of the walletd router.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Answer {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

/// Sends a request to a walletd router.
pub async fn http(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Answer {
    let mut builder = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let req = builder.body(Body::from(body)).expect("request");
    let Ok(resp) = router.clone().oneshot(req).await;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    Answer {
        status,
        headers,
        body,
    }
}

/// Sends a JSON-RPC request to `/json_rpc`.
pub async fn rpc(router: &Router, request: &Value) -> Value {
    let answer = http(
        router,
        "POST",
        "/json_rpc",
        &[("Content-Type".into(), "application/json".into())],
        serde_json::to_vec(request).expect("json"),
    )
    .await;
    assert_eq!(answer.status, 200);
    answer.json()
}
