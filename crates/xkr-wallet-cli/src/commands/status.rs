//! `status` (`src/zedwallet++/command_implementations.cpp` `status`, `printHeights`,
//! `printSyncStatus`, `printHashrate`, `printSyncSummary`), plus the transaction versions the
//! node accepts and its next fork height.

use std::fmt::Write as _;
use xkr_daemon_client::Info;

use crate::format;
use crate::input::Painter;
use crate::session::{NodeFactory, OpenedOf, Session};

/// The lines about the node's rules: `tx_versions` (a C++ node takes version 1 only) and the
/// next of its `upgrade_heights` above its top block.
pub(crate) fn node_rules(p: Painter, info: &Info) -> String {
    let versions = info.tx_versions.as_ref().map_or_else(
        || "1".to_owned(),
        |v| {
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    let top = info.local_block_count();
    let next_fork = info
        .upgrade_heights
        .iter()
        .copied()
        .filter(|h| *h > top)
        .min()
        .map_or_else(
            || "none scheduled".to_owned(),
            |h| format!("height {h} ({} blocks from now)", h - top),
        );
    format!(
        "Transaction versions: {}\nNext fork: {}\n",
        p.ok(&versions),
        p.ok(&next_fork)
    )
}

impl<F: NodeFactory> Session<F> {
    /// `status`.
    pub(crate) fn status(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let s = w.wallet.status();
        let (wallet, local, network) = (
            s.wallet_block_count,
            s.local_daemon_block_count,
            s.network_block_count,
        );
        let height_tone = |good: bool, n: u64| {
            if good {
                p.ok(&n.to_string())
            } else {
                p.warn(&n.to_string())
            }
        };
        let mut text = format!(
            "Wallet blockchain height: {}\nLocal blockchain height: {}\nNetwork blockchain \
             height: {}\n\n",
            height_tone(wallet + 1000 > network, wallet),
            height_tone(local == network, local),
            p.ok(&network.to_string())
        );
        let network_sync = format!("{}%", format::sync_percentage(local, network));
        let wallet_sync = format!("{}%", format::sync_percentage(wallet, network));
        let _ = writeln!(
            text,
            "Network sync status: {}\nWallet sync status: {}\n",
            if local == network {
                p.ok(&network_sync)
            } else {
                p.warn(&network_sync)
            },
            if wallet + 10 > network {
                p.ok(&wallet_sync)
            } else {
                p.warn(&wallet_sync)
            }
        );
        if s.hashrate != 0 {
            let _ = writeln!(
                text,
                "Network hashrate: {} (Based on the last local block)",
                p.ok(&format::mining_speed(s.hashrate))
            );
        }
        let _ = writeln!(text, "Peers: {}", p.ok(&s.peer_count.to_string()));
        if let Some(info) = &w.wallet.daemon_status().info {
            text.push_str(&node_rules(p, info));
        }
        let _ = writeln!(text, "Wallet: {}", p.dim(&crate::args::version_line()));
        text.push('\n');
        if local == 0 && network == 0 {
            let _ = writeln!(
                text,
                "{}{}{}",
                p.warn("Uh oh, it looks like you don't have "),
                p.warn("kryptokronad"),
                p.warn(" open!")
            );
        } else if wallet + 1000 < network && local == network {
            let _ = writeln!(
                text,
                "{}\nBalances might be incorrect whilst this is ongoing.",
                p.info(
                    "You are synced with the network, but the blockchain is still being \
                     scanned for your transactions."
                )
            );
        } else if local == network {
            let _ = writeln!(text, "{}", p.ok("Yay! You are synced!"));
        } else {
            let _ = writeln!(
                text,
                "{}",
                p.warn("Be patient, you are still syncing with the network!")
            );
        }
        self.say(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(json: &str) -> Info {
        serde_json::from_str(json).expect("info")
    }

    #[test]
    fn rules_lines() {
        let p = Painter::new(false);
        let cpp = info(
            r#"{"height":101,"network_height":101,"difficulty":1,"outgoing_connections_count":0,
                "incoming_connections_count":0,"upgrade_heights":[50,3000000]}"#,
        );
        assert_eq!(
            node_rules(p, &cpp),
            "Transaction versions: 1\nNext fork: height 3000000 (2999900 blocks from now)\n"
        );
        let xkrd = info(
            r#"{"height":11,"network_height":11,"difficulty":1,"outgoing_connections_count":0,
                "incoming_connections_count":0,"upgrade_heights":[4],"tx_versions":[1,2]}"#,
        );
        assert_eq!(
            node_rules(p, &xkrd),
            "Transaction versions: 1, 2\nNext fork: none scheduled\n"
        );
    }
}
