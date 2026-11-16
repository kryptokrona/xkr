//! Sample desktop app around xkrd.
//!
//! Starts xkrd as a sidecar (Tauri `externalBin`) with its own data directory under the app
//! data directory, with the Hyperswarm bridge (`bridge/` as a Tauri resource, run by xkrd on
//! Bare worker threads through libbare-kit from the `lib/` resource), waits for its RPC, and
//! shows `/status` (height, peers, sync state, built-in miner). Mining is switched by
//! restarting xkrd with `--mine`. On quit xkrd is interrupted and waited for, so its database
//! gets a clean, durable commit.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod node;
mod rpc;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::{Manager, RunEvent, State, WindowEvent};

use node::NodeManager;

/// Sidecar state, settings and paths.
#[tauri::command]
async fn node_info(node: State<'_, Arc<NodeManager>>) -> Result<Value, String> {
    Ok(node.info())
}

/// xkrd's `/status` (`docs/node-architecture.md`, "Sidecar use").
#[tauri::command]
async fn node_status(node: State<'_, Arc<NodeManager>>) -> Result<Value, String> {
    let port = node.rpc_port();
    tauri::async_runtime::spawn_blocking(move || {
        rpc::get_json(port, "/status", Duration::from_secs(3))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Starts or stops the built-in miner (restarts xkrd; see `NodeManager::set_mining`).
#[tauri::command]
async fn set_mining(
    node: State<'_, Arc<NodeManager>>,
    enabled: bool,
    address: String,
    threads: usize,
) -> Result<(), String> {
    let node = Arc::clone(node.inner());
    tauri::async_runtime::spawn_blocking(move || node.set_mining(enabled, address, threads))
        .await
        .map_err(|e| e.to_string())?
}

fn main() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let node = Arc::new(NodeManager::new(app.handle())?);
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
                    node.shutdown();
                    app.exit(0);
                });
            }
        })
        .invoke_handler(tauri::generate_handler![node_info, node_status, set_mining])
        .build(tauri::generate_context!())
        .expect("cannot build the Tauri app");

    app.run(|app, event| {
        // Quit paths that skip CloseRequested (Cmd-Q on macOS, app.exit): stop xkrd before the
        // process ends. No-op when it is already stopped.
        if let RunEvent::Exit = event
            && let Some(node) = app.try_state::<Arc<NodeManager>>()
        {
            node.shutdown();
        }
    });
}
