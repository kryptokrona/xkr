//! `advanced`, `address`, `balance`, `backup`, `seed`, `keys` and `help`
//! (`src/zedwallet++/command_implementations.cpp` `advanced`, `balance`, `backup`,
//! `printPrivateKeys`, `help`; `command_dispatcher.cpp` for `address`; `open.cpp`
//! `promptSaveKeys`).

use std::fmt::Write as _;
use xkr_wallet::Transaction;
use xkr_wallet_core::{ADDRESS_PREFIX, ADDRESS_PREFIX_ALT, Address};
use zeroize::Zeroizing;

use crate::big;
use crate::commands;
use crate::session::{NodeFactory, OpenedOf, Session};
use crate::steps::info_block;

/// `utilities::alternateAddressForm`: the same keys under the other prefix (`SEKR` / `Xkr`).
pub(crate) fn alternate_address(address: &str) -> Option<String> {
    let parsed = Address::parse(address).ok()?;
    let prefix = if parsed.prefix == ADDRESS_PREFIX {
        ADDRESS_PREFIX_ALT
    } else {
        ADDRESS_PREFIX
    };
    Some(Address { prefix, ..parsed }.encode())
}

impl<F: NodeFactory> Session<F> {
    /// `help` (and `advanced`): the commands by group; `help <command>` explains one.
    pub(crate) fn help(&self, w: &OpenedOf<F>, topic: &str) {
        let available = commands::available(w.wallet.is_view_wallet());
        if topic.is_empty() {
            self.say(&commands::groups(self.paint, &available));
            return;
        }
        match commands::find(topic, &available) {
            Some(command) => self.say(&commands::explain(self.paint, command)),
            None => self.say(&format!(
                "  {} · type help for the list\n",
                self.paint
                    .warn(&format!("Unknown command: {}", topic.to_lowercase()))
            )),
        }
    }

    /// `address` (`addr`): the primary address, and under it, dim, the same wallet with the
    /// other prefix.
    pub(crate) fn address(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let address = w.wallet.primary_address();
        let mut text = format!("  {}\n", p.bold(&address));
        if let Some(alternate) = alternate_address(&address) {
            let _ = writeln!(text, "  {}", p.dim(&alternate));
        }
        self.say(&text);
    }

    /// The available balance as `balance` counts it: a view wallet counts its non-fusion
    /// transactions, since it cannot see its spends.
    #[allow(clippy::unused_self)] // a session method like the other balance helpers
    pub(crate) fn available_balance(&self, w: &OpenedOf<F>) -> (u64, u64) {
        let wallet = &w.wallet;
        let balance = wallet.balance(None).unwrap_or_default();
        let available = if wallet.is_view_wallet() {
            let sum: i64 = wallet
                .transactions(None, None)
                .unwrap_or_default()
                .iter()
                .filter(|t| !t.is_fusion())
                .map(Transaction::total_amount)
                .sum();
            u64::try_from(sum).unwrap_or(0)
        } else {
            balance.unlocked
        };
        (available, balance.locked)
    }

    /// The balance in big digits on a colour terminal (see [`crate::big`]).
    pub(crate) fn balance_lines(&self, w: &OpenedOf<F>) -> String {
        let (available, locked) = self.available_balance(w);
        big::balance_block(self.paint, available, locked, self.big_width())
    }

    /// The terminal width for big digits; `None` without a colour terminal.
    fn big_width(&self) -> Option<usize> {
        (self.console.is_terminal() && self.paint.colour()).then(big::columns)
    }

    /// `balance` (`bal`, `money`, `account`): the available balance (big on a terminal),
    /// what is locked, and the C++ notes for view wallets and unfinished syncs.
    pub(crate) fn balance(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let wallet = &w.wallet;
        let mut text = self.balance_lines(w);
        if wallet.is_view_wallet() {
            text.push_str(&p.info(
                "\nPlease note that view only wallets can only track incoming transactions,\n",
            ));
            text.push_str(&p.info("and so your wallet balance may appear inflated.\n"));
        }
        let status = wallet.status();
        if status.local_daemon_block_count < status.network_block_count {
            text.push_str(&p.info("\nYour daemon is not fully synced with the network!\n"));
            text.push_str("Your balance may be incorrect until you are fully synced!\n");
        } else if status.wallet_block_count + 1000 < status.network_block_count {
            text.push_str(
                &p.info("\nThe blockchain is still being scanned for your transactions.\n"),
            );
            text.push_str("Balances might be incorrect whilst this is ongoing.\n");
        }
        self.say(&text);
    }

    /// The line that goes before secrets.
    fn keep_private(&self) {
        self.say(&format!(
            "  {}\n",
            self.paint
                .warn("Keep this private: anyone who has it can spend your XKR.")
        ));
    }

    /// `seed`: the 25 words of the primary address after the password. A view wallet and a
    /// wallet whose keys were not made from a seed have none.
    pub(crate) fn seed(&mut self, w: &OpenedOf<F>) {
        let p = self.paint;
        if w.wallet.is_view_wallet() {
            self.say(&format!(
                "  {} · a view-only wallet has no private spend key\n",
                p.warn("No seed")
            ));
            return;
        }
        let Ok(seed) = w.wallet.mnemonic(&w.wallet.primary_address()) else {
            self.say(&format!(
                "  {} · the keys of this wallet were not made from a seed; keys shows them\n",
                p.warn("No seed")
            ));
            return;
        };
        let seed = Zeroizing::new(seed);
        if !self.confirm_password(&w.password, "  Password › ") {
            return;
        }
        self.keep_private();
        self.say(&Zeroizing::new(format!("  {}\n", p.bold(&seed))));
    }

    /// `keys`: the private view key and, for a spending wallet, the private spend key of the
    /// primary address, after the password.
    pub(crate) fn keys(&mut self, w: &OpenedOf<F>) {
        if !self.confirm_password(&w.password, "  Password › ") {
            return;
        }
        let wallet = &w.wallet;
        let mut rows = vec![("View key", wallet.keys().to_hex())];
        if !wallet.is_view_wallet()
            && let Ok((_, spend)) = wallet.spend_keys(&wallet.primary_address())
        {
            rows.push(("Spend key", spend.to_hex()));
        }
        self.keep_private();
        let text = Zeroizing::new(info_block(self.paint, &rows));
        for (_, value) in &mut rows {
            zeroize::Zeroize::zeroize(value);
        }
        self.say(&text);
    }

    /// `printPrivateKeys`: the view key; for a spending wallet also the spend key of the
    /// primary address and its seed when the keys are deterministic.
    pub(crate) fn print_private_keys(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let wallet = &w.wallet;
        let mut text = Zeroizing::new(format!(
            "{}{}\n",
            p.ok("Private view key:\n"),
            p.ok(&Zeroizing::new(wallet.keys().to_hex()))
        ));
        if !wallet.is_view_wallet() {
            let primary = wallet.primary_address();
            if let Ok((_, spend)) = wallet.spend_keys(&primary) {
                text.push_str(&p.ok("\nPrivate spend key:\n"));
                text.push_str(&Zeroizing::new(p.ok(&Zeroizing::new(spend.to_hex()))));
                text.push('\n');
            }
            if let Ok(seed) = wallet.mnemonic(&primary) {
                let seed = Zeroizing::new(seed);
                text.push_str(&p.ok("\nMnemonic seed:\n"));
                text.push_str(&Zeroizing::new(p.ok(&seed)));
                text.push('\n');
            }
        }
        self.say(&text);
    }

    /// `backup`: after the password.
    pub(crate) fn backup(&mut self, w: &OpenedOf<F>) {
        if self.confirm_password(&w.password, "Confirm your current password: ") {
            self.print_private_keys(w);
        }
    }

    /// `promptSaveKeys`, shown once for a new wallet: the address, then the seed and the
    /// private keys to write down.
    pub(crate) fn prompt_save_keys(&self, w: &OpenedOf<F>) {
        let wallet = &w.wallet;
        let primary = wallet.primary_address();
        let mut rows = vec![("Address", primary.clone())];
        if let Ok(seed) = wallet.mnemonic(&primary) {
            rows.push(("Seed", seed));
        }
        rows.push(("View key", wallet.keys().to_hex()));
        if let Ok((_, spend)) = wallet.spend_keys(&primary) {
            rows.push(("Spend key", spend.to_hex()));
        }
        self.say(&format!(
            "  {}\n",
            self.paint.warn(
                "Write these down and keep them private: they restore this wallet, and anyone \
                 who has them can spend your XKR."
            )
        ));
        let text = Zeroizing::new(info_block(self.paint, &rows));
        for (_, value) in &mut rows {
            zeroize::Zeroize::zeroize(value);
        }
        self.say(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternate_forms() {
        let sekr = "SEKReX4MYn2HAJFoHvcqVBPyHYom2ynKeK6dpYptbp8gQNzdzE73ZDkNmNurqfhhcMSUXpS1ZGEJKiKJUcPCyw7vYaCc35yBZCf";
        let xkr = "XkrezgVVv8PTPBBHRdFJMnFBi5GV4d8BV9PHLgKDVMzraC2YLfkjyBX6N2PoUXXyN7Dyt3prMAq1p1QXd8nRaxZy8N9RGwvT4C";
        assert_eq!(alternate_address(sekr).as_deref(), Some(xkr));
        assert_eq!(alternate_address(xkr).as_deref(), Some(sekr));
        assert_eq!(alternate_address("nope"), None);
    }
}
