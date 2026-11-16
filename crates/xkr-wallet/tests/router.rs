//! The wallet against xkrd's RPC router in process, on the fork-14 test chain of vector E
//! (`crates/xkr-rpc/tests/tx_v2_walletsync/node.rs`: fork 14 at height 4, three blocks of
//! grace). The node validates everything the wallet sends: version 1 while `/info` lists `[1]`,
//! version 2 once it lists `[1, 2]` and `[2]`.

#[path = "../../xkr-wallet-core/tests/vectors/mod.rs"]
mod vectors;

#[allow(dead_code)] // the vector test's node; its recording helpers are not used here
#[path = "../../xkr-rpc/tests/tx_v2_walletsync/node.rs"]
mod node;

mod common;

use common::{RouterTransport, config, wait_for_block, wallet_path};
use node::{Miner, TempDb, node};
use vectors::secret;
use xkr_consensus::Network;
use xkr_daemon_client::DaemonClient;
use xkr_serialization::decode_transaction;
use xkr_types::{Hash, SecretKey};
use xkr_wallet::{Balance, FileFormat, Node, SendRequest, Wallet};
use xkr_wallet_core::{BuiltTransaction, WalletKeys, scan_transaction};

fn deterministic(label: &str) -> WalletKeys {
    WalletKeys::from_spend_secret(&secret(label)).expect("reduced secret")
}

/// The pool transaction `hash`, as the miner wants it.
async fn from_pool(client: &DaemonClient<RouterTransport>, hash: Hash) -> BuiltTransaction {
    let answer = client.transactions(&[hash]).await.expect("gettransactions");
    let blob = answer.blobs.first().expect("in the pool").clone();
    let tx = decode_transaction(&blob).expect("decodes");
    BuiltTransaction {
        fee: 10,
        tx,
        blob,
        hash,
        tx_secret_key: SecretKey::ZERO,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syncs_and_sends_version_1_then_2_through_xkrd() {
    let db = TempDb::create();
    let state = node(&db);
    let client = DaemonClient::new(RouterTransport(xkr_rpc::router(state.clone(), &[])));
    let me = deterministic("xkr-wallet router wallet");
    let other = deterministic("xkr-wallet router other");
    let mut m = Miner::new(state.clone());
    m.mine(&me.address(), &[]); // 1
    m.mine(&me.address(), &[]); // 2
    m.mine(&other.address(), &[]); // 3

    let dir = tempfile::tempdir().unwrap();
    let w = Wallet::import_keys(
        wallet_path(dir.path(), "router.wallet"),
        "pw",
        me.spend_secret,
        me.view_secret,
        0,
        Node::new(client.clone(), "router", 0),
        config(Network::Testnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    wait_for_block(&w, m.tip).await;
    let mined: u64 = [1u64, 2]
        .iter()
        .map(|h| m.coinbases[h].prefix.outputs[0].amount)
        .sum();
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: mined,
            locked: 0
        }
    );
    assert_eq!(w.transactions(None, None).unwrap().len(), 2);

    let to = other.address().to_string();
    let request = SendRequest {
        mixin: Some(0),
        ..SendRequest::basic(&to, 100_000, "")
    };
    let mut sent = 0u64;
    // (blocks to mine first, version expected): /info lists [1] at height 3, [1, 2] from the
    // fork at 4, [2] after the grace period
    for (blocks_before, want) in [(0, 1u8), (0, 2), (3, 2)] {
        for _ in 0..blocks_before {
            m.mine(&other.address(), &[]);
        }
        wait_for_block(&w, m.tip).await;
        let hash = w.send_advanced(request.clone()).await.unwrap();
        let tx = from_pool(&client, hash).await;
        assert_eq!(
            tx.tx.prefix.version,
            want,
            "at height {}",
            w.status().wallet_block_count
        );
        let paid: u64 = scan_transaction(&other, &tx.tx.prefix)
            .iter()
            .map(|f| f.amount)
            .sum();
        assert_eq!(paid, 100_000);
        sent += 100_010;
        // the node accepts the transaction into a block
        m.mine(&other.address(), &[&tx]);
        wait_for_block(&w, m.tip).await;
        assert_eq!(w.unconfirmed(None).unwrap().len(), 0);
        let confirmed = w.transactions(None, None).unwrap();
        let record = confirmed
            .iter()
            .find(|t| t.hash == hash)
            .expect("confirmed");
        assert_eq!(record.total_amount(), -100_010);
        assert_eq!(record.fee, 10);
        assert_eq!(
            w.balance(None).unwrap(),
            Balance {
                unlocked: mined - sent,
                locked: 0
            }
        );
    }
    assert_eq!(w.status().wallet_block_count, 9);
    w.close().await.unwrap();
}
