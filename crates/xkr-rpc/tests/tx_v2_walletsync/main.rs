//! Vector E of `docs/tx-v2.md` §7, `docs/test-vectors/tx-v2-walletsync.json`: the wallet
//! RPC answers of a fork-14 testnet chain built in process (fork 13 at 2, fork 14 at `F` = 4,
//! grace 3, fixed testnet difficulty, fixed timestamps), with the wallet keys and what a wallet
//! must find in them.
//!
//! `XKR_WRITE_VECTORS=1 cargo test -p xkr-rpc --test tx_v2_walletsync` writes the file; without
//! the variable the chain is rebuilt, compared with the file and the file is scanned.

#[path = "../../../xkr-wallet-core/tests/vectors/mod.rs"]
mod vectors;

mod node;

use node::{
    Exchange, F, FORK13, GRACE, Indexes, Miner, TempDb, exchange, index_map, input, node, transfer,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vectors::{Stats, Wallet, byte_hex, from_hex, secret};
use xkr_rpc::router;
use xkr_serialization::transaction_hash;
use xkr_txpool::TxSource;
use xkr_types::{PublicKey, Transaction, TxInput, tx_public_key};
use xkr_wallet_core::{
    FoundOutput, ScanStats, TxVersion, WalletKeys, scan_tagged_outputs_with_stats,
};

const FILE: &str = "tx-v2-walletsync.json";
const DESCRIPTION: &str = "Fork-14 wallet sync (docs/tx-v2.md §9): \
    xkrd RPC answers recorded from an in-process testnet chain (fork 13 at 2, fork 14 at F = 4, \
    grace 3: blocks 5..=7 accept versions 1 and 2, block 8 only 2). Subwallets share the view \
    key. expected lists every output the wallet owns (in sync order: height, coinbase first, \
    then transactions, output index), the spends of those outputs, the summed ScanStats of a \
    view-tag scan with the primary keys, the unspent balance, the tx_versions of the /getinfo \
    snapshots and the wallet's outputs in the pool transaction. Secrets are \
    H_s(\"xkr tx-v2 vectors \" || label). Generated and checked by \
    crates/xkr-rpc/tests/tx_v2_walletsync/.";
const SCENARIO: [&str; 8] = [
    "1: v5 block, v1 coinbase to primary",
    "2: v5 block, v1 coinbase to x",
    "3: v6 block, v1 coinbase to subwallet",
    "4 = F: v6 block, v1 coinbase to y",
    "5: v7 block, v2 coinbase to primary; t1 (v1, grace): primary spends its coinbase of 1, \
     1 000 000 to z, change to primary",
    "6: v7 block, v2 coinbase to z; t2 (v2): x pays 500 000 to subwallet, change to x; \
     t3 (v2): y pays 500 000 to z, change to y",
    "7: v7 block, v2 coinbase to z",
    "8 = F + 4: v7 block, v2 coinbase to z; t4 (v2, ring 2 with t3's output as decoy, first \
     seen in the pool): subwallet spends its 500 000, 200 000 to z, change to primary",
];

#[derive(Debug, Serialize, Deserialize)]
struct WalletSyncVectors {
    description: String,
    network: String,
    fork13_height: u32,
    fork14_height: u32,
    fork14_grace: u64,
    scenario: Vec<String>,
    /// Primary first; both have the same view key.
    subwallets: Vec<Wallet>,
    getwalletsyncdata: Exchange,
    get_global_indexes_for_range: Exchange,
    /// `/getinfo` with the top block at 3, 4 and 7 (`tx_versions` [1], [1, 2], [2]).
    getinfo: Vec<Exchange>,
    get_pool_changes_lite: Exchange,
    expected: Expected,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Expected {
    outputs: Vec<OwnedOutput>,
    spent: Vec<Spend>,
    scan_stats: Stats,
    unspent_balance: u64,
    tx_versions: Vec<Vec<u8>>,
    pool_outputs: Vec<OwnedOutput>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OwnedOutput {
    /// 0 for the pool transaction.
    height: u64,
    tx_hash: String,
    subwallet: String,
    index: usize,
    amount: u64,
    /// `None` in the pool (not yet in a block).
    global_index: Option<u64>,
    key: String,
    view_tag: Option<String>,
    key_image: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Spend {
    height: u64,
    tx_hash: String,
    key_image: String,
}

/// One transaction as a wallet sees it.
struct SeenTx {
    height: u64,
    hash: String,
    tx_public_key: PublicKey,
    outputs: Vec<(u64, PublicKey, Option<u8>)>,
    key_images: Vec<String>,
}

impl SeenTx {
    fn from_tx(height: u64, tx: &Transaction) -> Self {
        Self {
            height,
            hash: transaction_hash(tx).expect("hash").to_hex(),
            tx_public_key: tx_public_key(&tx.prefix.extra).expect("tx public key"),
            outputs: tx
                .prefix
                .outputs
                .iter()
                .map(|o| (o.amount, *o.key(), o.view_tag()))
                .collect(),
            key_images: tx
                .prefix
                .inputs
                .iter()
                .filter_map(|i| match i {
                    TxInput::Key { key_image, .. } => Some(key_image.to_hex()),
                    TxInput::Coinbase { .. } => None,
                })
                .collect(),
        }
    }
}

struct Keys {
    primary: WalletKeys,
    subwallet: WalletKeys,
    x: WalletKeys,
    y: WalletKeys,
    z: WalletKeys,
}

fn deterministic(label: &str) -> WalletKeys {
    WalletKeys::from_spend_secret(&secret(label)).expect("reduced secret")
}

fn keys() -> Keys {
    let primary = deterministic("walletsync primary spend");
    let subwallet =
        WalletKeys::from_secrets(&secret("walletsync subwallet spend"), &primary.view_secret)
            .expect("reduced secrets");
    Keys {
        primary,
        subwallet,
        x: deterministic("walletsync x spend"),
        y: deterministic("walletsync y spend"),
        z: deterministic("walletsync z spend"),
    }
}

/// Scans with both subwallets: the outputs found (by index) and the primary scan's stats.
fn scan_seen(tx: &SeenTx, k: &Keys) -> (Vec<(&'static str, FoundOutput)>, ScanStats) {
    let (r, outputs) = (&tx.tx_public_key, &tx.outputs);
    let (primary, stats) = scan_tagged_outputs_with_stats(&k.primary, r, outputs);
    let (sub, _) = scan_tagged_outputs_with_stats(&k.subwallet, r, outputs);
    let mut found: Vec<(&'static str, FoundOutput)> = primary
        .into_iter()
        .map(|f| ("primary", f))
        .chain(sub.into_iter().map(|f| ("subwallet", f)))
        .collect();
    found.sort_by_key(|f| f.1.index);
    (found, stats)
}

/// The wallet's outputs in `tx`, with global indexes if it is in a block.
fn owned(tx: &SeenTx, k: &Keys, indexes: Option<&Indexes>) -> Vec<OwnedOutput> {
    scan_seen(tx, k)
        .0
        .into_iter()
        .map(|(subwallet, f)| OwnedOutput {
            height: tx.height,
            tx_hash: tx.hash.clone(),
            subwallet: subwallet.to_string(),
            index: f.index,
            amount: f.amount,
            global_index: indexes.map(|m| m[&tx.hash][f.index]),
            key: f.key.to_hex(),
            view_tag: tx.outputs[f.index].2.map(byte_hex),
            key_image: f.key_image.to_hex(),
        })
        .collect()
}

/// What the wallet finds in `txs` (sync order), `indexes` from `/get_global_indexes_for_range`.
fn expected(
    txs: &[SeenTx],
    pool: &SeenTx,
    indexes: &Indexes,
    tx_versions: Vec<Vec<u8>>,
    k: &Keys,
) -> Expected {
    let mut outputs = Vec::new();
    let mut stats = ScanStats::default();
    for tx in txs {
        stats.add(&scan_seen(tx, k).1);
        outputs.extend(owned(tx, k, Some(indexes)));
    }
    let mut spent = Vec::new();
    for tx in txs {
        for image in &tx.key_images {
            if outputs.iter().any(|o| &o.key_image == image) {
                spent.push(Spend {
                    height: tx.height,
                    tx_hash: tx.hash.clone(),
                    key_image: image.clone(),
                });
            }
        }
    }
    let unspent_balance = outputs
        .iter()
        .filter(|o| spent.iter().all(|s| s.key_image != o.key_image))
        .map(|o| o.amount)
        .sum();
    Expected {
        outputs,
        spent,
        scan_stats: stats.into(),
        unspent_balance,
        tx_versions,
        pool_outputs: owned(pool, k, None),
    }
}

async fn build() -> WalletSyncVectors {
    let db = TempDb::create();
    let state = node(&db);
    let app = router(state.clone(), &[]);
    let k = keys();
    let (primary, sub, z) = (k.primary.address(), k.subwallet.address(), k.z.address());
    let mut m = Miner::new(state.clone());
    let mut infos = Vec::new();

    m.mine(&primary, &[]);
    m.mine(&k.x.address(), &[]);
    m.mine(&sub, &[]);
    infos.push(exchange(&app, "/getinfo", json!({})).await);
    m.mine(&k.y.address(), &[]);
    infos.push(exchange(&app, "/getinfo", json!({})).await);

    // F + 1: a version-1 spend in the grace period.
    let (out, global) = input(&app, &k.primary, &m.coinbases[&1], 1).await;
    let t1 = transfer(
        "t1",
        TxVersion::V1,
        &out,
        global,
        vec![],
        &z,
        1_000_000,
        &primary,
    );
    m.mine(&primary, &[&t1]);

    // F + 2: two version-2 transfers, one to the subwallet, one foreign.
    let (out, global) = input(&app, &k.x, &m.coinbases[&2], 2).await;
    let change = k.x.address();
    let t2 = transfer(
        "t2",
        TxVersion::V2,
        &out,
        global,
        vec![],
        &sub,
        500_000,
        &change,
    );
    let (out, global) = input(&app, &k.y, &m.coinbases[&4], 4).await;
    let change = k.y.address();
    let t3 = transfer(
        "t3",
        TxVersion::V2,
        &out,
        global,
        vec![],
        &z,
        500_000,
        &change,
    );
    m.mine(&z, &[&t2, &t3]);
    m.mine(&z, &[]);

    // F + 4: the subwallet spends its output, ring with t3's output; first through the pool.
    let (own, own_global) = input(&app, &k.subwallet, &t2.tx, 6).await;
    let (decoy, decoy_global) = input(&app, &k.z, &t3.tx, 6).await;
    let decoys = vec![(decoy_global, decoy.key)];
    let t4 = transfer(
        "t4",
        TxVersion::V2,
        &own,
        own_global,
        decoys,
        &z,
        200_000,
        &primary,
    );
    let added = state
        .chain
        .submit_transaction(t4.blob.clone(), TxSource::Local);
    assert_eq!(added.expect("pool").expect("t4 admitted"), t4.hash);
    let pool_request = json!({ "tailBlockId": m.tip.to_hex(), "knownTxsIds": [] });
    let pool = exchange(&app, "/get_pool_changes_lite", pool_request).await;
    infos.push(exchange(&app, "/getinfo", json!({})).await);
    m.mine(&z, &[&t4]);

    let sync_request = json!({
        "blockHashCheckpoints": [],
        "startHeight": 1,
        "startTimestamp": 0,
        "blockCount": 100,
    });
    let sync = exchange(&app, "/getwalletsyncdata", sync_request).await;
    let range = json!({ "startHeight": 1, "endHeight": 9 });
    let indexes = exchange(&app, "/get_global_indexes_for_range", range).await;
    let tx_versions = infos.iter().map(|i| versions(&i.response)).collect();
    let seen: Vec<SeenTx> = m
        .mined
        .iter()
        .map(|(h, tx)| SeenTx::from_tx(*h, tx))
        .collect();
    let pool_tx = SeenTx::from_tx(0, &t4.tx);
    let index_of = index_map(&indexes.response);
    let expected = expected(&seen, &pool_tx, &index_of, tx_versions, &k);
    WalletSyncVectors {
        description: DESCRIPTION.to_string(),
        network: "testnet".to_string(),
        fork13_height: FORK13,
        fork14_height: F,
        fork14_grace: GRACE,
        scenario: SCENARIO.iter().map(ToString::to_string).collect(),
        subwallets: vec![
            Wallet::new("primary", &k.primary),
            Wallet::new("subwallet", &k.subwallet),
        ],
        getwalletsyncdata: sync,
        get_global_indexes_for_range: indexes,
        getinfo: infos,
        get_pool_changes_lite: pool,
        expected,
    }
}

fn versions(info: &Value) -> Vec<u8> {
    let list = info["tx_versions"].as_array().expect("tx_versions");
    list.iter()
        .map(|v| u8::try_from(v.as_u64().expect("version")).expect("small"))
        .collect()
}

/// A transaction of the `/getwalletsyncdata` or pool JSON as a wallet parses it.
fn seen_json(
    height: u64,
    hash: &str,
    tx_public: &str,
    outputs: &[Value],
    inputs: &[Value],
) -> SeenTx {
    SeenTx {
        height,
        hash: hash.to_string(),
        tx_public_key: from_hex(tx_public),
        outputs: outputs
            .iter()
            .map(|o| {
                let tag = o["view_tag"]
                    .as_str()
                    .map(|t| hex::decode(t).expect("hex")[0]);
                let key = from_hex(o["key"].as_str().expect("key"));
                (o["amount"].as_u64().expect("amount"), key, tag)
            })
            .collect(),
        key_images: inputs
            .iter()
            .map(|i| i["k_image"].as_str().expect("k_image").to_string())
            .collect(),
    }
}

fn verify(v: &WalletSyncVectors) {
    let k = keys();
    let primary = v.subwallets[0].keys();
    let subwallet = v.subwallets[1].keys();
    assert_eq!(primary, k.primary);
    assert_eq!(subwallet, k.subwallet);
    assert_eq!(
        primary.view_secret, subwallet.view_secret,
        "subwallets share the view key"
    );
    let none: Vec<Value> = Vec::new();
    let mut txs = Vec::new();
    let items = v.getwalletsyncdata.response["items"]
        .as_array()
        .expect("items");
    assert_eq!(v.getwalletsyncdata.response["status"], "OK");
    assert_eq!(items.len(), 8);
    for (n, item) in items.iter().enumerate() {
        let height = item["blockHeight"].as_u64().expect("height");
        assert_eq!(height, n as u64 + 1);
        let cb = &item["coinbaseTX"];
        let outputs = cb["outputs"].as_array().expect("outputs");
        // Coinbases are version 2 (tagged) from F + 1 on.
        let tagged = outputs.iter().all(|o| o.get("view_tag").is_some());
        assert_eq!(tagged, height > u64::from(F), "coinbase at {height}");
        let hash = cb["hash"].as_str().expect("hash");
        let tx_public = cb["txPublicKey"].as_str().expect("txPublicKey");
        txs.push(seen_json(height, hash, tx_public, outputs, &none));
        for t in item["transactions"].as_array().expect("transactions") {
            let hash = t["hash"].as_str().expect("hash");
            let tx_public = t["txPublicKey"].as_str().expect("txPublicKey");
            let outputs = t["outputs"].as_array().expect("outputs");
            let inputs = t["inputs"].as_array().expect("inputs");
            txs.push(seen_json(height, hash, tx_public, outputs, inputs));
        }
    }
    let added = v.get_pool_changes_lite.response["addedTxs"]
        .as_array()
        .expect("addedTxs");
    assert_eq!(added.len(), 1);
    let prefix = &added[0]["transactionPrefixInfo.txPrefix"];
    assert_eq!(prefix["version"], 2);
    let extra = hex::decode(prefix["extra"].as_str().expect("extra")).expect("hex");
    let pool_public = tx_public_key(&extra).expect("pool tx public key").to_hex();
    let pool_outputs: Vec<Value> = prefix["vout"]
        .as_array()
        .expect("vout")
        .iter()
        .map(|o| {
            assert_eq!(o["target"]["type"], "03");
            let data = &o["target"]["data"];
            json!({ "amount": o["amount"], "key": data["key"], "view_tag": data["view_tag"] })
        })
        .collect();
    let pool_inputs: Vec<Value> = prefix["vin"]
        .as_array()
        .expect("vin")
        .iter()
        .map(|i| i["value"].clone())
        .collect();
    let pool_hash = added[0]["transactionPrefixInfo.txHash"]
        .as_str()
        .expect("txHash");
    let pool = seen_json(0, pool_hash, &pool_public, &pool_outputs, &pool_inputs);
    let tx_versions: Vec<Vec<u8>> = v.getinfo.iter().map(|i| versions(&i.response)).collect();
    assert_eq!(tx_versions, vec![vec![1], vec![1, 2], vec![2]]);
    assert_eq!(v.getinfo.len(), 3);
    for (info, height) in v.getinfo.iter().zip([4, 5, 8]) {
        let got = info.response["height"].as_u64();
        assert_eq!(got, Some(height), "getinfo height = top block + 1");
    }
    let indexes = index_map(&v.get_global_indexes_for_range.response);
    let got = expected(&txs, &pool, &indexes, tx_versions, &k);
    assert_eq!(got, v.expected);
    // The scenario's shape: tagged outputs were skipped by tag, the spend at F + 4 is found.
    assert!(v.expected.scan_stats.skipped_by_tag > 0);
    assert_eq!(
        v.expected.spent.len(),
        2,
        "t1 and t4 spend the wallet's outputs"
    );
    assert!(
        v.expected
            .pool_outputs
            .iter()
            .all(|o| o.subwallet == "primary")
    );
    // t4's change of 299 990 = 90 + 900 + 9 000 + 90 000 + 200 000
    assert_eq!(v.expected.pool_outputs.len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_walletsync_vectors() {
    if !vectors::writing() {
        return;
    }
    let fresh = build().await;
    let v = vectors::generate(FILE, || fresh).expect("writing");
    verify(&v);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walletsync_vectors() {
    if vectors::writing() {
        return;
    }
    let fresh = build().await;
    let v = vectors::load(FILE, || fresh).expect("not writing");
    verify(&v);
}
