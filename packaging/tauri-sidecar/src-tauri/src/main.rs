//! The xkr desktop app: node, wallet, miner, p2pool and Hugin relay in one window.
//!
//! Starts xkrd as a sidecar (Tauri `externalBin`) with its own data directory under the app
//! data directory, with the Hyperswarm bridge (`bridge/` as a Tauri resource, run by xkrd on
//! Bare worker threads through libbare-kit from the `lib/` resource), waits for its RPC, and
//! reports `/status` (chain, peers, built-in miner, p2pool, Hugin relay). Settings that are
//! xkrd start flags (mining, p2pool, relay) are applied by restarting xkrd. The wallet runs in
//! the app (`wallet.rs`) and syncs from that xkrd. On quit the wallet is saved, then xkrd is
//! interrupted and waited for, so its database gets a clean, durable commit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod node;
mod rpc;
mod wallet;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::{Manager, RunEvent, State, WindowEvent};
use xkr_consensus::Network;

use node::{NodeManager, Settings};
use wallet::{NodeAddr, WalletManager};

type Nodes<'a> = State<'a, Arc<NodeManager>>;
type Wallets<'a> = State<'a, Arc<WalletManager>>;

fn network(node: &NodeManager) -> Network {
    if node.settings().testnet {
        Network::Testnet
    } else {
        Network::Mainnet
    }
}

/// The node the wallet syncs from: the bundled xkrd unless the settings name another.
fn wallet_node(node: &NodeManager) -> Result<NodeAddr, String> {
    let s = node.settings();
    if s.wallet_node.is_empty() {
        return Ok(NodeAddr {
            host: "127.0.0.1".into(),
            port: node.rpc_port(),
        });
    }
    let (host, port) = node::wallet_node_addr(&s.wallet_node)?;
    Ok(NodeAddr { host, port })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// Sidecar state, settings and paths.
#[tauri::command]
async fn node_info(node: Nodes<'_>) -> Result<Value, String> {
    Ok(node.info())
}

/// xkrd's `/status` (`docs/node-architecture.md`, "Sidecar use").
#[tauri::command]
async fn node_status(node: Nodes<'_>) -> Result<Value, String> {
    let port = node.rpc_port();
    blocking(move || rpc::get_json(port, "/status", Duration::from_secs(3))).await
}

/// The last lines of xkrd's log.
#[tauri::command]
async fn node_log(node: Nodes<'_>, lines: usize) -> Result<String, String> {
    Ok(node.log(lines.clamp(1, 2000)))
}

/// Applies new settings (restarts xkrd when they changed; see `NodeManager::update_settings`).
#[tauri::command]
async fn update_settings(
    node: Nodes<'_>,
    wallets: Wallets<'_>,
    settings: Settings,
) -> Result<(), String> {
    let n = Arc::clone(node.inner());
    blocking(move || n.update_settings(settings)).await?;
    wallets.swap_node(wallet_node(&node)?).await
}

/// Restarts xkrd with the current settings.
#[tauri::command]
async fn node_restart(node: Nodes<'_>) -> Result<(), String> {
    let n = Arc::clone(node.inner());
    blocking(move || n.restart()).await
}

/// Wallet files to open.
#[tauri::command]
async fn wallet_list(node: Nodes<'_>, wallets: Wallets<'_>) -> Result<Value, String> {
    Ok(wallets.list(network(&node)))
}

#[tauri::command]
async fn wallet_create(
    node: Nodes<'_>,
    wallets: Wallets<'_>,
    name: String,
    password: String,
) -> Result<Value, String> {
    wallets
        .create(network(&node), wallet_node(&node)?, &name, &password)
        .await
}

#[tauri::command]
async fn wallet_import_seed(
    node: Nodes<'_>,
    wallets: Wallets<'_>,
    name: String,
    password: String,
    mnemonic: String,
    scan_height: u64,
) -> Result<(), String> {
    wallets
        .import_seed(
            network(&node),
            wallet_node(&node)?,
            &name,
            &password,
            &mnemonic,
            scan_height,
        )
        .await
}

#[tauri::command]
async fn wallet_open(
    node: Nodes<'_>,
    wallets: Wallets<'_>,
    path: PathBuf,
    password: String,
) -> Result<(), String> {
    wallets
        .open(network(&node), wallet_node(&node)?, &path, &password)
        .await
}

#[tauri::command]
async fn wallet_close(wallets: Wallets<'_>) -> Result<(), String> {
    wallets.close().await
}

#[tauri::command]
async fn wallet_overview(wallets: Wallets<'_>) -> Result<Value, String> {
    Ok(wallets.overview().await)
}

#[tauri::command]
async fn wallet_transactions(wallets: Wallets<'_>, limit: usize) -> Result<Value, String> {
    wallets.transactions(limit).await
}

#[tauri::command]
async fn wallet_send(
    wallets: Wallets<'_>,
    address: String,
    amount: u64,
    payment_id: String,
) -> Result<String, String> {
    wallets.send(&address, amount, &payment_id).await
}

#[tauri::command]
async fn wallet_mnemonic(wallets: Wallets<'_>, password: String) -> Result<String, String> {
    wallets.mnemonic(&password).await
}

#[tauri::command]
async fn wallet_rescan(wallets: Wallets<'_>, height: u64) -> Result<(), String> {
    wallets.rescan(height).await
}

#[tauri::command]
async fn check_address(address: String) -> Result<(), String> {
    wallet::check_address(&address)
}

/// Saves the wallet, then stops xkrd. Idempotent.
fn quit(app: &tauri::AppHandle) {
    if let Some(w) = app.try_state::<Arc<WalletManager>>() {
        let w = Arc::clone(w.inner());
        if let Err(e) = tauri::async_runtime::block_on(async move { w.close().await }) {
            eprintln!("cannot save the wallet: {e}");
        }
    }
    if let Some(node) = app.try_state::<Arc<NodeManager>>() {
        node.shutdown();
    }
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let node = Arc::new(NodeManager::new(app.handle())?);
            app.manage(Arc::new(WalletManager::new(node.app_data())));
            app.manage(Arc::clone(&node));
            std::thread::spawn(move || {
                if let Err(e) = node.start() {
                    eprintln!("xkrd did not start: {e}");
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle().clone();
                let node = Arc::clone(app.state::<Arc<NodeManager>>().inner());
                if node.is_exiting() {
                    return;
                }
                // Keep the window (it shows "stopping") until xkrd has exited.
                api.prevent_close();
                std::thread::spawn(move || {
                    quit(&app);
                    app.exit(0);
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            node_info,
            node_status,
            node_log,
            node_restart,
            update_settings,
            wallet_list,
            wallet_create,
            wallet_import_seed,
            wallet_open,
            wallet_close,
            wallet_overview,
            wallet_transactions,
            wallet_send,
            wallet_mnemonic,
            wallet_rescan,
            check_address,
        ])
        .build(tauri::generate_context!())
        .expect("cannot build the Tauri app");

    app.run(|app, event| {
        // Quit paths that skip CloseRequested (Cmd-Q on macOS, app.exit): save the wallet and
        // stop xkrd before the process ends. No-op when both are stopped already.
        if let RunEvent::Exit = event {
            quit(app);
        }
    });
}
