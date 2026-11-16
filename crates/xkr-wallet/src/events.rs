//! What the wallet reports while it runs (`src/wallet_backend/event_handler.h` `EventHandler`:
//! `onSynced`, `onTransaction`, plus the states walletd needs for sent transactions).
//!
//! [`crate::Wallet::subscribe`] hands out a `tokio::sync::broadcast` receiver; a receiver that
//! falls behind loses the oldest events (`RecvError::Lagged`), the wallet never waits for it.

use xkr_types::Hash;

use crate::state::Transaction;

/// Capacity of the event channel.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalletEvent {
    /// A transaction touching the wallet was found in a block (`onTransaction`). Sent
    /// transactions come here again once confirmed.
    Transaction(Transaction),
    /// This wallet sent a transaction; it waits in the pool (walletd state `SUCCEEDED`, block
    /// index unconfirmed).
    Sent(Transaction),
    /// The node no longer knows a sent transaction; its inputs are spendable again (walletd
    /// state `CANCELLED`).
    Cancelled(Hash),
    /// The node's chain changed below the wallet's height: everything from `height` on was
    /// undone and is scanned again.
    Rollback {
        /// First block index that was undone.
        height: u64,
    },
    /// The wallet processed a block at the network height (`onSynced`).
    Synced {
        /// Index of that block.
        height: u64,
    },
    /// The wallet was reset and scans again from `height`.
    Reset {
        /// Scan height.
        height: u64,
    },
    /// The daemon answered again after failing.
    DaemonOnline,
    /// The daemon did not answer.
    DaemonOffline {
        /// What went wrong.
        error: String,
    },
}
