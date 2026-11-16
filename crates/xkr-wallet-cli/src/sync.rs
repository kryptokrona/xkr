//! Waiting for the wallet to catch up before the menu (`src/zedwallet++/sync.cpp`
//! `syncWallet`).
//!
//! The engine syncs in the background; this only shows its progress, `<wallet> of <node>`
//! (one line that updates in place on a terminal, a line per check otherwise), and prints the
//! transactions found on the way.

use crate::format;
use crate::session::{NodeFactory, OpenedOf, Session};

/// `syncWallet`: save every 50 000 blocks.
const SAVE_EVERY: u64 = 50_000;
/// Checks without progress before the "may be stuck" warning.
const STUCK_AFTER: u32 = 20;

impl<F: NodeFactory> Session<F> {
    /// `syncWallet`: until the wallet reaches the node's height at the start.
    pub(crate) fn sync_wallet(&self, opened: &OpenedOf<F>) {
        let p = self.paint;
        let wallet = &opened.wallet;
        let start = wallet.status();
        let mut wallet_count = start.wallet_block_count;
        let target = start.local_daemon_block_count;
        if wallet_count == start.network_block_count {
            return;
        }
        if target < start.network_block_count {
            self.say(
                "Your kryptokronad isn't fully synced yet!\nUntil you are fully synced, you \
                 won't be able to send transactions,\nand your balance may be missing or \
                 incorrect!\n\n",
            );
        }
        if wallet_count == 0 {
            self.say(&p.info(
                "Scanning through the blockchain to find transactions that belong to you.\n\
                 Please wait, this will take some time.\n\n",
            ));
        } else {
            self.say(&p.info(
                "Scanning through the blockchain to find any new transactions you received\n\
                 whilst your wallet wasn't open.\nPlease wait, this may take some time.\n\n",
            ));
        }
        let terminal = self.console.is_terminal();
        // the first block whose transactions are not shown yet
        let mut next = if start.last_block_hash.is_some() {
            wallet_count + 1
        } else {
            0
        };
        let mut last_saved = wallet_count;
        let mut stuck = 0u32;
        let mut line_open = false;
        while wallet_count < target {
            let status = wallet.status();
            let now = status.wallet_block_count;
            let progress = format!(
                "{} of {}",
                p.ok(&now.to_string()),
                p.info(&status.local_daemon_block_count.to_string())
            );
            if terminal {
                self.say(&format!("\r\x1b[K{progress}"));
                line_open = true;
            } else {
                self.say(&format!("{progress}\n"));
            }
            stuck = if now == wallet_count { stuck + 1 } else { 0 };
            if status.last_block_hash.is_some() && now >= next {
                let found = wallet
                    .transactions(Some(next..now + 1), None)
                    .unwrap_or_default();
                next = now + 1;
                for tx in found.iter().filter(|t| !t.is_fusion()) {
                    if line_open {
                        self.say("\n");
                        line_open = false;
                    }
                    let text = if tx.total_amount() < 0 {
                        format::outgoing_transfer(p, tx)
                    } else {
                        format::incoming_transfer(p, tx)
                    };
                    self.say(&format!("{}{text}", p.info("\nNew transaction found!\n\n")));
                }
            }
            wallet_count = now;
            if wallet_count > last_saved + SAVE_EVERY {
                if line_open {
                    self.say("\n");
                    line_open = false;
                }
                self.say(&p.info("\nSaving progress...\n\n"));
                let _ = self.rt.block_on(wallet.save());
                last_saved = wallet_count;
            }
            if stuck >= STUCK_AFTER {
                if line_open {
                    self.say("\n");
                    line_open = false;
                }
                self.say(&format!(
                    "{}\n",
                    p.warn(
                        "Syncing may be stuck. Ensure your daemon or remote node is online, and \
                         not syncing.\n(Syncing often stalls wallet operation)\nGive the daemon \
                         a restart if possible.\nIf this persists, visit https://kryptokrona.org \
                         for support."
                    )
                ));
            }
            if wallet_count >= target {
                break;
            }
            std::thread::sleep(self.config.timing.sync_poll);
        }
        if line_open {
            self.say("\n");
        }
    }
}
