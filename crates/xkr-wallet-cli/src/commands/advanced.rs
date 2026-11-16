//! The advanced commands that are not about sending or the address book
//! (`src/zedwallet++/command_implementations.cpp` `changePassword`, `getTxPrivateKey`,
//! `createIntegratedAddress`, `listTransfers`, `reset`, `save`, `saveCSV`, `swapNode`), and the
//! subwallet commands of the Rust wallet (`WalletBackend::addSubWallet`, `importSubWallet`,
//! `importViewSubWallet`, `getBalances`).

use std::fmt::Write as _;
use xkr_types::Hash;
use xkr_wallet::validate::{create_integrated_address, validate_address, validate_payment_id};

use crate::ask::error_text;
use crate::csv::{CSV_FILENAME, csv_text};
use crate::format;
use crate::input::Tone;
use crate::session::{NodeFactory, OpenedOf, Session};

impl<F: NodeFactory> Session<F> {
    /// `changePassword`.
    pub(crate) fn change_password(&mut self, w: &mut OpenedOf<F>) {
        if !self.confirm_password(&w.password, "Confirm your current password: ") {
            return;
        }
        let Some(password) = self.console.read_new_password("  New password › ") else {
            return;
        };
        let result = self
            .rt
            .block_on(w.wallet.change_password(password.as_str()));
        w.password = password;
        let p = self.paint;
        match result {
            Ok(()) => self.say(&format!("{}\n", p.ok("Your password has been changed!"))),
            Err(_) => self.say(&format!(
                "{}\n",
                p.warn(
                    "Your password has been changed, but saving the updated wallet failed. If \
                     you quit without saving succeeding, your password may not update."
                )
            )),
        }
    }

    /// `getTxPrivateKey`.
    pub(crate) fn get_tx_private_key(&mut self, w: &OpenedOf<F>) {
        let Some(text) = self.get_hash(
            "What transaction hash do you want to get the private key of?: ",
            true,
        ) else {
            return;
        };
        let Ok(hash) = text.parse::<Hash>() else {
            return;
        };
        let p = self.paint;
        match w.wallet.tx_private_key(&hash) {
            Ok(key) => self.say(&format!(
                "{}{}\n",
                p.info("Transaction private key: "),
                p.ok(&key.to_hex())
            )),
            Err(e) => self.say(&format!("{}\n", p.warn(e.message()))),
        }
    }

    /// `createIntegratedAddress`.
    pub(crate) fn make_integrated_address(&mut self) {
        let p = self.paint;
        self.say(&format!(
            "{}{}\n\n",
            p.info("Creating an integrated address from an "),
            p.info("address and payment ID pair...")
        ));
        let address = loop {
            let Some(line) = self.console.read_line("Address: ", Tone::Info) else {
                return;
            };
            let address = line.trim();
            match validate_address(address, false) {
                Ok(_) => break address.to_owned(),
                Err(e) => self.say(&format!(
                    "{}{}\n",
                    p.warn("Invalid address: "),
                    p.warn(e.message())
                )),
            }
        };
        let payment_id = loop {
            let Some(line) = self.console.read_line("Payment ID: ", Tone::Info) else {
                return;
            };
            let payment_id = line.trim();
            match validate_payment_id(payment_id) {
                Ok(_) => break payment_id.to_owned(),
                Err(e) => self.say(&format!(
                    "{}{}\n",
                    p.warn("Invalid payment ID: "),
                    p.warn(e.message())
                )),
            }
        };
        match create_integrated_address(&address, &payment_id) {
            Ok(integrated) => self.say(&format!("{}\n", p.info(&integrated))),
            Err(e) => self.say(&format!(
                "{}{}\n",
                p.warn("Failed to create integrated address: "),
                p.warn(e.message())
            )),
        }
    }

    /// `listTransfers`: confirmed transactions, then the sent ones still in the pool; fusions
    /// are left out.
    pub(crate) fn list_transfers(&self, w: &OpenedOf<F>, incoming: bool, outgoing: bool) {
        let p = self.paint;
        let mut transactions = w.wallet.transactions(None, None).unwrap_or_default();
        transactions.extend(w.wallet.unconfirmed(None).unwrap_or_default());
        let (mut spent, mut received) = (0u64, 0u64);
        let (mut outgoing_count, mut incoming_count) = (0u64, 0u64);
        let mut text = String::new();
        for tx in transactions.iter().filter(|t| !t.is_fusion()) {
            let amount = tx.total_amount();
            if amount < 0 && outgoing {
                text.push_str(&format::outgoing_transfer(p, tx));
                spent = spent.saturating_add(amount.unsigned_abs());
                outgoing_count += 1;
            } else if amount > 0 && incoming {
                text.push_str(&format::incoming_transfer(p, tx));
                received = received.saturating_add(amount.unsigned_abs());
                incoming_count += 1;
            }
        }
        text.push_str(&p.info("Summary:\n\n"));
        if incoming {
            let _ = writeln!(
                text,
                "{}",
                p.ok(&format!(
                    "{incoming_count} incoming transactions, totalling {}",
                    format::amount(received)
                ))
            );
        }
        if outgoing {
            let _ = writeln!(
                text,
                "{}",
                p.warn(&format!(
                    "{outgoing_count} outgoing transactions, totalling {}",
                    format::amount(spent)
                ))
            );
        }
        self.say(&text);
    }

    /// `txs`: every transaction, newest first (those waiting for a block on top), one line
    /// each: date · ± amount · short hash · payment ID. `txs in` / `txs out` keep only one
    /// direction. Fusions move nothing and are left out.
    pub(crate) fn txs(&self, w: &OpenedOf<F>, filter: &str) {
        let p = self.paint;
        let (incoming, outgoing) = match filter.to_lowercase().as_str() {
            "" => (true, true),
            "in" => (true, false),
            "out" => (false, true),
            other => {
                self.say(&format!(
                    "  {} · txs, txs in or txs out\n",
                    p.warn(&format!("Unknown filter: {other}"))
                ));
                return;
            }
        };
        let mut transactions = w.wallet.unconfirmed(None).unwrap_or_default();
        let mut confirmed = w.wallet.transactions(None, None).unwrap_or_default();
        confirmed.sort_by_key(|t| std::cmp::Reverse(t.block_height));
        transactions.extend(confirmed);
        let mut text = String::new();
        let mut count = 0usize;
        for tx in transactions.iter().filter(|t| !t.is_fusion()) {
            let total = tx.total_amount();
            if (total > 0 && !incoming) || (total < 0 && !outgoing) || total == 0 {
                continue;
            }
            count += 1;
            let when = if tx.is_unconfirmed() {
                format!("{:<16}", "waiting")
            } else {
                format::date(tx.timestamp)
            };
            let sign = if total > 0 { "+" } else { "-" };
            let amount = format!("{sign}{} XKR", format::amount_short(total.unsigned_abs()));
            let amount = if total > 0 {
                p.ok(&amount)
            } else {
                p.warn(&amount)
            };
            let _ = write!(
                text,
                "  {} {} {amount} {} {}",
                p.dim(&when),
                p.dim("·"),
                p.dim("·"),
                format::short_address(&tx.hash.to_hex())
            );
            if !tx.payment_id.is_empty() {
                let _ = write!(
                    text,
                    " {} pid {}",
                    p.dim("·"),
                    format::short_address(&tx.payment_id)
                );
            }
            text.push('\n');
        }
        if count == 0 {
            let _ = writeln!(text, "  {}", p.dim("No transactions yet"));
        } else {
            let _ = writeln!(
                text,
                "  {}",
                p.dim(&format!(
                    "{count} transaction{}",
                    if count == 1 { "" } else { "s" }
                ))
            );
        }
        self.say(&text);
    }

    /// `reset`: scans again from a height, showing what it finds.
    pub(crate) fn reset(&mut self, w: &OpenedOf<F>) {
        let Some(height) = self.get_scan_height() else {
            return;
        };
        let p = self.paint;
        self.say(&format!(
            "\n{}\n{}{}\n\n",
            p.info("This process may take some time to complete."),
            p.info("You can't make any transactions during the "),
            p.info("process.")
        ));
        if !self.confirm("Are you sure?", true) {
            return;
        }
        self.say(&format!("{}\n", p.info("Resetting wallet...")));
        if let Some(monitor) = &self.monitor {
            monitor.pause(true);
        }
        if let Err(e) = self.rt.block_on(w.wallet.reset(height)) {
            self.say(&format!(
                "{}{}\n",
                p.warn("Failed to save wallet! Error: "),
                p.warn(&error_text(&e))
            ));
        }
        self.sync_wallet(w);
        if let Some(monitor) = &self.monitor {
            monitor.pause(false);
        }
    }

    /// `save`.
    pub(crate) fn save(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        self.say(&format!("{}\n", p.info("Saving.")));
        match self.rt.block_on(w.wallet.save()) {
            Ok(()) => self.say(&format!("{}\n", p.info("Saved."))),
            Err(e) => self.say(&format!(
                "{}{}\n",
                p.warn("Failed to save wallet! Error: "),
                p.warn(&error_text(&e))
            )),
        }
    }

    /// `saveCSV`: `transactions.csv` in the session directory.
    pub(crate) fn save_csv(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let transactions = w.wallet.transactions(None, None).unwrap_or_default();
        if transactions.is_empty() {
            self.say(&p.warn("You have no transactions to save to the CSV!\n"));
            return;
        }
        self.say(&format!("{}\n", p.info("Saving CSV file...")));
        if std::fs::write(self.path_of(CSV_FILENAME), csv_text(&transactions)).is_err() {
            self.say(&format!(
                "{}\n{}\n",
                p.warn("Couldn't open transactions.csv file for saving!"),
                p.warn("Ensure it is not open in any other application.")
            ));
            return;
        }
        self.say(&format!(
            "{}{}{}\n",
            p.ok("CSV successfully written to "),
            p.ok(CSV_FILENAME),
            p.ok("!")
        ));
    }

    /// `swapNode` (also the node-down menu's `swap_node`).
    pub(crate) fn swap_node(&mut self, w: &OpenedOf<F>) {
        let (host, port) = self.get_daemon_address();
        let p = self.paint;
        self.say(&p.info("\nSwapping node, this may take some time...\n"));
        match self.nodes.connect(&host, port) {
            Ok(node) => {
                self.rt.block_on(w.wallet.swap_node(node));
                self.say(&p.ok("Node swap complete.\n\n"));
            }
            Err(e) => self.say(&format!(
                "{}{}\n",
                p.warn("Failed to connect to the daemon: "),
                p.warn(&error_text(&e))
            )),
        }
    }

    /// `subwallets`: every address with its balance, the primary address first.
    pub(crate) fn subwallets(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let primary = w.wallet.primary_address();
        let mut text = String::new();
        for (i, balance) in w.wallet.balances().iter().enumerate() {
            let label = if balance.address == primary {
                format!("Address {} (primary): ", i + 1)
            } else {
                format!("Address {}: ", i + 1)
            };
            let _ = writeln!(
                text,
                "{}{}\nAvailable balance: {}\nLocked (unconfirmed) balance: {}\n",
                p.info(&label),
                p.ok(&balance.address),
                p.ok(&format::amount(balance.unlocked)),
                p.warn(&format::amount(balance.locked))
            );
        }
        self.say(&text);
    }

    /// `create_subwallet`: a new address with its own random spend key.
    pub(crate) fn create_subwallet(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        match self.rt.block_on(w.wallet.add_subwallet()) {
            Ok((address, key)) => {
                let key = zeroize::Zeroizing::new(key.to_hex());
                self.say(&format!(
                    "{}\n{}\n\n{}\n{}\n\n{}\n",
                    p.info("A new address has been added to your wallet:"),
                    p.ok(&address),
                    p.info("Private spend key of this address:"),
                    p.ok(&key),
                    p.warn(
                        "Your mnemonic seed does not restore this address. Keep a copy of its \
                         private spend key."
                    )
                ));
            }
            Err(e) => self.say(&format!(
                "{}{}\n",
                p.warn("Failed to add a new address: "),
                p.warn(&error_text(&e))
            )),
        }
    }

    /// `import_subwallet`: from a private spend key, or for a view wallet a public spend key.
    pub(crate) fn import_subwallet(&mut self, w: &OpenedOf<F>) {
        let p = self.paint;
        let result = if w.wallet.is_view_wallet() {
            let Some(key) = self.get_public_key("  Public spend key › ") else {
                return;
            };
            let Some(height) = self.get_scan_height() else {
                return;
            };
            (
                self.rt
                    .block_on(w.wallet.import_view_subwallet(key, height)),
                height,
            )
        } else {
            let Some(key) = self.get_private_key("  Private spend key › ") else {
                return;
            };
            let Some(height) = self.get_scan_height() else {
                return;
            };
            (
                self.rt.block_on(w.wallet.import_subwallet(key, height)),
                height,
            )
        };
        match result {
            (Ok(address), height) => self.say(&format!(
                "\n{}{}{}\n{}\n",
                p.info("The address "),
                p.ok(&address),
                p.info(" has been added to your wallet."),
                p.info(&format!(
                    "Its transactions show up once the wallet has scanned from block {height}."
                ))
            )),
            (Err(e), _) => self.say(&format!(
                "{}{}\n",
                p.warn("Failed to import the address: "),
                p.warn(&error_text(&e))
            )),
        }
    }
}
