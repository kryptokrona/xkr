//! Persistence: format A (`WalletBackend` file, also in wallet-backend-js's string form) and
//! format B (walletd container with the engine's state in `<container>.xkrstate`).

mod common;

use common::{Mock, config, keys, wait_for_block, wallet_path};
use xkr_consensus::Network;
use xkr_crypto::os_rng;
use xkr_wallet::persist::green::{
    GreenCachedKey, GreenHistory, GreenTransaction, GreenTransfer, cache,
};
use xkr_wallet::{ErrorCode, FileFormat, Wallet, sidecar_path};
use xkr_wallet_core::WalletKeys;
use xkr_wallet_files::fault::{self, Fault};
use xkr_wallet_files::green::{self, GreenRecord, GreenViewKeys, GreenWallet};
use xkr_wallet_files::{FileKind, backend, detect};

fn container_for(keys: &WalletKeys) -> GreenWallet {
    GreenWallet {
        view: GreenViewKeys {
            public_key: keys.view_public,
            secret_key: keys.view_secret,
            creation_timestamp: 0,
        },
        records: vec![GreenRecord {
            spend_public_key: keys.spend_public,
            spend_secret_key: Some(keys.spend_secret),
            creation_timestamp: 0,
        }],
        history: None,
        cache_error: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_a_round_trip_and_errors() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let path = wallet_path(dir.path(), "a.wallet");
    let w = Wallet::import_keys(
        &path,
        "pw",
        me.spend_secret,
        me.view_secret,
        0,
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    wait_for_block(&w, tip).await;
    let (sub, _) = w.add_subwallet().await.unwrap();
    let hash = w
        .send_basic(&keys().address().to_string(), 1_000, "")
        .await
        .unwrap();
    let balances = w.balances();
    let transactions = w.transactions(None, None).unwrap();
    let pending = w.unconfirmed(None).unwrap();
    let height = w.status().wallet_block_count;
    let tx_key = w.tx_private_key(&hash).unwrap();
    w.close().await.unwrap();

    // what C++ wallet-api reads
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(detect(&bytes), FileKind::WalletBackend);
    let json = backend::open(&bytes, "pw").unwrap();
    assert_eq!(json.sub_wallets.public_spend_keys.len(), 2);
    assert_eq!(json.sub_wallets.locked_transactions.len(), 1);
    assert_eq!(json.sub_wallets.tx_private_keys.len(), 1);
    assert_eq!(
        json.wallet_synchronizer
            .transaction_synchronizer_status
            .last_known_block_height,
        height
    );

    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletGreen, false),
    )
    .await
    .unwrap();
    assert_eq!(w.format(), FileFormat::WalletBackend);
    assert_eq!(w.balances(), balances);
    assert_eq!(w.transactions(None, None).unwrap(), transactions);
    assert_eq!(w.unconfirmed(None).unwrap(), pending);
    assert_eq!(w.status().wallet_block_count, height);
    assert_eq!(w.tx_private_key(&hash).unwrap(), tx_key);
    assert_eq!(w.addresses()[1], sub);
    drop(w);

    let node = || mock.node();
    let cfg = || config(Network::Mainnet, FileFormat::WalletBackend, false);
    let e = Wallet::open(&path, "wrong", node(), cfg())
        .await
        .unwrap_err();
    assert_eq!(e.code(), ErrorCode::WrongPassword);
    let e = Wallet::open(dir.path().join("missing"), "pw", node(), cfg())
        .await
        .unwrap_err();
    assert_eq!(e.code(), ErrorCode::FilenameNonExistent);
    let e = Wallet::create(&path, "pw", node(), cfg())
        .await
        .unwrap_err();
    assert_eq!(e.code(), ErrorCode::WalletFileAlreadyExists);
    let garbage = dir.path().join("garbage");
    std::fs::write(&garbage, b"not a wallet at all").unwrap();
    let e = Wallet::open(&garbage, "pw", node(), cfg())
        .await
        .unwrap_err();
    assert_eq!(e.code(), ErrorCode::NotAWalletFile);
    let e = Wallet::import_seed(dir.path().join("s"), "pw", "one two", 0, node(), cfg())
        .await
        .unwrap_err();
    assert_eq!(e.code(), ErrorCode::MnemonicWrongLength);

    // wallet-backend-js's string form is read and written back in that form
    let js = dir.path().join("js.wallet");
    std::fs::write(&js, backend::encode_js_buffer_string(&bytes)).unwrap();
    let w = Wallet::open(&js, "pw", node(), cfg()).await.unwrap();
    assert_eq!(w.format(), FileFormat::WalletBackendJsString);
    assert_eq!(w.balances(), balances);
    w.close().await.unwrap();
    let text = std::fs::read(&js).unwrap();
    assert_eq!(detect(&text), FileKind::WalletBackendJsString);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_import_and_mnemonic() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let seed = xkr_wallet_core::mnemonic_from_secret(&me.spend_secret);
    let w = Wallet::import_seed(
        wallet_path(dir.path(), "seed.wallet"),
        "",
        &seed,
        0,
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.primary_address(), me.address().to_string());
    assert_eq!(w.keys(), me.view_secret);
    assert_eq!(w.mnemonic(&w.primary_address()).unwrap(), seed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_b_container_with_sidecar() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let path = wallet_path(dir.path(), "walletd.container");
    green::write_keys_only_file(&path, &container_for(&me), "pw", &mut os_rng()).unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();

    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    assert_eq!(w.format(), FileFormat::WalletGreen);
    assert_eq!(w.primary_address(), me.address().to_string());
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);
    w.save().await.unwrap();
    let sidecar = sidecar_path(&path);
    assert!(sidecar.exists());
    // the container stays a container C++ opens; the state is format A with its password
    assert!(backend::open_file(&sidecar, "pw").is_ok());

    // a key change rewrites the container (keys only)
    let (sub, sub_secret) = w.add_subwallet().await.unwrap();
    let reread = green::open_file(&path, "pw").unwrap();
    assert_eq!(reread.records.len(), 2);
    assert_eq!(reread.records[0].spend_public_key, me.spend_public);
    assert_eq!(reread.records[1].spend_secret_key, Some(sub_secret));
    assert!(reread.history.is_none());
    let height = w.status().wallet_block_count;
    w.close().await.unwrap();

    // reopened: the state comes from the sidecar, no rescan needed
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.addresses(), vec![me.address().to_string(), sub.clone()]);
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);
    assert_eq!(w.status().wallet_block_count, height);

    // a new password applies to both files
    w.change_password("new").await.unwrap();
    w.close().await.unwrap();
    assert!(green::open_file(&path, "pw").is_err());
    assert!(backend::open_file(&sidecar, "new").is_ok());
    let w = Wallet::open(
        &path,
        "new",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);

    // walletd `export`: a keys-only container elsewhere
    let exported = dir.path().join("export.container");
    w.export_container(&exported).await.unwrap();
    let copy = green::open_file(&exported, "new").unwrap();
    assert_eq!(copy.records.len(), 2);
    assert_eq!(copy.view.secret_key, me.view_secret);
    w.close().await.unwrap();

    // C++ replaced the container with other keys: the stale sidecar is not used
    let other = keys();
    green::write_keys_only_file(&path, &container_for(&other), "new", &mut os_rng()).unwrap();
    let w = Wallet::open(
        &path,
        "new",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.addresses(), vec![other.address().to_string()]);
    assert_eq!(w.transactions(None, None).unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_in_either_format() {
    let mock = Mock::new();
    let dir = tempfile::tempdir().unwrap();
    let a = Wallet::create(
        wallet_path(dir.path(), "new-a.wallet"),
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    let seed = a.mnemonic(&a.primary_address()).unwrap();
    assert_eq!(seed.split(' ').count(), 25);
    let b_path = wallet_path(dir.path(), "new-b.container");
    let b = Wallet::create(
        &b_path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletGreen, false),
    )
    .await
    .unwrap();
    assert_eq!(b.format(), FileFormat::WalletGreen);
    let container = green::open_file(&b_path, "pw").unwrap();
    assert_eq!(container.view.secret_key, b.keys());
    assert_eq!(container.records.len(), 1);
    assert!(container.records[0].creation_timestamp > 0);
    assert!(sidecar_path(&b_path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn container_without_addresses_and_export_with_history() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let path = wallet_path(dir.path(), "walletd.container");
    green::write_keys_only_file(&path, &container_for(&me), "pw", &mut os_rng()).unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    wait_for_block(&w, tip).await;

    // walletd `export` with a history: C++ reads it as the container's cache
    let history = GreenHistory {
        save_level: cache::SAVE_KEYS_AND_TRANSACTIONS,
        cached_keys: vec![GreenCachedKey {
            spend_public_key: me.spend_public,
            balances: None,
        }],
        transactions: vec![GreenTransaction {
            state: cache::transaction_state::SUCCEEDED,
            timestamp: 1_700_000_090,
            block_height: 1,
            hash: w.transactions(None, None).unwrap()[0].hash,
            total_amount: 1_000_000,
            is_base: true,
            ..GreenTransaction::default()
        }],
        transfers: vec![GreenTransfer {
            transaction_id: 0,
            address: me.address().to_string(),
            amount: 1_000_000,
            transfer_type: cache::transfer_type::USUAL,
        }],
    };
    let exported = dir.path().join("export.container");
    w.export_container_with_history(&exported, Some(history.clone()))
        .await
        .unwrap();
    let copy = green::open_file(&exported, "pw").unwrap();
    assert_eq!(copy.records.len(), 1);
    assert_eq!(copy.history, Some(history));
    let e = Wallet::open(
        &exported,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(e.transactions(None, None).unwrap().len(), 1);
    drop(e);

    // walletd `deleteAddress`: the primary and only address too
    let (sub, _) = w.add_subwallet().await.unwrap();
    w.delete_address(&me.address().to_string()).await.unwrap();
    assert_eq!(w.primary_address(), sub);
    w.delete_address(&sub).await.unwrap();
    assert_eq!(w.addresses(), Vec::<String>::new());
    assert_eq!(w.primary_address(), "");
    assert_eq!(w.status().last_block_hash, None);
    assert_eq!(green::open_file(&path, "pw").unwrap().records.len(), 0);
    w.close().await.unwrap();

    // the empty container opens, takes a tracking address and is a tracking container then
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    assert_eq!(w.subwallet_count(), 0);
    let tracked = w.import_view_subwallet(me.spend_public, 0).await.unwrap();
    assert_eq!(tracked, me.address().to_string());
    assert!(w.is_view_wallet());
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);
    w.close().await.unwrap();
    let container = green::open_file(&path, "pw").unwrap();
    assert!(container.is_tracking());
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert!(w.is_view_wallet());
    assert_eq!(w.addresses(), vec![tracked]);
}

/// Whether `dir` holds exactly `names`.
fn only_files(dir: &std::path::Path, names: &[&str]) -> bool {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    let mut want: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    want.sort();
    found == want
}

/// Points spread over a file of `len` bytes, and the end.
fn stop_points(len: usize) -> Vec<Fault> {
    let step = (len / 24).max(1);
    let mut points: Vec<Fault> = (0..len).step_by(step).map(Fault::AfterBytes).collect();
    points.push(Fault::AfterBytes(len - 1));
    points.push(Fault::BeforeRename);
    points
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_save_leaves_the_last_saved_wallet() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    let path = wallet_path(dir.path(), "a.wallet");
    let cfg = |format| config(Network::Mainnet, format, false);

    // format A: one file
    let w = Wallet::import_keys(
        &path,
        "pw",
        me.spend_secret,
        me.view_secret,
        0,
        mock.node(),
        cfg(FileFormat::WalletBackend),
    )
    .await
    .unwrap();
    w.add_subwallet().await.unwrap();
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    for point in stop_points(len) {
        fault::inject(&path, point);
        assert!(w.save().await.is_err(), "{point:?}");
        let json = backend::open_file(&path, "pw").unwrap();
        assert_eq!(json.sub_wallets.public_spend_keys.len(), 1, "{point:?}");
        assert!(only_files(dir.path(), &["a.wallet"]), "{point:?}");
    }
    w.save().await.unwrap();
    assert_eq!(
        backend::open_file(&path, "pw")
            .unwrap()
            .sub_wallets
            .public_spend_keys
            .len(),
        2
    );
    drop(w);

    // format B: the container and the state file
    let path = wallet_path(dir.path(), "b.container");
    let sidecar = sidecar_path(&path);
    let w = Wallet::import_keys(
        &path,
        "pw",
        me.spend_secret,
        me.view_secret,
        0,
        mock.node(),
        cfg(FileFormat::WalletGreen),
    )
    .await
    .unwrap();
    let names = ["a.wallet", "b.container", "b.container.xkrstate"];
    // a stopped container write: the old pair
    let len = std::fs::metadata(&path).unwrap().len() as usize;
    for point in stop_points(len) {
        fault::inject(&path, point);
        assert!(w.add_subwallet().await.is_err(), "{point:?}");
        assert_eq!(green::open_file(&path, "pw").unwrap().records.len(), 1);
        let state = backend::open_file(&sidecar, "pw").unwrap();
        assert_eq!(state.sub_wallets.public_spend_keys.len(), 1, "{point:?}");
        assert!(only_files(dir.path(), &names), "{point:?}");
    }
    // closing writes the state with the addresses whose container write failed; that state
    // does not belong to the container and is not used
    w.close().await.unwrap();
    let w = Wallet::open(&path, "pw", mock.node(), cfg(FileFormat::WalletGreen))
        .await
        .unwrap();
    assert_eq!(green::open_file(&path, "pw").unwrap().records.len(), 1);
    assert_eq!(w.addresses().len(), 1);
    // a stopped state write after the container: new keys, the old state is rebuilt
    let len = std::fs::metadata(&sidecar).unwrap().len() as usize;
    for point in stop_points(len) {
        fault::inject(&sidecar, point);
        assert!(w.add_subwallet().await.is_err(), "{point:?}");
        let records = green::open_file(&path, "pw").unwrap().records.len();
        let reopened = Wallet::open(&path, "pw", mock.node(), cfg(FileFormat::WalletGreen))
            .await
            .unwrap();
        assert_eq!(reopened.addresses().len(), records, "{point:?}");
        drop(reopened);
        assert!(only_files(dir.path(), &names), "{point:?}");
    }
}

/// A `WalletLegacy` file (version 1, simplewallet) with `keys` and no details, as C++
/// `WalletLegacySerializer::serialize` wrote it.
fn legacy_v1_file(keys: &WalletKeys, password: &str) -> Vec<u8> {
    use green::binary::Writer;
    let mut p = Writer::new();
    p.varint(0); // creation timestamp
    p.bytes(&keys.spend_public.0);
    p.bytes(&keys.spend_secret.0);
    p.bytes(&keys.view_public.0);
    p.bytes(&keys.view_secret.0);
    p.bool(false); // has_details
    p.string(b""); // cache
    let iv = [7u8; green::chacha8::IV_SIZE];
    let cipher = green::chacha8::chacha8(
        &p.into_bytes(),
        &green::chacha8::generate_chacha8_key(password),
        &iv,
    );
    let mut w = Writer::new();
    w.varint(1);
    w.bytes(&iv);
    w.string(&cipher);
    w.into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_wallet_file_is_converted_on_open() {
    let mock = Mock::new();
    let me = keys();
    let dir = tempfile::tempdir().unwrap();
    mock.mine_to(&me.address(), 1_000_000, 0);
    let tip = mock.mine();
    let path = wallet_path(dir.path(), "old.wallet");
    let old = legacy_v1_file(&me, "pw");
    std::fs::write(&path, &old).unwrap();
    assert_eq!(detect(&old), FileKind::WalletGreenLegacy(1));

    // a wrong password changes nothing on disk
    let err = Wallet::open(
        &path,
        "nope",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), ErrorCode::WrongPassword);
    assert_eq!(std::fs::read(&path).unwrap(), old);
    assert!(!green::legacy::backup_path(&path).exists());

    // opened like any wallet: converted to a container, the old file kept as .backup
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, true),
    )
    .await
    .unwrap();
    assert_eq!(w.format(), FileFormat::WalletGreen);
    assert_eq!(w.primary_address(), me.address().to_string());
    assert_eq!(
        std::fs::read(green::legacy::backup_path(&path)).unwrap(),
        old
    );
    assert_eq!(
        detect(&std::fs::read(&path).unwrap()),
        FileKind::WalletGreen
    );
    wait_for_block(&w, tip).await;
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);
    w.close().await.unwrap();

    // later opens are ordinary container opens
    let w = Wallet::open(
        &path,
        "pw",
        mock.node(),
        config(Network::Mainnet, FileFormat::WalletBackend, false),
    )
    .await
    .unwrap();
    assert_eq!(w.balance(None).unwrap().unlocked, 1_000_000);
    w.close().await.unwrap();
}
