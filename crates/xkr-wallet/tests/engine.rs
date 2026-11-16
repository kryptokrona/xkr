//! The wallet engine against the mock daemon (`tests/common`): sync and balances, spends found
//! through key images, rollback, cancelled sends, locked outputs, view wallets, subwallets,
//! the node fee, the transaction version, fusion, delayed transactions, block hashes and
//! `extra`.

mod common;

use std::collections::HashSet;
use std::time::Duration;

use common::{Mock, config, hash_of, keys, payment, wait_for_block, wait_until, wallet_path};
use xkr_consensus::Network;
use xkr_types::{Hash, KeyImage, Transaction, TxInput, TxOutputTarget};
use xkr_wallet::{
    Balance, ErrorCode, FileFormat, FusionRequest, Node, SendRequest, TxState, Wallet, WalletEvent,
};
use xkr_wallet_core::{WalletKeys, scan_transaction};

/// A wallet for `keys` on `mock`, syncing from height 0.
async fn wallet(mock: &Mock, dir: &tempfile::TempDir, keys: &WalletKeys) -> Wallet<Mock> {
    Wallet::import_keys(
        wallet_path(dir.path(), "w.wallet"),
        "pw",
        keys.spend_secret,
        keys.view_secret,
        0,
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .expect("wallet")
}

/// The amount `keys` receives in `tx`.
fn received(keys: &WalletKeys, tx: &Transaction) -> u64 {
    scan_transaction(keys, &tx.prefix)
        .iter()
        .map(|f| f.amount)
        .sum()
}

fn key_images(tx: &Transaction) -> Vec<KeyImage> {
    tx.prefix
        .inputs
        .iter()
        .filter_map(|i| match i {
            TxInput::Key { key_image, .. } => Some(*key_image),
            TxInput::Coinbase { .. } => None,
        })
        .collect()
}

fn ring_sizes(tx: &Transaction) -> Vec<usize> {
    tx.prefix
        .inputs
        .iter()
        .filter_map(|i| match i {
            TxInput::Key { output_indexes, .. } => Some(output_indexes.len()),
            TxInput::Coinbase { .. } => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn syncs_transactions_and_balances() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let cb = mock.mine_to(&me.address(), 5_000_000, 0);
    let pid = Hash([0xab; 32]);
    let pay = payment(&me.address(), &[1_000_000, 20_000], Some(pid), 0);
    let pay_hash = mock.push(pay.clone());
    mock.mine();
    let tip = mock.mine();

    let w = Wallet::import_keys(
        wallet_path(dir.path(), "w.wallet"),
        "pw",
        me.spend_secret,
        me.view_secret,
        0,
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    let mut events = w.subscribe();
    w.start_sync();
    wait_for_block(&w, tip).await;

    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 6_020_000,
            locked: 0
        }
    );
    let txs = w.transactions(None, None).unwrap();
    assert_eq!(txs.len(), 2);
    assert!(txs[0].is_coinbase);
    assert_eq!(txs[0].block_height, 1);
    assert_ne!(txs[0].hash, Hash::ZERO);
    assert_eq!(txs[1].hash, pay_hash);
    assert_eq!(txs[1].payment_id, pid.to_hex());
    assert_eq!(txs[1].total_amount(), 1_020_000);
    assert_eq!(txs[1].fee, 10);
    assert_eq!(txs[1].block_height, 2);
    assert_eq!(w.transactions(Some(2..3), None).unwrap().len(), 1);
    assert_eq!(
        w.transactions(None, Some(&w.primary_address()))
            .unwrap()
            .len(),
        2
    );
    let status = w.status();
    assert_eq!(status.wallet_block_count, 3);
    assert_eq!(status.network_block_count, 3);
    assert_eq!(status.peer_count, 10);
    assert_eq!(status.hashrate, 100);
    assert!(status.daemon_online);
    assert_eq!(w.subwallet_count(), 1);
    assert!(!w.is_view_wallet());

    let mut seen = HashSet::new();
    while let Ok(event) = events.try_recv() {
        if let WalletEvent::Transaction(tx) = event {
            seen.insert(tx.hash);
        }
    }
    assert!(seen.contains(&pay_hash));
    assert_eq!(seen.len(), 2, "the coinbase of block {cb} and the payment");

    // `extra` of an incoming transaction comes from the node.
    let extra = w.transaction_extra(&pay_hash).await.unwrap();
    assert_eq!(extra, pay.prefix.extra);
    assert_eq!(
        w.transaction_extra(&Hash([1; 32]))
            .await
            .unwrap_err()
            .walletd_code(),
        Some(23)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_coinbase_and_unlock_times() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    // A coinbase locked for 20 blocks, a payment locked until a far timestamp.
    mock.mine_to(&me.address(), 700_000, 1 + 20);
    mock.push(payment(&me.address(), &[300_000], None, 4_000_000_000));
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 0,
            locked: 1_000_000
        }
    );
    // `isInputUnlocked`: unlocked once height + 1 >= unlock time.
    let mut tip = tip;
    for _ in 0..18 {
        tip = mock.mine();
    }
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 700_000);
    assert_eq!(w.balance(None).unwrap().locked, 300_000);
    let sent = w
        .send_basic(&keys().address().to_string(), 800_000, "")
        .await;
    assert_eq!(sent.unwrap_err().code(), ErrorCode::NotEnoughBalance);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_confirms_and_spends() {
    let mock = Mock::new();
    let me = keys();
    let them = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;

    let hash = w
        .send_basic(&them.address().to_string(), 123_456, "")
        .await
        .unwrap();
    let tx = mock.pool_tx(&hash).expect("relayed");
    assert_eq!(hash_of(&tx), hash);
    assert_eq!(tx.prefix.version, 1);
    // default mixin 3 at this height: rings of 4, own output once, repeated decoys dropped
    assert_eq!(ring_sizes(&tx), vec![4]);
    assert_eq!(received(&them, &tx), 123_456);
    assert_eq!(received(&me, &tx), 1_000_000 - 123_456 - 10);
    assert!(
        tx.prefix
            .outputs
            .iter()
            .all(|o| xkr_wallet_core::select::pretty_amount_power(o.amount).is_some())
    );

    // pending: the input is locked, the change counts as locked
    let pending = w.unconfirmed(None).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].total_amount(), -(123_456 + 10));
    assert_eq!(pending[0].block_height, 0);
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 0,
            locked: 1_000_000 - 123_456 - 10
        }
    );
    assert_eq!(w.transaction(&hash).unwrap().state, TxState::Succeeded);
    assert!(w.tx_private_key(&hash).is_ok());
    assert_eq!(
        w.tx_private_key(&Hash([7; 32])).unwrap_err().code(),
        ErrorCode::TxPrivateKeyNotFound
    );
    assert_eq!(w.transaction_extra(&hash).await.unwrap(), tx.prefix.extra);

    // confirmed: the input is spent (found through its key image), the change unlocked
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(w.unconfirmed(None).unwrap().len(), 0);
    let confirmed: Vec<_> = w
        .transactions(None, None)
        .unwrap()
        .into_iter()
        .filter(|t| t.hash == hash)
        .collect();
    assert_eq!(confirmed.len(), 1);
    assert_eq!(confirmed[0].block_height, 3);
    assert_eq!(confirmed[0].total_amount(), -(123_456 + 10));
    assert_eq!(confirmed[0].fee, 10);
    assert_eq!(
        confirmed[0].destinations.len(),
        2,
        "destination and change, kept from the pending record"
    );
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 1_000_000 - 123_456 - 10,
            locked: 0
        }
    );
    assert_eq!(key_images(&tx).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_checks_follow_cpp() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let to = keys().address().to_string();
    let send = |request: SendRequest| {
        let w = &w;
        async move { w.send_advanced(request).await.unwrap_err().code() }
    };
    assert_eq!(
        send(SendRequest::default()).await,
        ErrorCode::NoDestinationsGiven
    );
    assert_eq!(
        send(SendRequest::basic(&to, 0, "")).await,
        ErrorCode::AmountIsZero
    );
    assert_eq!(
        send(SendRequest::basic("SEKRbad", 1, "")).await,
        ErrorCode::AddressWrongLength
    );
    let low_fee = SendRequest {
        fee: Some(1),
        ..SendRequest::basic(&to, 1_000, "")
    };
    assert_eq!(send(low_fee).await, ErrorCode::FeeTooSmall);
    assert_eq!(
        send(SendRequest::basic(&to, 2_000_000, "")).await,
        ErrorCode::NotEnoughBalance
    );
    assert_eq!(
        send(SendRequest::basic(&to, 1_000, "abc")).await,
        ErrorCode::PaymentIdWrongLength
    );
    let integrated =
        xkr_wallet::create_integrated_address(&to, &"ab".repeat(32)).expect("integrated");
    assert_eq!(
        send(SendRequest::basic(&integrated, 1_000, &"cd".repeat(32))).await,
        ErrorCode::ConflictingPaymentIds
    );
    let change_integrated = SendRequest {
        change_address: Some(
            xkr_wallet::create_integrated_address(&w.primary_address(), &"ab".repeat(32)).unwrap(),
        ),
        ..SendRequest::basic(&to, 1_000, "")
    };
    assert_eq!(
        send(change_integrated).await,
        ErrorCode::AddressIsIntegrated
    );
    let foreign_source = SendRequest {
        source_addresses: vec![to.clone()],
        ..SendRequest::basic(&to, 1_000, "")
    };
    assert_eq!(send(foreign_source).await, ErrorCode::AddressNotInWallet);

    // Too few ring members from the node.
    mock.lock().decoys = Some(1);
    assert_eq!(
        send(SendRequest::basic(&to, 1_000, "")).await,
        ErrorCode::NotEnoughFakeOutputs
    );
    mock.lock().decoys = None;
    // The node refuses: DAEMON_ERROR, the input stays spendable.
    mock.lock().refuse = Some("nope".into());
    let e = w.send_basic(&to, 1_000, "").await.unwrap_err();
    assert_eq!(
        (e.code(), e.detail()),
        (ErrorCode::DaemonError, Some("nope"))
    );
    mock.lock().refuse = None;
    // The node is gone: DAEMON_OFFLINE.
    mock.lock().offline = true;
    assert_eq!(
        send(SendRequest::basic(&to, 1_000, "")).await,
        ErrorCode::DaemonOffline
    );
    mock.lock().offline = false;
    // An integrated destination carries its payment id.
    let hash = w.send_basic(&integrated, 1_000, "").await.unwrap();
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(
        xkr_types::payment_id(&tx.prefix.extra),
        Some(Hash([0xab; 32]))
    );
    assert_eq!(w.unconfirmed(None).unwrap()[0].payment_id, "ab".repeat(32));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_send_unlocks_its_inputs() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    let mut events = w.subscribe();
    wait_for_block(&w, tip).await;
    let hash = w
        .send_basic(&keys().address().to_string(), 10_000, "")
        .await
        .unwrap();
    assert_eq!(w.balance(None).unwrap().unlocked, 0);
    mock.forget(hash);
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    wait_until("the cancelled transaction", || {
        w.unconfirmed(None).unwrap().is_empty()
    })
    .await;
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 1_000_000,
            locked: 0
        }
    );
    assert_eq!(w.transaction(&hash).unwrap().state, TxState::Cancelled);
    let mut cancelled = false;
    while let Ok(event) = events.try_recv() {
        cancelled |= event == WalletEvent::Cancelled(hash);
    }
    assert!(cancelled);
    // the inputs can be spent again
    w.send_basic(&keys().address().to_string(), 10_000, "")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reorg_rolls_back() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0); // 1
    mock.mine(); // 2
    let w = wallet(&mock, &dir, &me).await;
    let mut events = w.subscribe();
    wait_for_block(&w, mock.tip()).await;
    // spent at 3
    let hash = w
        .send_basic(&keys().address().to_string(), 10_000, "")
        .await
        .unwrap();
    mock.mine(); // 3, with the transfer
    mock.mine_to(&me.address(), 50_000, 0); // 4
    let tip = mock.mine(); // 5
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(None).unwrap().unlocked,
        1_000_000 - 10_010 + 50_000
    );

    // The node switches to a chain without blocks 3 and 4.
    mock.rewind_to(3);
    mock.mine();
    mock.mine();
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(w.status().wallet_block_count, 5);
    // the spend and the coinbase at 4 are undone; the spent output is unspent again
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 1_000_000,
            locked: 0
        }
    );
    assert!(
        w.transactions(None, None)
            .unwrap()
            .iter()
            .all(|t| t.hash != hash)
    );
    let mut rollback = None;
    while let Ok(event) = events.try_recv() {
        if let WalletEvent::Rollback { height } = event {
            rollback = Some(height);
        }
    }
    assert_eq!(rollback, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_wallet_sees_incoming_only() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.push(payment(&me.address(), &[5_000], None, 0));
    let tip = mock.mine();
    let w = Wallet::import_view(
        wallet_path(dir.path(), "view.wallet"),
        "pw",
        me.view_secret,
        &me.address().to_string(),
        0,
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    wait_for_block(&w, tip).await;
    assert!(w.is_view_wallet());
    assert_eq!(w.balance(None).unwrap().unlocked, 1_005_000);
    assert_eq!(w.transactions(None, None).unwrap().len(), 2);
    let to = keys().address().to_string();
    assert_eq!(
        w.send_basic(&to, 1, "").await.unwrap_err().code(),
        ErrorCode::IllegalViewWalletOperation
    );
    assert_eq!(
        w.fusion_basic().await.unwrap_err().code(),
        ErrorCode::IllegalViewWalletOperation
    );
    assert_eq!(
        w.mnemonic(&w.primary_address()).unwrap_err().code(),
        ErrorCode::IllegalViewWalletOperation
    );
    assert_eq!(
        w.add_subwallet().await.unwrap_err().code(),
        ErrorCode::IllegalViewWalletOperation
    );
    assert_eq!(w.keys(), me.view_secret);
    // a view subwallet for another spend key
    let other = WalletKeys::from_secrets(&keys().spend_secret, &me.view_secret).unwrap();
    let address = w
        .import_view_subwallet(other.spend_public, 0)
        .await
        .unwrap();
    assert_eq!(address, other.address().to_string());
    wait_for_block(&w, tip).await;
    assert_eq!(w.subwallet_count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subwallets_sources_and_change() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let w = wallet(&mock, &dir, &me).await;
    let primary = w.primary_address();
    let (sub, sub_secret) = w.add_subwallet().await.unwrap();
    assert_ne!(sub, primary);
    let sub_address = xkr_wallet_core::Address::parse(&sub).unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.mine_to(&sub_address, 2_000_000, 0);
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    let balances = w.balances();
    assert_eq!(balances.len(), 2);
    assert_eq!(
        (balances[0].address.as_str(), balances[0].unlocked),
        (primary.as_str(), 1_000_000)
    );
    assert_eq!(
        (balances[1].address.as_str(), balances[1].unlocked),
        (sub.as_str(), 2_000_000)
    );
    assert_eq!(w.balance(Some(&sub)).unwrap().unlocked, 2_000_000);
    assert_eq!(
        w.balance(Some(&keys().address().to_string()))
            .unwrap_err()
            .code(),
        ErrorCode::AddressNotInWallet
    );
    assert_eq!(w.spend_keys(&sub).unwrap().1, sub_secret);
    // a random subwallet has no seed; the primary of a deterministic wallet has one
    assert_eq!(
        w.mnemonic(&sub).unwrap_err().code(),
        ErrorCode::KeysNotDeterministic
    );
    assert_eq!(
        xkr_wallet_core::secret_from_mnemonic(&w.mnemonic(&primary).unwrap()).unwrap(),
        me.spend_secret
    );

    // take from the subwallet only, change back to it
    let request = SendRequest {
        source_addresses: vec![sub.clone()],
        change_address: Some(sub.clone()),
        mixin: Some(0),
        ..SendRequest::basic(&keys().address().to_string(), 500_000, "")
    };
    let hash = w.send_advanced(request).await.unwrap();
    let pending = w.unconfirmed(Some(&sub)).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].transfers.len(), 1);
    assert_eq!(pending[0].transfers[0].0, sub_address.spend);
    assert_eq!(pending[0].transfers[0].1, -(500_000 + 10));
    assert_eq!(w.unconfirmed(Some(&primary)).unwrap().len(), 0);
    assert_eq!(w.balance(Some(&primary)).unwrap().unlocked, 1_000_000);
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(ring_sizes(&tx), vec![1]);
    let sub_keys = WalletKeys::from_secrets(&sub_secret, &me.view_secret).unwrap();
    assert_eq!(received(&sub_keys, &tx), 2_000_000 - 500_010);

    // deleting: not the primary address
    assert_eq!(
        w.delete_subwallet(&primary).await.unwrap_err().code(),
        ErrorCode::CannotDeletePrimaryAddress
    );
    w.delete_subwallet(&sub).await.unwrap();
    assert_eq!(w.addresses(), vec![primary.clone()]);
    assert_eq!(w.unconfirmed(None).unwrap().len(), 0);
    // importing it again rescans from the given height
    let again = w.import_subwallet(sub_secret, 0).await.unwrap();
    assert_eq!(again, sub);
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(Some(&sub)).unwrap().unlocked, 2_000_000 - 500_010);
    assert_eq!(
        w.import_subwallet(sub_secret, 0).await.unwrap_err().code(),
        ErrorCode::SubwalletAlreadyExists
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_fee_is_paid_as_a_destination() {
    let mock = Mock::new();
    let me = keys();
    let operator = keys();
    mock.lock().fee = (operator.address().to_string(), 5_000);
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let node = w.node();
    assert_eq!(
        (node.fee_amount, node.fee_address),
        (5_000, operator.address().to_string())
    );
    assert_eq!((node.host.as_str(), node.port), ("mock", 11898));
    let hash = w
        .send_basic(&keys().address().to_string(), 100_000, "")
        .await
        .unwrap();
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(received(&operator, &tx), 5_000);
    assert_eq!(received(&me, &tx), 1_000_000 - 100_000 - 5_000 - 10);
    assert_eq!(
        w.unconfirmed(None).unwrap()[0].total_amount(),
        -(100_000 + 5_000 + 10)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transaction_version_follows_the_daemon() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..5 {
        mock.mine_to(&me.address(), 1_000_000, 0);
    }
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let cases: [(Option<Vec<u8>>, u8, u8); 5] = [
        (Some(vec![1]), 7, 1),
        (Some(vec![1, 2]), 7, 2),
        (Some(vec![2]), 7, 2),
        (None, 6, 1),
        (None, 7, 2),
    ];
    for (versions, major, want) in cases {
        {
            let mut st = mock.lock();
            st.tx_versions.clone_from(&versions);
            st.major_version = major;
        }
        let hash = w
            .send_basic(&keys().address().to_string(), 100_000, "")
            .await
            .unwrap();
        let tx = mock.pool_tx(&hash).unwrap();
        assert_eq!(
            tx.prefix.version, want,
            "tx_versions {versions:?}, major {major}"
        );
        let tagged = tx
            .prefix
            .outputs
            .iter()
            .all(|o| matches!(o.target, TxOutputTarget::TaggedKey { .. }));
        assert_eq!(tagged, want == 2);
        assert_eq!(tx.clsag_signatures.len(), usize::from(want == 2));
    }
    // the version-2 change comes back with view tags and is found
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(None).unwrap().unlocked,
        5 * 1_000_000 - 5 * 100_010
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fusion_and_estimate() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let w = wallet(&mock, &dir, &me).await;
    // three outputs: nothing to fuse
    for _ in 0..3 {
        mock.mine_to(&me.address(), 1_000, 0);
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.fusion_basic().await.unwrap_err().code(),
        ErrorCode::FullyOptimized
    );
    for _ in 0..12 {
        mock.mine_to(&me.address(), 1_000, 0);
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    let estimate = w.estimate_fusion(10_000, &[]).unwrap();
    assert_eq!(
        (estimate.fusion_ready_count, estimate.total_output_count),
        (15, 15)
    );
    assert_eq!(w.estimate_fusion(1_000, &[]).unwrap().fusion_ready_count, 0);

    let hash = w.fusion_basic().await.unwrap();
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(key_images(&tx).len(), 15);
    let outputs: Vec<u64> = tx.prefix.outputs.iter().map(|o| o.amount).collect();
    assert_eq!(outputs, vec![5_000, 10_000]);
    assert_eq!(received(&me, &tx), 15_000);
    let pending = w.unconfirmed(None).unwrap();
    assert!(pending[0].is_fusion());
    assert_eq!(w.balance(None).unwrap().locked, 15_000);
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 15_000,
            locked: 0
        }
    );

    // walletd's variant with a threshold
    for _ in 0..12 {
        mock.mine_to(&me.address(), 300, 0);
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    let request = FusionRequest {
        threshold: Some(1_000),
        mixin: Some(0),
        ..FusionRequest::default()
    };
    let hash = w.fusion_advanced(request).await.unwrap();
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(key_images(&tx).len(), 12);
    assert_eq!(ring_sizes(&tx), vec![1; 12]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_transactions() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let to = keys().address().to_string();

    let hash = w
        .delayed_create(SendRequest::basic(&to, 1_000, ""))
        .await
        .unwrap();
    assert_eq!(w.delayed_list(), vec![hash]);
    assert!(mock.pool_tx(&hash).is_none());
    assert_eq!(w.transaction(&hash).unwrap().state, TxState::Created);
    assert_ne!(w.transaction_extra(&hash).await.unwrap().len(), 0);
    // its input is set aside
    assert_eq!(
        w.send_basic(&to, 1_000, "").await.unwrap_err().code(),
        ErrorCode::NotEnoughBalance
    );
    w.delayed_delete(&hash).unwrap();
    assert_eq!(w.delayed_list().len(), 0);
    assert_eq!(
        w.delayed_delete(&hash).unwrap_err().walletd_code(),
        Some(23)
    );

    let hash = w
        .delayed_create(SendRequest::basic(&to, 2_000, ""))
        .await
        .unwrap();
    assert_eq!(w.delayed_send(&hash).await.unwrap(), hash);
    assert!(mock.pool_tx(&hash).is_some());
    assert_eq!(w.delayed_list().len(), 0);
    assert_eq!(w.unconfirmed(None).unwrap().len(), 1);
    assert!(w.delayed_send(&hash).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn block_hashes_from_progress_and_node() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let mut tip = mock.tip();
    for _ in 0..130 {
        tip = mock.mine();
    }
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let chain: Vec<Hash> = mock.lock().blocks.iter().map(|b| b.hash).collect();
    // block 0..5 are older than the last 100 hashes: asked from the node
    assert_eq!(w.block_hashes(0, 5).await.unwrap(), chain[0..5].to_vec());
    assert_eq!(
        w.block_hashes(120, 100).await.unwrap(),
        chain[120..].to_vec()
    );
    assert_eq!(w.block_hashes(500, 10).await.unwrap().len(), 0);
    assert_eq!(w.block_height_of(&chain[7]).await.unwrap(), Some(7));
    assert_eq!(w.block_height_of(&Hash([3; 32])).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_and_swap_node() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.mine();
    mock.mine_to(&me.address(), 2_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 3_000_000);
    w.reset(3).await.unwrap();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 2_000_000);
    assert_eq!(w.transactions(None, None).unwrap().len(), 1);

    // the same chain behind another node: sync continues
    let other = common::Mock(std::sync::Arc::clone(&mock.0));
    w.swap_node(Node::new(
        xkr_daemon_client::DaemonClient::new(other),
        "other",
        1,
    ))
    .await;
    assert_eq!(w.node().host, "other");
    let tip = mock.mine_to(&me.address(), 5, 0);
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 2_000_005);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(w.status().daemon_online);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delayed_and_cancelled_transactions_are_saved() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let to = keys().address().to_string();
    let delayed = w
        .delayed_create(SendRequest::basic(&to, 1_000, ""))
        .await
        .unwrap();
    let sent = w.send_basic(&to, 2_000, "").await.unwrap();
    mock.forget(sent);
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    wait_until("the cancelled transaction", || {
        w.unconfirmed(None).unwrap().is_empty()
    })
    .await;
    w.close().await.unwrap();

    let w = Wallet::open(
        wallet_path(dir.path(), "w.wallet"),
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.delayed_list(), vec![delayed]);
    assert_eq!(w.transaction(&delayed).unwrap().state, TxState::Created);
    assert_eq!(w.transaction(&sent).unwrap().state, TxState::Cancelled);
    assert_eq!(w.cancelled(None).unwrap().len(), 1);
    // the delayed transaction still holds its input
    assert_eq!(
        w.send_basic(&to, 1_500_000, "").await.unwrap_err().code(),
        ErrorCode::NotEnoughBalance
    );
    assert_eq!(w.delayed_send(&delayed).await.unwrap(), delayed);
    assert!(mock.pool_tx(&delayed).is_some());
    assert_eq!(w.delayed_list().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_from_a_timestamp() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.mine();
    mock.mine_to(&me.address(), 2_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 3_000_000);
    let now = xkr_wallet::state::unix_now();
    let timestamp = xkr_wallet::state::scan_height_to_timestamp(3, now);
    w.reset_from(0, timestamp).await.unwrap();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 2_000_000);
    // no timestamp: the height
    w.reset_from(1, 0).await.unwrap();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 3_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_fee_goes_first_for_walletd() {
    let mock = Mock::new();
    let me = keys();
    let operator = keys().address().to_string();
    mock.lock().fee = (operator.clone(), 5_000);
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let to = keys().address().to_string();
    let listed = |hash: &Hash| -> Vec<String> {
        w.transaction(hash)
            .unwrap()
            .transaction
            .destinations
            .iter()
            .map(|d| d.address.clone())
            .collect()
    };
    // WalletBackend: after the destinations, before the change
    let hash = w.send_basic(&to, 100_000, "").await.unwrap();
    assert_eq!(&listed(&hash)[..2], &[to.clone(), operator.clone()]);
    // walletd: first
    w.set_node_fee_first(true);
    let hash = w.send_basic(&to, 100_000, "").await.unwrap();
    assert_eq!(&listed(&hash)[..2], &[operator.clone(), to.clone()]);
    let tx = mock.pool_tx(&hash).unwrap();
    assert_eq!(received(&me, &tx), 1_000_000 - 100_000 - 5_000 - 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_list_rescans_once() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let (a_public, a_secret) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
    let (b_public, b_secret) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
    let sub = |public| xkr_wallet_core::Address::new(public, me.view_public);
    mock.mine_to(&sub(a_public), 300, 0);
    mock.mine_to(&sub(b_public), 400, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let mut events = w.subscribe();
    // a bad key in the list: nothing is added
    assert_eq!(
        w.import_subwallets(&[a_secret, a_secret], 0)
            .await
            .unwrap_err()
            .code(),
        ErrorCode::SubwalletAlreadyExists
    );
    assert_eq!(w.subwallet_count(), 1);
    let addresses = w.import_subwallets(&[a_secret, b_secret], 0).await.unwrap();
    assert_eq!(
        addresses,
        vec![sub(a_public).to_string(), sub(b_public).to_string()]
    );
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(Some(addresses[0].as_str())).unwrap().unlocked,
        300
    );
    assert_eq!(
        w.balance(Some(addresses[1].as_str())).unwrap().unlocked,
        400
    );
    let mut resets = 0;
    while let Ok(event) = events.try_recv() {
        resets += usize::from(matches!(event, WalletEvent::Reset { .. }));
    }
    assert_eq!(resets, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_change_follows_the_cpp_spendable_rules() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let to = keys().address().to_string();
    w.send_basic(&to, 100_000, "").await.unwrap();
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    // `WalletBackend`: the change of the last block is spendable (only the unlock time counts)
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 899_990,
            locked: 0
        }
    );
    // ... and taken: everything goes, from outputs of the last block
    w.send_basic(&to, 899_980, "").await.unwrap();
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap(), Balance::default());
    // walletd (`transactionSoftLockTime` 1): an output of the last processed block waits
    w.set_spendable_age(1);
    let tip = mock.mine_to(&me.address(), 500_000, 0);
    wait_for_block(&w, tip).await;
    assert_eq!(
        w.balance(None).unwrap(),
        Balance {
            unlocked: 0,
            locked: 500_000
        }
    );
    assert_eq!(
        w.send_basic(&to, 100_000, "").await.unwrap_err().code(),
        ErrorCode::NotEnoughBalance
    );
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 500_000);
    w.send_basic(&to, 100_000, "").await.unwrap();
}

/// Mines a block every 10 ms until the returned sender is dropped or sent to.
fn keep_mining(mock: &Mock) -> tokio::sync::oneshot::Sender<()> {
    let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
    let miner = common::Mock(std::sync::Arc::clone(&mock.0));
    tokio::spawn(async move {
        loop {
            miner.mine();
            tokio::select! {
                _ = &mut stopped => return,
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    });
    stop
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fusion_returns_while_the_chain_grows() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    // outputs below the fusion dust threshold (10 at these heights) never make a fusion
    for _ in 0..40 {
        mock.mine_to(&me.address(), 5, 0);
    }
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let stop = keep_mining(&mock);
    let answer = tokio::time::timeout(Duration::from_secs(30), w.fusion_basic())
        .await
        .expect("fusion returns");
    assert_eq!(answer.unwrap_err().code(), ErrorCode::FullyOptimized);
    drop(stop);

    let mock = Mock::new();
    let dir = tempfile::tempdir().unwrap();
    let w = wallet(&mock, &dir, &me).await;
    for _ in 0..30 {
        mock.mine_to(&me.address(), 1_000, 0);
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    let stop = keep_mining(&mock);
    let hash = tokio::time::timeout(Duration::from_secs(30), w.fusion_basic())
        .await
        .expect("fusion returns")
        .unwrap();
    assert!(mock.pool_tx(&hash).is_some() || w.transaction(&hash).is_some());
    drop(stop);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autosave_after_transactions_and_blocks() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let w = wallet(&mock, &dir, &me).await;
    let path = wallet_path(dir.path(), "w.wallet");
    let on_disk = |what: fn(&xkr_wallet_files::WalletBackendJson) -> u64| -> u64 {
        xkr_wallet_files::backend::open_file(&path, "pw").map_or(0, |j| what(&j))
    };
    let transactions =
        |j: &xkr_wallet_files::WalletBackendJson| j.sub_wallets.transactions.len() as u64;
    let height = |j: &xkr_wallet_files::WalletBackendJson| {
        j.wallet_synchronizer
            .transaction_synchronizer_status
            .last_known_block_height
    };
    // after a transaction
    w.set_autosave(Some(xkr_wallet::AutosaveConfig {
        interval: Duration::from_secs(3600),
        on_transaction: true,
        after_blocks: 1_000_000,
        min_gap: Duration::from_millis(100),
    }));
    let tip = mock.mine_to(&me.address(), 1_000, 0);
    wait_for_block(&w, tip).await;
    wait_until("the saved transaction", || on_disk(transactions) == 1).await;
    // blocks without a transaction: only after `after_blocks` of them
    w.set_autosave(Some(xkr_wallet::AutosaveConfig {
        interval: Duration::from_secs(3600),
        on_transaction: true,
        after_blocks: 5,
        min_gap: Duration::from_millis(100),
    }));
    let saved_height = on_disk(height);
    for _ in 0..2 {
        mock.mine();
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(on_disk(height), saved_height);
    for _ in 0..2 {
        mock.mine();
    }
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    wait_until("the saved blocks", || on_disk(height) >= saved_height + 5).await;
    // off: nothing is written any more
    w.set_autosave(None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let tip = mock.mine_to(&me.address(), 1_000, 0);
    wait_for_block(&w, tip).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(on_disk(transactions), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_outputs_of_a_transaction() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = wallet(&mock, &dir, &me).await;
    wait_for_block(&w, tip).await;
    let hash = w
        .send_basic(&keys().address().to_string(), 100_000, "")
        .await
        .unwrap();
    let tip = mock.mine();
    wait_for_block(&w, tip).await;
    let received = w.received_in(&[hash, Hash([1; 32])]);
    assert_eq!(received.len(), 1);
    assert_eq!(received[&hash], vec![(me.spend_public, 899_990)]);
}
