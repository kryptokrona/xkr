//! The client against xkrd's router in process, on the chain of vector E
//! (`docs/test-vectors/tx-v2-walletsync.json`, built by `crates/xkr-rpc/tests/tx_v2_walletsync`:
//! fork 13 at 2, fork 14 at 4, grace 3, fixed keys, timestamps and randomness). The chain is
//! rebuilt here step by step with the client doing the wallet's calls, so every typed answer can
//! be compared with the answer recorded in the vector.

#[path = "../../xkr-wallet-core/tests/vectors/mod.rs"]
mod vectors;

#[allow(dead_code)] // the vector test's node; its recording helpers are not used here
#[path = "../../xkr-rpc/tests/tx_v2_walletsync/node.rs"]
mod node;

mod common;

use std::collections::HashMap;

use common::{RouterTransport, walletsync_fixture};
use node::{F, Miner, TempDb, node, transfer};
use serde_json::{Value, json};
use vectors::secret;
use xkr_daemon_client::mining::codes;
use xkr_daemon_client::{
    DaemonClient, DaemonError, Info, SendOutcome, WalletBlock, WalletSyncRequest,
};
use xkr_rpc::router;
use xkr_serialization::{transaction_hash, transaction_hash_from_blob};
use xkr_types::{Hash, Transaction};
use xkr_wallet_core::{Address, FoundOutput, TxVersion, WalletKeys, scan_transaction};

type Client = DaemonClient<RouterTransport>;

fn deterministic(label: &str) -> WalletKeys {
    WalletKeys::from_spend_secret(&secret(label)).expect("reduced secret")
}

/// The one output `keys` owns in `tx` (mined at `height`) and its global index, read with the
/// client like a wallet does.
async fn input(
    client: &Client,
    keys: &WalletKeys,
    tx: &Transaction,
    height: u64,
) -> (FoundOutput, u32) {
    let mut found = scan_transaction(keys, &tx.prefix);
    assert_eq!(found.len(), 1, "one output at height {height}");
    let f = found.remove(0);
    let indexes = client
        .global_indexes_for_range(height, height + 1)
        .await
        .expect("global indexes");
    let hash = transaction_hash(tx).expect("hash");
    let global = indexes[&hash][f.index];
    (f, u32::try_from(global).expect("small index"))
}

/// `fixture[key].response` (or `fixture[key][n].response`) parsed like the client parses it.
fn recorded<T: serde::de::DeserializeOwned>(answer: &Value) -> T {
    serde_json::from_value(answer.clone()).expect("recorded answer parses")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_reads_vector_e_from_xkrd() {
    let fixture = walletsync_fixture();
    let db = TempDb::create();
    let state = node(&db);
    let client: Client = DaemonClient::new(RouterTransport(router(state.clone(), &[])));

    let primary_keys = deterministic("walletsync primary spend");
    let sub_keys = WalletKeys::from_secrets(
        &secret("walletsync subwallet spend"),
        &primary_keys.view_secret,
    )
    .expect("reduced secrets");
    let (x, y, z) = (
        deterministic("walletsync x spend"),
        deterministic("walletsync y spend"),
        deterministic("walletsync z spend"),
    );
    let (primary, sub, z_addr) = (primary_keys.address(), sub_keys.address(), z.address());
    let mut m = Miner::new(state.clone());
    let mut infos: Vec<Info> = Vec::new();

    m.mine(&primary, &[]);
    m.mine(&x.address(), &[]);
    m.mine(&sub, &[]);
    infos.push(client.info().await.expect("info at 3"));
    m.mine(&y.address(), &[]);
    infos.push(client.info().await.expect("info at 4"));

    let (out, global) = input(&client, &primary_keys, &m.coinbases[&1], 1).await;
    let t1 = transfer(
        "t1",
        TxVersion::V1,
        &out,
        global,
        vec![],
        &z_addr,
        1_000_000,
        &primary,
    );
    m.mine(&primary, &[&t1]);

    let (out, global) = input(&client, &x, &m.coinbases[&2], 2).await;
    let t2 = transfer(
        "t2",
        TxVersion::V2,
        &out,
        global,
        vec![],
        &sub,
        500_000,
        &x.address(),
    );
    let (out, global) = input(&client, &y, &m.coinbases[&4], 4).await;
    let t3 = transfer(
        "t3",
        TxVersion::V2,
        &out,
        global,
        vec![],
        &z_addr,
        500_000,
        &y.address(),
    );
    m.mine(&z_addr, &[&t2, &t3]);
    m.mine(&z_addr, &[]);

    let (spent_output, spent_global) = input(&client, &sub_keys, &t2.tx, 6).await;
    let (decoy, decoy_global) = input(&client, &z, &t3.tx, 6).await;
    let t4 = transfer(
        "t4",
        TxVersion::V2,
        &spent_output,
        spent_global,
        vec![(decoy_global, decoy.key)],
        &z_addr,
        200_000,
        &primary,
    );

    // Sent through the client instead of straight into the pool: same pool afterwards.
    let sent = client.send_raw_transaction(&t4.blob).await.expect("sent");
    assert_eq!(sent, SendOutcome::Ok(t4.hash));
    // What a C++ node's bare `{"status":"OK"}` falls back to is the same hash.
    assert_eq!(transaction_hash_from_blob(&t4.blob), t4.hash);
    let again = client.send_raw_transaction(&t4.blob).await.expect("answer");
    assert!(
        matches!(again, SendOutcome::Failed(ref e) if !e.is_empty()),
        "{again:?}"
    );
    let garbage = client
        .send_raw_transaction(&[1, 2, 3])
        .await
        .expect("answer");
    assert!(!garbage.is_ok());

    let unknown = Hash([0x5a; 32]);
    let status = client
        .transactions_status(&[t4.hash, unknown, t1.hash])
        .await
        .expect("status");
    assert!(status.in_pool.contains(&t4.hash));
    assert!(status.in_block.contains(&t1.hash));
    assert!(status.unknown.contains(&unknown));
    assert_eq!(
        status.in_pool.len() + status.in_block.len() + status.unknown.len(),
        3
    );

    let txs = client
        .transactions(&[t4.hash, unknown, t2.hash])
        .await
        .expect("transactions");
    assert_eq!(txs.blobs, vec![t4.blob.clone(), t2.blob.clone()]);
    assert_eq!(txs.missed, vec![unknown]);

    infos.push(client.info().await.expect("info with t4 in the pool"));
    m.mine(&z_addr, &[&t4]);

    let status = client
        .transactions_status(&[t4.hash])
        .await
        .expect("status");
    assert!(status.in_block.contains(&t4.hash));

    // /getinfo at the three points of the vector.
    let recorded_infos: Vec<Info> = fixture["getinfo"]
        .as_array()
        .expect("getinfo")
        .iter()
        .map(|e| recorded(&e["response"]))
        .collect();
    assert_eq!(infos, recorded_infos);
    let versions: Vec<Option<Vec<u8>>> = infos.iter().map(|i| i.tx_versions.clone()).collect();
    assert_eq!(
        versions,
        vec![Some(vec![1]), Some(vec![1, 2]), Some(vec![2])]
    );
    assert_eq!(infos[2].tx_pool_size, 1);

    // /getwalletsyncdata as in the vector.
    let request = WalletSyncRequest {
        checkpoints: Vec::new(),
        start_height: 1,
        start_timestamp: 0,
        block_count: Some(100),
    };
    let blocks = client.wallet_sync_data(&request).await.expect("sync data");
    let expected: Vec<WalletBlock> = recorded(&fixture["getwalletsyncdata"]["response"]["items"]);
    assert_eq!(blocks, expected);
    assert_eq!(blocks.len(), 8);
    for b in &blocks {
        let tagged = b.coinbase.outputs.iter().all(|o| o.view_tag.is_some());
        assert_eq!(tagged, b.height > u64::from(F), "coinbase at {}", b.height);
    }
    let spends: usize = blocks
        .iter()
        .flat_map(|b| &b.transactions)
        .map(|t| t.inputs.len())
        .sum();
    assert_eq!(spends, 4, "t1..t4 have one input each");

    // Resuming after the block at 5: the node starts after the first known checkpoint.
    let resume = WalletSyncRequest {
        checkpoints: vec![blocks[4].hash, blocks[0].hash],
        start_height: 0,
        start_timestamp: 0,
        block_count: None,
    };
    let rest = client.wallet_sync_data(&resume).await.expect("resumed");
    assert_eq!(rest, expected[5..].to_vec());
    // No checkpoint on the main chain.
    let lost = WalletSyncRequest {
        checkpoints: vec![unknown],
        ..WalletSyncRequest::default()
    };
    let e = client.wallet_sync_data(&lost).await.unwrap_err();
    assert!(
        matches!(e, DaemonError::Status(ref s) if s == "Failed to perform query"),
        "{e}"
    );

    // /get_global_indexes_for_range as in the vector.
    let indexes = client
        .global_indexes_for_range(1, 9)
        .await
        .expect("indexes");
    let expected_indexes: HashMap<Hash, Vec<u64>> =
        fixture["get_global_indexes_for_range"]["response"]["indexes"]
            .as_array()
            .expect("indexes")
            .iter()
            .map(|e| {
                let hash: Hash = e["key"].as_str().expect("key").parse().expect("hash");
                (hash, recorded::<Vec<u64>>(&e["value"]))
            })
            .collect();
    assert_eq!(indexes, expected_indexes);

    // Ring members: the testnet unlocks outputs one block deep, so the node answers with the
    // outputs that exist, fewer than asked for when an amount has few; too many amounts fail.
    let rings = client
        .random_outs(&[500_000, 1], 3)
        .await
        .expect("random outs");
    assert_eq!(rings.len(), 2);
    assert_eq!(rings[0].amount, 500_000);
    assert_eq!(rings[1].amount, 1);
    assert!(
        rings
            .iter()
            .all(|r| !r.outs.is_empty() && r.outs.len() <= 3)
    );
    assert!(rings.iter().all(|r| {
        r.outs
            .windows(2)
            .all(|w| w[0].global_amount_index < w[1].global_amount_index)
    }));
    let e = client.random_outs(&[1; 1_001], 1).await.unwrap_err();
    assert!(
        matches!(e, DaemonError::Status(ref s) if s == "Failed"),
        "{e}"
    );

    headers_and_mining(&client, &blocks, &primary).await;
    errors(&client).await;
}

/// Header calls against the sync data, then a template and refused submissions.
async fn headers_and_mining(client: &Client, blocks: &[WalletBlock], miner: &Address) {
    assert_eq!(client.block_count().await.expect("count"), 9);
    let top = client.last_block_header().await.expect("top");
    assert_eq!(top.height, 8);
    assert_eq!(top.hash, blocks[7].hash);
    assert_eq!(top.depth, 0);
    assert_eq!(top.timestamp, blocks[7].timestamp);
    assert_eq!(top.major_version, 7);
    assert_eq!(top.num_txes, 2);

    let five = client.block_header_by_height(5).await.expect("header 5");
    assert_eq!(five.hash, blocks[4].hash);
    assert_eq!(five.prev_hash, blocks[3].hash);
    assert_eq!(five.depth, 3);
    let by_hash = client
        .block_header_by_hash(&blocks[4].hash)
        .await
        .expect("by hash");
    assert_eq!(by_hash, five);

    let range = client.block_headers_range(1, 8).await.expect("range");
    let hashes: Vec<Hash> = range.iter().map(|h| h.hash).collect();
    let synced: Vec<Hash> = blocks.iter().map(|b| b.hash).collect();
    assert_eq!(hashes, synced);
    assert!(range.iter().zip(1..).all(|(h, n)| h.height == n));

    let template = client.block_template(miner, 8).await.expect("template");
    assert_eq!(template.height, 9);
    assert!(template.difficulty > 0);
    assert!(template.reserved_offset > 0);
    assert!(template.reserved_offset as usize + 8 <= template.blob.len());

    let e = client.submit_block(&[0xde, 0xad]).await.unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::BLOCK_NOT_ACCEPTED), "{e}");
    // An unmined template does not meet its difficulty... unless the difficulty is 1.
    if template.difficulty > 1 {
        let e = client.submit_block(&template.blob).await.unwrap_err();
        assert_eq!(e.rpc_code(), Some(codes::BLOCK_NOT_ACCEPTED), "{e}");
    }
}

/// How failures of the node come out of the client.
async fn errors(client: &Client) {
    let e = client.block_header_by_height(100).await.unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::TOO_BIG_HEIGHT), "{e}");
    let e = client.block_headers_range(5, 2).await.unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::TOO_BIG_HEIGHT), "{e}");
    let e = client
        .json_rpc("no_such_method", json!({}))
        .await
        .unwrap_err();
    assert_eq!(e.rpc_code(), Some(codes::METHOD_NOT_FOUND), "{e}");
    let count = client
        .json_rpc("getblockcount", Value::Null)
        .await
        .expect("count");
    assert_eq!(count["count"], 9);

    // A request the node cannot parse: HTTP 400 with the node's status text.
    let e = client
        .post("/getwalletsyncdata", &json!({ "startHeight": "one" }))
        .await
        .unwrap_err();
    match e {
        DaemonError::Http {
            status,
            ref message,
        } => {
            assert_eq!(status, 400);
            assert!(message.starts_with("Failed to parse request"), "{message}");
        }
        other => panic!("not an HTTP error: {other}"),
    }
    // An unknown route: 404 with an empty body.
    let e = client.get("/no_such_route").await.unwrap_err();
    assert!(matches!(e, DaemonError::Http { status: 404, .. }), "{e}");
    assert!(!e.is_unreachable());
}
