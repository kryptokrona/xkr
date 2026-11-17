//! Test daemons for the wallet engine: [`Mock`] is a small in-memory chain behind the daemon
//! routes a wallet uses (answers shaped like xkrd's), [`RouterTransport`] hands requests to the
//! `xkr-rpc` router in process. Plus transaction builders and wallet helpers.
#![allow(dead_code)] // every test file uses a different subset

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use serde_json::{Value, json};
use tower::ServiceExt;
use xkr_consensus::Network;
use xkr_crypto::{
    cn_fast_hash, derive_public_key, generate_key_derivation, generate_keys, os_rng, view_tag,
};
use xkr_daemon_client::{
    DaemonClient, HttpRequest, HttpResponse, Method, Transport, TransportError,
};
use xkr_serialization::{decode_transaction, encode_transaction, transaction_hash};
use xkr_types::{
    Hash, KeyImage, Signature, Transaction, TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
    payment_id, tx_public_key,
};
use xkr_wallet::{FileFormat, Node, SyncConfig, Wallet, WalletConfig};
use xkr_wallet_core::extra::build_extra;
use xkr_wallet_core::{Address, WalletKeys};

/// Timestamp of block 0; blocks are 90 s apart.
pub const BASE_TIMESTAMP: u64 = 1_700_000_000;

/// Fast sync settings for tests.
pub fn config(network: Network, format: FileFormat, start_sync: bool) -> WalletConfig {
    WalletConfig {
        network,
        format,
        sync: SyncConfig {
            poll_interval: Duration::from_millis(15),
            info_interval: Duration::from_millis(15),
            block_count: 100,
            queue_blocks: 1000,
        },
        start_sync,
    }
}

/// A random wallet.
pub fn keys() -> WalletKeys {
    WalletKeys::generate(&mut os_rng())
}

/// Waits (at most 20 s) until `done` holds.
pub async fn wait_until(what: &str, done: impl FnMut() -> bool) {
    wait_until_within(what, Duration::from_secs(20), done).await;
}

/// [`wait_until`] with its own deadline, for waits whose polls are slow themselves (opening
/// an encrypted wallet file runs its key derivation every time).
pub async fn wait_until_within(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until the wallet has processed the block with `hash` and its view of the node's
/// network height (polled separately, used for the unlock checks) has reached it too.
pub async fn wait_for_block<T: Transport + 'static>(wallet: &Wallet<T>, hash: Hash) {
    wait_until("the wallet to reach the tip", || {
        let s = wallet.status();
        s.last_block_hash == Some(hash) && s.network_block_count >= s.wallet_block_count
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// transactions

/// A coinbase paying `amount` to `to` at `height`.
pub fn coinbase(to: &Address, amount: u64, height: u64, unlock_time: u64, v2: bool) -> Transaction {
    let (r_pub, r_sec) = generate_keys(&mut os_rng());
    let d = generate_key_derivation(&to.view, &r_sec).expect("view key");
    let key = derive_public_key(&d, 0, &to.spend).expect("spend key");
    let target = if v2 {
        TxOutputTarget::TaggedKey {
            key,
            view_tag: view_tag(&d, 0),
        }
    } else {
        TxOutputTarget::Key(key)
    };
    let mut extra = vec![0x01];
    extra.extend_from_slice(&r_pub.0);
    Transaction {
        prefix: TransactionPrefix {
            version: if v2 { 2 } else { 1 },
            unlock_time,
            inputs: vec![TxInput::Coinbase {
                height: height as u32,
            }],
            outputs: vec![TxOutput { amount, target }],
            extra,
        },
        signatures: Vec::new(),
        clsag_signatures: Vec::new(),
    }
}

/// A payment from outside the wallet: one input with a random key image, outputs of
/// `amounts` to `to` (the mock does not check signatures).
pub fn payment(
    to: &Address,
    amounts: &[u64],
    payment_id: Option<Hash>,
    unlock_time: u64,
) -> Transaction {
    let (r_pub, r_sec) = generate_keys(&mut os_rng());
    let d = generate_key_derivation(&to.view, &r_sec).expect("view key");
    let outputs = amounts
        .iter()
        .enumerate()
        .map(|(i, &amount)| TxOutput {
            amount,
            target: TxOutputTarget::Key(
                derive_public_key(&d, i as u64, &to.spend).expect("spend key"),
            ),
        })
        .collect();
    let total: u64 = amounts.iter().sum();
    Transaction {
        prefix: TransactionPrefix {
            version: 1,
            unlock_time,
            inputs: vec![TxInput::Key {
                amount: total + 10,
                output_indexes: vec![0],
                key_image: KeyImage(generate_keys(&mut os_rng()).0.0),
            }],
            outputs,
            extra: build_extra(&r_pub, payment_id.as_ref(), None),
        },
        signatures: vec![vec![Signature::ZERO]],
        clsag_signatures: Vec::new(),
    }
}

/// The hash of a transaction.
pub fn hash_of(tx: &Transaction) -> Hash {
    transaction_hash(tx).expect("encodes")
}

// ---------------------------------------------------------------------------------------------
// the mock daemon

/// One block of the mock chain.
#[derive(Debug, Clone)]
pub struct MockBlock {
    pub hash: Hash,
    pub timestamp: u64,
    pub coinbase: Transaction,
    pub txs: Vec<Transaction>,
}

/// The mock chain and its knobs.
#[derive(Debug, Default)]
pub struct MockState {
    pub blocks: Vec<MockBlock>,
    pub pool: Vec<Transaction>,
    /// `/info` `tx_versions` (`None`: a C++ node).
    pub tx_versions: Option<Vec<u8>>,
    /// `/info` `major_version`.
    pub major_version: u8,
    /// `/fee`.
    pub fee: (String, u64),
    /// Refuse every sent transaction with this reason.
    pub refuse: Option<String>,
    /// Transactions the node reports as unknown.
    pub forgotten: HashSet<Hash>,
    /// No answers at all.
    pub offline: bool,
    /// Ring member candidates per amount (`None`: as many as asked for, plus a repeated one).
    pub decoys: Option<usize>,
    /// Paths asked, in order.
    pub requests: Vec<String>,
    salt: u64,
}

/// An in-memory daemon.
#[derive(Debug, Clone)]
pub struct Mock(pub Arc<Mutex<MockState>>);

impl Default for Mock {
    fn default() -> Self {
        Self::new()
    }
}

impl Mock {
    /// A chain with a genesis block paying someone else.
    pub fn new() -> Self {
        let mock = Self(Arc::new(Mutex::new(MockState {
            major_version: 6,
            ..MockState::default()
        })));
        mock.mine_to(&keys().address(), 1_000, 0);
        mock
    }

    /// A [`Node`] for the wallet.
    pub fn node(&self) -> Node<Self> {
        Node::new(DaemonClient::new(self.clone()), "mock", 11898)
    }

    /// The state, locked.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.0.lock().expect("mock lock")
    }

    /// Index of the top block.
    pub fn height(&self) -> u64 {
        self.lock().blocks.len() as u64 - 1
    }

    /// Hash of the top block.
    pub fn tip(&self) -> Hash {
        self.lock().blocks.last().expect("genesis").hash
    }

    /// Adds a block with a coinbase of `amount` to `to` and every pool transaction; returns
    /// the block hash.
    pub fn mine_to(&self, to: &Address, amount: u64, unlock_time: u64) -> Hash {
        let mut st = self.lock();
        let height = st.blocks.len() as u64;
        let v2 = st.major_version >= 7;
        let coinbase = coinbase(to, amount, height, unlock_time, v2);
        let forgotten = st.forgotten.clone();
        let txs: Vec<Transaction> = std::mem::take(&mut st.pool)
            .into_iter()
            .filter(|t| !forgotten.contains(&hash_of(t)))
            .collect();
        st.salt += 1;
        let mut seed = height.to_le_bytes().to_vec();
        seed.extend_from_slice(&st.salt.to_le_bytes());
        let hash = cn_fast_hash(&seed);
        st.blocks.push(MockBlock {
            hash,
            timestamp: BASE_TIMESTAMP + height * 90,
            coinbase,
            txs,
        });
        hash
    }

    /// A block paying someone else.
    pub fn mine(&self) -> Hash {
        self.mine_to(&keys().address(), 1_000, 0)
    }

    /// Puts a transaction into the pool (it goes into the next block).
    pub fn push(&self, tx: Transaction) -> Hash {
        let hash = hash_of(&tx);
        self.lock().pool.push(tx);
        hash
    }

    /// The pool transaction with `hash`.
    pub fn pool_tx(&self, hash: &Hash) -> Option<Transaction> {
        self.lock()
            .pool
            .iter()
            .find(|t| hash_of(t) == *hash)
            .cloned()
    }

    /// Drops a pool transaction and reports it as unknown from now on.
    pub fn forget(&self, hash: Hash) {
        let mut st = self.lock();
        st.pool.retain(|t| hash_of(t) != hash);
        st.forgotten.insert(hash);
    }

    /// Removes the blocks at and above `height` (their transactions are dropped).
    pub fn rewind_to(&self, height: u64) {
        self.lock().blocks.truncate(height as usize);
    }

    fn handle(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut st = self.lock();
        st.requests.push(request.path.clone());
        if st.offline {
            return Err(TransportError::Connection("mock offline".into()));
        }
        let body: Value = if request.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).expect("JSON request")
        };
        let answer = match (request.method, request.path.as_str()) {
            (Method::Get, "/info") => st.info(),
            (Method::Get, "/fee") => {
                json!({ "status": "OK", "address": st.fee.0, "amount": st.fee.1 })
            }
            (Method::Post, "/getwalletsyncdata") => st.sync_data(&body),
            (Method::Post, "/get_global_indexes_for_range") => st.global_indexes(&body),
            (Method::Post, "/getrandom_outs") => st.random_outs(&body),
            (Method::Post, "/sendrawtransaction") => st.send_raw(&body),
            (Method::Post, "/get_transactions_status") => st.transactions_status(&body),
            (Method::Post, "/gettransactions") => st.transactions(&body),
            (Method::Post, "/json_rpc") => st.json_rpc(&body),
            _ => {
                return Ok(HttpResponse {
                    status: 404,
                    body: br#"{"status":"Not found"}"#.to_vec(),
                });
            }
        };
        Ok(HttpResponse {
            status: 200,
            body: serde_json::to_vec(&answer).expect("json"),
        })
    }
}

impl Transport for Mock {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse, TransportError>> + Send {
        std::future::ready(self.handle(&request))
    }
}

fn output_json(o: &TxOutput) -> Value {
    let mut v = json!({ "key": o.key().to_hex(), "amount": o.amount });
    if let Some(tag) = o.view_tag() {
        v["view_tag"] = json!(format!("{tag:02x}"));
    }
    v
}

fn tx_public_key_hex(tx: &Transaction) -> String {
    tx_public_key(&tx.prefix.extra).unwrap_or_default().to_hex()
}

fn coinbase_json(tx: &Transaction) -> Value {
    json!({
        "outputs": tx.prefix.outputs.iter().map(output_json).collect::<Vec<_>>(),
        "hash": hash_of(tx).to_hex(),
        "txPublicKey": tx_public_key_hex(tx),
        "unlockTime": tx.prefix.unlock_time,
    })
}

fn tx_json(tx: &Transaction) -> Value {
    let inputs: Vec<Value> = tx
        .prefix
        .inputs
        .iter()
        .filter_map(|i| match i {
            TxInput::Key {
                amount,
                output_indexes,
                key_image,
            } => Some(json!({
                "amount": amount,
                "key_offsets": output_indexes,
                "k_image": key_image.to_hex(),
            })),
            TxInput::Coinbase { .. } => None,
        })
        .collect();
    json!({
        "inputs": inputs,
        "paymentID": payment_id(&tx.prefix.extra).as_ref().map_or_default(Hash::to_hex),
        "outputs": tx.prefix.outputs.iter().map(output_json).collect::<Vec<_>>(),
        "hash": hash_of(tx).to_hex(),
        "txPublicKey": tx_public_key_hex(tx),
        "unlockTime": tx.prefix.unlock_time,
    })
}

fn header_json(blocks: &[MockBlock], height: usize) -> Value {
    let b = &blocks[height];
    let prev_hash = if height == 0 {
        Hash::ZERO
    } else {
        blocks[height - 1].hash
    };
    json!({
        "major_version": 6, "minor_version": 0, "timestamp": b.timestamp,
        "prev_hash": prev_hash.to_hex(),
        "nonce": 0, "orphan_status": false, "height": height,
        "depth": blocks.len() - 1 - height, "hash": b.hash.to_hex(), "difficulty": 1,
        "reward": 1, "num_txes": b.txs.len() + 1, "block_size": 100,
    })
}

fn hashes(body: &Value, key: &str) -> Vec<Hash> {
    body[key].as_array().map_or_default(|a| {
        a.iter()
            .map(|h| h.as_str().expect("hex").parse().expect("hash"))
            .collect()
    })
}

impl MockState {
    fn info(&self) -> Value {
        let height = self.blocks.len() as u64;
        json!({
            "status": "OK",
            "height": height,
            "network_height": height,
            "difficulty": 9_000,
            "outgoing_connections_count": 8,
            "incoming_connections_count": 2,
            "major_version": self.major_version,
            "tx_versions": self.tx_versions,
            "tx_pool_size": self.pool.len(),
        })
    }

    fn sync_data(&self, body: &Value) -> Value {
        let checkpoints = hashes(body, "blockHashCheckpoints");
        let start_height = body["startHeight"].as_u64().unwrap_or(0);
        let start_timestamp = body["startTimestamp"].as_u64().unwrap_or(0);
        let count = match body["blockCount"].as_u64().unwrap_or(100).min(100) {
            0 => 100,
            n => n,
        };
        let mut last_known = 0;
        if !checkpoints.is_empty() {
            match checkpoints
                .iter()
                .find_map(|c| self.blocks.iter().position(|b| b.hash == *c))
            {
                Some(pos) => last_known = pos as u64,
                None => return json!({ "status": "Failed to perform query" }),
            }
        }
        let timestamp_height = if start_timestamp == 0 {
            0
        } else {
            self.blocks
                .iter()
                .position(|b| b.timestamp >= start_timestamp)
                .unwrap_or(self.blocks.len()) as u64
        };
        let first = if start_height == 0 {
            timestamp_height
        } else {
            start_height
        };
        let start = (if last_known == 0 { 0 } else { last_known + 1 }).max(first);
        let end = (start + count).min(self.blocks.len() as u64);
        let items: Vec<Value> = (start..end)
            .map(|h| {
                let b = &self.blocks[h as usize];
                json!({
                    "coinbaseTX": coinbase_json(&b.coinbase),
                    "transactions": b.txs.iter().map(tx_json).collect::<Vec<_>>(),
                    "blockHeight": h,
                    "blockHash": b.hash.to_hex(),
                    "blockTimestamp": b.timestamp,
                })
            })
            .collect();
        json!({ "status": "OK", "items": items, "synced": end == self.blocks.len() as u64 })
    }

    /// Global output indexes of every transaction, by amount, in chain order.
    fn index_map(&self) -> HashMap<Hash, (u64, Vec<u64>)> {
        let mut counters: HashMap<u64, u64> = HashMap::new();
        let mut out = HashMap::new();
        for (height, b) in self.blocks.iter().enumerate() {
            for tx in std::iter::once(&b.coinbase).chain(&b.txs) {
                let indexes = tx
                    .prefix
                    .outputs
                    .iter()
                    .map(|o| {
                        let c = counters.entry(o.amount).or_insert(0);
                        *c += 1;
                        *c - 1
                    })
                    .collect();
                out.insert(hash_of(tx), (height as u64, indexes));
            }
        }
        out
    }

    fn global_indexes(&self, body: &Value) -> Value {
        let start = body["startHeight"].as_u64().unwrap_or(0);
        let end = body["endHeight"].as_u64().unwrap_or(0);
        let indexes: Vec<Value> = self
            .index_map()
            .into_iter()
            .filter(|(_, (h, _))| (start..end).contains(h))
            .map(|(hash, (_, v))| json!({ "key": hash.to_hex(), "value": v }))
            .collect();
        json!({ "status": "OK", "indexes": indexes })
    }

    fn random_outs(&self, body: &Value) -> Value {
        let count = body["outs_count"].as_u64().unwrap_or(0) as usize;
        let outs: Vec<Value> = body["amounts"]
            .as_array()
            .expect("amounts")
            .iter()
            .map(|a| {
                let amount = a.as_u64().expect("amount");
                // As many as asked for plus one repeated index (dropped by the wallet).
                let n = self.decoys.unwrap_or(count + 1);
                let entries: Vec<Value> = (0..n)
                    .map(|i| {
                        let index = i.saturating_sub(1);
                        json!({
                            "global_amount_index": index,
                            "out_key": generate_keys(&mut os_rng()).0.to_hex(),
                        })
                    })
                    .collect();
                json!({ "amount": amount, "outs": entries })
            })
            .collect();
        json!({ "status": "OK", "outs": outs })
    }

    fn send_raw(&mut self, body: &Value) -> Value {
        let blob = hex::decode(body["tx_as_hex"].as_str().expect("hex")).expect("hex");
        let Ok(tx) = decode_transaction(&blob) else {
            return json!({ "status": "Failed", "error": "does not decode" });
        };
        if let Some(reason) = &self.refuse {
            return json!({ "status": "Failed", "error": reason });
        }
        let hash = hash_of(&tx);
        self.pool.push(tx);
        json!({ "status": "OK", "transactionHash": hash.to_hex() })
    }

    fn find(&self, hash: &Hash) -> Option<(Transaction, bool)> {
        if let Some(tx) = self.pool.iter().find(|t| hash_of(t) == *hash) {
            return Some((tx.clone(), false));
        }
        self.blocks
            .iter()
            .flat_map(|b| std::iter::once(&b.coinbase).chain(&b.txs))
            .find(|t| hash_of(t) == *hash)
            .map(|t| (t.clone(), true))
    }

    fn transactions_status(&self, body: &Value) -> Value {
        let (mut in_pool, mut in_block, mut unknown) = (Vec::new(), Vec::new(), Vec::new());
        for hash in hashes(body, "transactionHashes") {
            match self.find(&hash) {
                Some((_, true)) => in_block.push(hash.to_hex()),
                Some((_, false)) if !self.forgotten.contains(&hash) => in_pool.push(hash.to_hex()),
                _ => unknown.push(hash.to_hex()),
            }
        }
        json!({
            "status": "OK",
            "transactionsInPool": in_pool,
            "transactionsInBlock": in_block,
            "transactionsUnknown": unknown,
        })
    }

    fn transactions(&self, body: &Value) -> Value {
        let (mut found, mut missed) = (Vec::new(), Vec::new());
        for hash in hashes(body, "txs_hashes") {
            match self.find(&hash) {
                Some((tx, _)) => found.push(hex::encode(encode_transaction(&tx).expect("encodes"))),
                None => missed.push(hash.to_hex()),
            }
        }
        json!({ "status": "OK", "txs_as_hex": found, "missed_tx": missed })
    }

    fn json_rpc(&self, body: &Value) -> Value {
        let params = &body["params"];
        let result = match body["method"].as_str().unwrap_or_default() {
            "get_block_headers_range" => {
                let start = params["start_height"].as_u64().unwrap_or(0) as usize;
                let end = params["end_height"].as_u64().unwrap_or(0) as usize;
                let end = end.min(self.blocks.len() - 1);
                let headers: Vec<Value> = (start..=end)
                    .map(|h| header_json(&self.blocks, h))
                    .collect();
                json!({ "status": "OK", "headers": headers })
            }
            "getblockheaderbyhash" => {
                let hash: Hash = params["hash"]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_default();
                match self.blocks.iter().position(|b| b.hash == hash) {
                    Some(h) => {
                        json!({ "status": "OK", "block_header": header_json(&self.blocks, h) })
                    }
                    None => {
                        return json!({ "jsonrpc": "2.0", "id": 0,
                            "error": { "code": -5, "message": "Block not found" } });
                    }
                }
            }
            other => {
                return json!({ "jsonrpc": "2.0", "id": 0,
                    "error": { "code": -32601, "message": format!("no method {other}") } });
            }
        };
        json!({ "jsonrpc": "2.0", "id": 0, "result": result })
    }
}

// ---------------------------------------------------------------------------------------------
// xkrd's router in process

/// Sends requests straight into an axum router.
#[derive(Debug, Clone)]
pub struct RouterTransport(pub Router);

impl Transport for RouterTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post => "POST",
        };
        let req = axum::http::Request::builder()
            .method(method)
            .uri(&request.path)
            .header("content-type", "application/json")
            .body(Body::from(request.body))
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        // The router's error type is `Infallible`.
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

/// A wallet file path in `dir`.
pub fn wallet_path(dir: &Path, name: &str) -> std::path::PathBuf {
    dir.join(name)
}
