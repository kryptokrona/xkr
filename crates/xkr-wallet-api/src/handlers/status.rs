//! `GET /status` (`ApiDispatcher::getStatus`).

use serde_json::json;
use xkr_daemon_client::Transport;
use xkr_wallet::Wallet;

use crate::respond::Reply;

/// `{walletBlockCount, localDaemonBlockCount, networkBlockCount, peerCount, hashrate,
/// isViewWallet, subWalletCount}`.
#[must_use]
pub fn status<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    let s = wallet.status();
    Reply::json(
        200,
        &json!({
            "walletBlockCount": s.wallet_block_count,
            "localDaemonBlockCount": s.local_daemon_block_count,
            "networkBlockCount": s.network_block_count,
            "peerCount": s.peer_count,
            "hashrate": s.hashrate,
            "isViewWallet": s.is_view_wallet,
            "subWalletCount": s.subwallet_count,
        }),
    )
}
