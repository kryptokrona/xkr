//! `/queryblockslite` and `/get_blocks_hashes_by_timestamps`, the block routes the C++
//! `NodeRpcProxy` (kryptokrona-service, xkrwallet) uses besides the wallet subset, on a testnet
//! chain built in process (fixed difficulty, a valid merge-mining tag, chosen timestamps, as in
//! `pruned.rs`). Requests go straight into the router, no socket.
//!
//! Shapes follow `COMMAND_RPC_QUERY_BLOCKS_LITE`, `COMMAND_RPC_GET_BLOCKS_HASHES_BY_TIMESTAMPS`
//! (`src/rpc/core_rpc_server_commands_definitions.h`), `BlockShortInfo` /
//! `TransactionPrefixInfo` (`src/cryptonote_core/icore_definitions.h`, serialized with the
//! dotted `KV_MEMBER` keys in `src/rpc/rpc_server.cpp`) and the `TransactionPrefix` serializer
//! (`src/cryptonote_core/cryptonote_serialization.cpp`).
//!
//! The chain (day `D` = 86 400 s, `D0` a UTC midnight):
//!
//! | height | timestamp |
//! |---|---|
//! | 0 | 0 (genesis) |
//! | 1, 2 | `D0 + 100`, `D0 + 200` |
//! | 3, 4 | `D0 + D + 500`, `D0 + D + 400` (out of order) |
//! | 5 | `D0 + 300` (back on the first day) |
//! | 6, 7 | `D0 + 3D + 10`, `D0 + 3D + 20`; block 7 spends block 1's miner output |
//! | 8 ..= 110 | `D0 + 4D + 30 * (height - 8)` |
//!
//! Day `D0 + 2D` has no block.

use std::collections::BTreeSet;
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
    aux_block_header_hash, block_hash, decode_block, encode_block, encode_transaction,
    transaction_hash_from_blob, transaction_prefix_hash,
};
use xkr_txpool::{PoolConfig, TxPool};
use xkr_types::{
    Block, BlockHeader, Hash, ParentBlock, PublicKey, RawBlock, SecretKey, Transaction,
    TransactionPrefix, TxInput, TxOutput, TxOutputTarget,
};

const DAY: u64 = 86_400;
/// A UTC midnight (2025-06-15).
const D0: u64 = 1_749_945_600;
/// Top block of the test chain.
const TOP: u64 = 110;
/// Fee of the spend in block 7.
const FEE: u64 = 1_000;
const FAILED: &str = "Failed to perform query";

/// Timestamp of the block at `height` (see the module table).
fn timestamp_of(height: u64) -> u64 {
    match height {
        0 => 0,
        1 => D0 + 100,
        2 => D0 + 200,
        3 => D0 + DAY + 500,
        4 => D0 + DAY + 400,
        5 => D0 + 300,
        6 => D0 + 3 * DAY + 10,
        7 => D0 + 3 * DAY + 20,
        h => D0 + 4 * DAY + 30 * (h - 8),
    }
}

/// A database file under the temp dir, removed on drop.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "xkr-rpc-node-proxy-{}-{}.redb",
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
        clients: None,
        explorer: true,
        info: NodeInfo {
            version: "rpc-node-proxy-test".into(),
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

    /// Adds the next block with `txs` at `timestamp`, paying `miner` the base reward plus
    /// `fee`; returns its hash and miner transaction.
    fn mine(
        &mut self,
        chain: &Chain,
        miner: PublicKey,
        txs: &[Vec<u8>],
        fee: u64,
        timestamp: u64,
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
                    height: u32::try_from(height).expect("small height"),
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
                timestamp,
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

/// A version-1 transaction spending output `(amount, index)` owned by `(public, secret)` to a
/// fresh key, paying [`FEE`]; its blob and prefix.
fn spend(
    public: PublicKey,
    secret: &SecretKey,
    amount: u64,
    index: u32,
) -> (Vec<u8>, TransactionPrefix) {
    let mut rng = os_rng();
    let key_image = generate_key_image(&public, secret);
    // A transaction public key field (tag 0x01 and 32 bytes).
    let mut extra = vec![0x01];
    extra.extend_from_slice(&generate_keys(&mut os_rng()).0.0);
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
            target: TxOutputTarget::Key(generate_keys(&mut os_rng()).0),
        }],
        extra,
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
    let blob = encode_transaction(&Transaction {
        prefix: prefix.clone(),
        signatures: vec![signatures],
        clsag_signatures: Vec::new(),
    })
    .expect("transaction encodes");
    (blob, prefix)
}

/// The test chain: block hashes by height, the spend of block 7 (hash, prefix).
struct TestChain {
    hashes: Vec<Hash>,
    spend_hash: Hash,
    spend_prefix: TransactionPrefix,
}

fn build_chain(chain: &Chain) -> TestChain {
    let mut miner = Miner::new(chain);
    let (owner_pub, owner_sec) = generate_keys(&mut os_rng());
    let mut hashes = vec![chain.genesis_hash()];
    let mut coinbase1: Option<Transaction> = None;
    let mut spent: Option<(Hash, TransactionPrefix)> = None;
    for height in 1..=TOP {
        let txs = if height == 7 {
            let coinbase = coinbase1.as_ref().expect("block 1 mined");
            let index = {
                let reader = chain.reader().expect("reader");
                chain
                    .with_compressor(|c| reader.tx_global_indexes(1, 0, c))
                    .expect("compressor")
                    .expect("indexes")
                    .expect("block")[0]
            };
            let (blob, prefix) = spend(
                owner_pub,
                &owner_sec,
                coinbase.prefix.outputs[0].amount,
                index,
            );
            spent = Some((transaction_hash_from_blob(blob.as_slice()), prefix));
            vec![blob]
        } else {
            Vec::new()
        };
        let payee = if height == 1 {
            owner_pub
        } else {
            generate_keys(&mut os_rng()).0
        };
        let fee = if txs.is_empty() { 0 } else { FEE };
        let (hash, coinbase) = miner.mine(chain, payee, &txs, fee, timestamp_of(height));
        if height == 1 {
            coinbase1 = Some(coinbase);
        }
        hashes.push(hash);
    }
    let (spend_hash, spend_prefix) = spent.expect("block 7 spends");
    TestChain {
        hashes,
        spend_hash,
        spend_prefix,
    }
}

/// Sends `body` to `path` with `method`; the status and the JSON answer (`Null` if none).
async fn call(app: &Router, method: &str, path: &str, body: Vec<u8>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 256 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_json(app: &Router, path: &str, body: &Value) -> (StatusCode, Value) {
    call(app, "POST", path, serde_json::to_vec(body).expect("json")).await
}

/// `/queryblockslite` with `blockIds` and `timestamp`; asserts `200` and the C++ response keys.
async fn lite(app: &Router, ids: &[Hash], timestamp: u64) -> Value {
    let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
    let (status, v) = post_json(
        app,
        "/queryblockslite",
        &json!({ "blockIds": ids, "timestamp": timestamp }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        keys(&v),
        [
            "currentHeight",
            "fullOffset",
            "items",
            "startHeight",
            "status"
        ],
        "{v}"
    );
    v
}

fn keys(v: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = v
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    k.sort_unstable();
    k
}

/// `(startHeight, currentHeight, fullOffset, item count)`.
fn summary(v: &Value) -> (u64, u64, u64, usize) {
    (
        v["startHeight"].as_u64().expect("startHeight"),
        v["currentHeight"].as_u64().expect("currentHeight"),
        v["fullOffset"].as_u64().expect("fullOffset"),
        v["items"].as_array().expect("items").len(),
    )
}

/// The `blockShortInfo.block` bytes of an item (a JSON array of byte values).
fn block_bytes(item: &Value) -> Vec<u8> {
    item["blockShortInfo.block"]
        .as_array()
        .expect("block is an array")
        .iter()
        .map(|b| u8::try_from(b.as_u64().expect("byte value")).expect("byte"))
        .collect()
}

/// Checks the items of a `/queryblockslite` answer against the chain: item `i` is block
/// `startHeight + i`, hash-only below `fullOffset` and full from it (the stored block blob, which
/// decodes and hashes to `blockId`, and the prefixes of its transactions).
fn check_items(app_chain: &Chain, t: &TestChain, v: &Value) {
    let start = v["startHeight"].as_u64().expect("startHeight");
    let full_offset = v["fullOffset"].as_u64().expect("fullOffset");
    let reader = app_chain.reader().expect("reader");
    for (i, item) in v["items"].as_array().expect("items").iter().enumerate() {
        let height = start + i as u64;
        assert_eq!(
            keys(item),
            [
                "blockShortInfo.block",
                "blockShortInfo.blockId",
                "blockShortInfo.txPrefixes"
            ],
            "item {i}"
        );
        assert_eq!(
            item["blockShortInfo.blockId"],
            t.hashes[height as usize].to_string(),
            "item {i}"
        );
        if height < full_offset {
            assert_eq!(
                item["blockShortInfo.block"],
                json!([]),
                "hash-only {height}"
            );
            assert_eq!(
                item["blockShortInfo.txPrefixes"],
                json!([]),
                "hash-only {height}"
            );
            continue;
        }
        let bytes = block_bytes(item);
        let stored = app_chain
            .with_compressor(|c| reader.block_blob(height, c))
            .expect("compressor")
            .expect("read")
            .expect("block");
        assert_eq!(bytes, stored, "block {height}");
        let block = decode_block(&bytes).expect("the block blob decodes");
        assert_eq!(block_hash(&block).expect("hash"), t.hashes[height as usize]);
        assert_eq!(block.header.timestamp, timestamp_of(height));
        let prefixes = item["blockShortInfo.txPrefixes"]
            .as_array()
            .expect("txPrefixes");
        if height == 7 {
            assert_eq!(prefixes.len(), 1);
            assert_eq!(
                keys(&prefixes[0]),
                [
                    "transactionPrefixInfo.txHash",
                    "transactionPrefixInfo.txPrefix"
                ]
            );
            assert_eq!(
                prefixes[0]["transactionPrefixInfo.txHash"],
                t.spend_hash.to_string()
            );
            assert_eq!(
                prefixes[0]["transactionPrefixInfo.txPrefix"],
                spend_prefix_json(&t.spend_prefix)
            );
        } else {
            assert!(prefixes.is_empty(), "block {height} has no transactions");
        }
    }
}

/// The C++ JSON of the spend's prefix, field by field (`serialize(TransactionPrefix &)`:
/// `version`, `unlock_time`, `vin` as `{ type: "02", value: { amount, key_offsets, k_image } }`,
/// `vout` as `{ amount, target: { type: "02", data: { key } } }`, `extra` as hex of 33 bytes).
fn spend_prefix_json(p: &TransactionPrefix) -> Value {
    let TxInput::Key {
        amount,
        output_indexes,
        key_image,
    } = &p.inputs[0]
    else {
        panic!("a key input");
    };
    let TxOutputTarget::Key(key) = &p.outputs[0].target else {
        panic!("an untagged output");
    };
    assert_eq!(p.extra.len(), 33);
    json!({
        "version": 1,
        "unlock_time": 0,
        "vin": [{
            "type": "02",
            "value": { "amount": amount, "key_offsets": output_indexes, "k_image": key_image.to_string() },
        }],
        "vout": [{
            "amount": p.outputs[0].amount,
            "target": { "type": "02", "data": { "key": key.to_string() } },
        }],
        "extra": hex::encode(&p.extra),
    })
}

/// What `BlockchainSynchronizer` does with a wallet created at `created`: query with the short
/// history (last known hash, genesis) and the creation timestamp until the answer reaches
/// `currentHeight`, adding every item after the last known block. Returns the hashes it ends
/// with and the heights it received in full.
async fn sync_like_walletd(app: &Router, genesis: Hash, created: u64) -> (Vec<Hash>, Vec<u64>) {
    let mut known = vec![genesis];
    let mut full = Vec::new();
    for _ in 0..20 {
        let last = *known.last().expect("genesis");
        let ids = if last == genesis {
            vec![genesis]
        } else {
            vec![last, genesis]
        };
        let v = lite(app, &ids, created).await;
        assert_eq!(v["status"], "OK");
        let start = v["startHeight"].as_u64().expect("startHeight");
        for (i, item) in v["items"].as_array().expect("items").iter().enumerate() {
            let height = start + i as u64;
            let hash: Hash = item["blockShortInfo.blockId"]
                .as_str()
                .expect("blockId")
                .parse()
                .expect("hash");
            if (height as usize) < known.len() {
                assert_eq!(known[height as usize], hash, "known block {height}");
                continue;
            }
            assert_eq!(height as usize, known.len(), "blocks arrive in order");
            known.push(hash);
            if !block_bytes(item).is_empty() {
                full.push(height);
            }
        }
        if known.len() as u64 == v["currentHeight"].as_u64().expect("currentHeight") + 1 {
            return (known, full);
        }
    }
    panic!("the sync did not finish");
}

/// `/queryblockslite` as `Core::queryBlocksLite` answers it: start from the first known hash,
/// `fullOffset` from the timestamp's day (walking back over days without blocks, the first
/// block of the day in chain order), hash-only items up to `fullOffset`, then at most 100 full
/// blocks; and the failure answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_blocks_lite_follows_the_cpp_core() {
    let db = TempDb::new();
    let st = testnet_state(&db);
    let t = build_chain(&st.chain);
    let genesis = t.hashes[0];
    let app = router(st.clone(), &[]);

    // From genesis without a timestamp: everything in full, 100 blocks per answer.
    let v = lite(&app, &[genesis], 0).await;
    assert_eq!(v["status"], "OK");
    assert_eq!(summary(&v), (0, TOP, 0, 100));
    check_items(&st.chain, &t, &v);

    // A day with blocks: hash-only items before its first block (6), then full blocks.
    let v = lite(&app, &[genesis], D0 + 3 * DAY + 5).await;
    assert_eq!(summary(&v), (0, TOP, 6, 106));
    check_items(&st.chain, &t, &v);

    // A day without blocks walks back to the previous one; its first block in chain order is
    // 3, although block 4 has the smaller timestamp.
    let v = lite(&app, &[genesis], D0 + 2 * DAY + 5).await;
    assert_eq!(summary(&v), (0, TOP, 3, 103));
    check_items(&st.chain, &t, &v);
    let v = lite(&app, &[genesis], D0 + DAY + 450).await;
    assert_eq!(summary(&v).2, 3);

    // The first day: block 1, not block 5 (same day, later in the chain).
    let v = lite(&app, &[genesis], D0 + 50).await;
    assert_eq!(summary(&v), (0, TOP, 1, 101));
    check_items(&st.chain, &t, &v);

    // After the last block: the first block of the last day with blocks.
    let v = lite(&app, &[genesis], D0 + 100 * DAY).await;
    assert_eq!(summary(&v), (0, TOP, 8, 108));
    check_items(&st.chain, &t, &v);

    // Before every block but genesis: 0.
    let v = lite(&app, &[genesis], D0 - 5 * DAY).await;
    assert_eq!(summary(&v), (0, TOP, 0, 100));

    // A known block and no timestamp: the timestamp of that block (day of block 3), raised to
    // the known height; items start at the known block itself.
    let v = lite(&app, &[t.hashes[4], genesis], 0).await;
    assert_eq!(summary(&v), (4, TOP, 4, 100));
    check_items(&st.chain, &t, &v);

    // A known block above `fullOffset`: full blocks from the known block.
    let v = lite(&app, &[t.hashes[105], t.hashes[64], genesis], D0).await;
    assert_eq!(summary(&v), (105, TOP, 105, 6));
    check_items(&st.chain, &t, &v);

    // Unknown hashes are skipped; an empty list starts at genesis.
    let unknown = Hash([0x42; 32]);
    let v = lite(&app, &[unknown, t.hashes[2]], 0).await;
    assert_eq!(summary(&v), (2, TOP, 2, 100));
    let v = lite(&app, &[], 0).await;
    assert_eq!(summary(&v), (0, TOP, 0, 100));

    // The top block itself.
    let v = lite(&app, &[t.hashes[TOP as usize]], 0).await;
    assert_eq!(summary(&v), (TOP, TOP, TOP, 1));
    check_items(&st.chain, &t, &v);

    // No known hash on the main chain, or a timestamp above time_t: the C++ failure answer.
    let failed = json!({ "status": FAILED, "startHeight": 0, "currentHeight": 0, "fullOffset": 0, "items": [] });
    assert_eq!(lite(&app, &[unknown], 0).await, failed);
    assert_eq!(lite(&app, &[genesis], 1 << 63).await, failed);
    assert_eq!(lite(&app, &[genesis], u64::MAX).await, failed);

    // A block id that is not a hash is refused.
    let (status, _) = post_json(
        &app,
        "/queryblockslite",
        &json!({ "blockIds": ["zz"], "timestamp": 0 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // GET with an empty body is the default request, like every other route.
    let (status, v) = call(&app, "GET", "/queryblockslite", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summary(&v), (0, TOP, 0, 100));
}

/// The walletd sync loop over `/queryblockslite`: a wallet created on the fourth day gets every
/// hash once, in order, and the blocks from that day's first block on in full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walletd_sync_loop_reaches_the_top() {
    let db = TempDb::new();
    let st = testnet_state(&db);
    let t = build_chain(&st.chain);
    let app = router(st.clone(), &[]);

    let (hashes, full) = sync_like_walletd(&app, t.hashes[0], D0 + 3 * DAY + 5).await;
    assert_eq!(hashes, t.hashes);
    assert_eq!(full, (6..=TOP).collect::<Vec<_>>());

    // A wallet created before the chain: every block in full.
    let (hashes, full) = sync_like_walletd(&app, t.hashes[0], 1).await;
    assert_eq!(hashes, t.hashes);
    assert_eq!(full, (1..=TOP).collect::<Vec<_>>());
}

/// `/get_blocks_hashes_by_timestamps` as `Core::getBlockHashesByTimestamps` answers it: blocks
/// with a timestamp in `[timestampBegin, timestampBegin + secondsCount)`, by timestamp, then
/// height; the overflow error; the body limit of both routes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_hashes_by_timestamps() {
    let db = TempDb::new();
    let st = testnet_state(&db);
    let t = build_chain(&st.chain);
    let app = router(st.clone(), &[]);
    let h = |heights: &[usize]| -> Value {
        json!(
            heights
                .iter()
                .map(|i| t.hashes[*i].to_string())
                .collect::<Vec<_>>()
        )
    };
    let by_ts = |begin: u64, count: u64| {
        let app = app.clone();
        async move {
            let (status, v) = post_json(
                &app,
                "/get_blocks_hashes_by_timestamps",
                &json!({ "timestampBegin": begin, "secondsCount": count }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{v}");
            assert_eq!(keys(&v), ["blockHashes", "status"], "{v}");
            v
        }
    };

    let v = by_ts(D0, DAY).await;
    assert_eq!(v["status"], "OK");
    assert_eq!(v["blockHashes"], h(&[1, 2, 5]));
    let v = by_ts(D0, 2 * DAY).await;
    assert_eq!(v["blockHashes"], h(&[1, 2, 5, 4, 3]));
    // End exclusive.
    assert_eq!(by_ts(D0 + 100, 100).await["blockHashes"], h(&[1]));
    assert_eq!(by_ts(D0 + 100, 101).await["blockHashes"], h(&[1, 2]));
    assert_eq!(by_ts(D0 + 100, 0).await["blockHashes"], json!([]));
    assert_eq!(by_ts(D0 + 2 * DAY, DAY).await["blockHashes"], json!([]));
    // The genesis block (timestamp 0) is not listed, as in C++.
    assert_eq!(by_ts(0, 1).await["blockHashes"], json!([]));
    let v = by_ts(D0 + 3 * DAY, 10 * DAY).await;
    assert_eq!(v["blockHashes"], h(&(6..=TOP as usize).collect::<Vec<_>>()));
    // Everything but genesis: 110 distinct hashes.
    let v = by_ts(0, D0 + 10 * DAY).await;
    let all: BTreeSet<String> = v["blockHashes"]
        .as_array()
        .expect("hashes")
        .iter()
        .map(|x| x.as_str().expect("hash").to_owned())
        .collect();
    assert_eq!(all.len(), t.hashes.len() - 1);
    assert!(!all.contains(&t.hashes[0].to_string()));

    let v = by_ts(u64::MAX - 5, 10).await;
    assert_eq!(
        v,
        json!({ "status": "Error: Timestamp overflow", "blockHashes": [] })
    );

    // The default request (empty body) is an empty range.
    let (status, v) = call(&app, "GET", "/get_blocks_hashes_by_timestamps", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!({ "status": "OK", "blockHashes": [] }));

    for path in ["/queryblockslite", "/get_blocks_hashes_by_timestamps"] {
        let (status, _) = call(&app, "POST", path, vec![b' '; 2 * 1024 * 1024 + 1]).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
    }
}
