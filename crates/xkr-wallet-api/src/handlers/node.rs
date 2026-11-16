//! Node, save and reset (`ApiDispatcher::getNodeInfo`, `setNodeInfo`, `saveWallet`,
//! `resetWallet`).

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_wallet::Wallet;

use super::Answer;
use crate::params;
use crate::respond::Reply;
use crate::server::{App, NodeFactory};

/// GET `/node`: `{daemonHost, daemonPort, nodeFee, nodeAddress}`.
#[must_use]
pub fn get<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    let n = wallet.node();
    Reply::json(
        200,
        &json!({
            "daemonHost": n.host,
            "daemonPort": n.port,
            "nodeFee": n.fee_amount,
            "nodeAddress": n.fee_address,
        }),
    )
}

/// PUT `/node`: `daemonHost` and `daemonPort` (both required); 202.
pub async fn set<N: NodeFactory>(
    app: &App<N>,
    wallet: &Wallet<N::Transport>,
    body: &Value,
) -> Answer {
    let host = params::string(body, "daemonHost")?;
    let port = params::u16_param(body, "daemonPort")?;
    let node = app.nodes.connect(&host, port)?;
    wallet.swap_node(node).await;
    Ok(Reply::empty(202))
}

/// PUT `/save`: 200 (C++ ignores the result of `WalletBackend::save`).
pub async fn save<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    if let Err(e) = wallet.save().await {
        tracing::warn!(error = %e, "saving the wallet failed");
    }
    Reply::empty(200)
}

/// PUT `/reset`: rescans from `scanHeight` (default 0); 200.
pub async fn reset<T: Transport + 'static>(wallet: &Wallet<T>, body: &Value) -> Answer {
    let scan_height = params::opt_u64(body, "scanHeight")?.unwrap_or(0);
    if let Err(e) = wallet.reset(scan_height).await {
        tracing::warn!(error = %e, "saving the wallet after the reset failed");
    }
    Ok(Reply::empty(200))
}
