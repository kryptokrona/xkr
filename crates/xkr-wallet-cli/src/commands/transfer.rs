//! `transfer`, `send_all` and `optimize` (`src/zedwallet++/transfer.cpp` `transfer`,
//! `sendTransaction`, `splitTX`, `confirmTransaction`; `src/zedwallet++/fusion.cpp`
//! `optimize`, `optimizeRound`; `command_dispatcher.cpp` for the `optimize` prompt).

use std::fmt::Write as _;
use xkr_consensus::params::MINIMUM_FEE;
use xkr_daemon_client::Transport;
use xkr_types::Hash;
use xkr_wallet::validate::{STANDARD_ADDRESS_LENGTH, STANDARD_ADDRESS_LENGTH_ALT};
use xkr_wallet::{ErrorCode, WalletError};

use crate::ask::{MINIMUM_SEND, error_text};
use crate::format;
use crate::session::{NodeFactory, Opened, OpenedOf, Session};

/// A standard (not integrated) address of either prefix, by its length as C++ checks it.
pub(crate) fn is_standard_length(address: &str) -> bool {
    address.len() == STANDARD_ADDRESS_LENGTH || address.len() == STANDARD_ADDRESS_LENGTH_ALT
}

/// `TOO_MANY_INPUTS_TO_FIT_IN_BLOCK`.
fn too_big(result: &Result<Hash, WalletError>) -> bool {
    matches!(result, Err(e) if e.code() == ErrorCode::TooManyInputsToFitInBlock)
}

/// `getTotalUnlockedBalance`.
pub(crate) fn unlocked<T: Transport + 'static>(w: &Opened<T>) -> u64 {
    w.wallet.balance(None).map_or(0, |b| b.unlocked)
}

/// The node's fee per transaction (0 without one).
fn node_fee<T: Transport + 'static>(w: &Opened<T>) -> u64 {
    w.wallet.node().fee_amount
}

impl<F: NodeFactory> Session<F> {
    /// Runs a send unless the wallet is closing, and tells closing how it ended, so closing
    /// never cuts a send in half.
    fn guarded(
        &self,
        send: impl FnOnce() -> Result<Hash, WalletError>,
    ) -> Result<Hash, WalletError> {
        if !self.gate.begin() {
            return Err(WalletError::with_message(
                ErrorCode::DaemonError,
                "The wallet is closing; the transaction was not sent.",
            ));
        }
        let result = send();
        self.gate.end(match &result {
            Ok(hash) => Ok(format!("sent, hash {hash}")),
            Err(e) => Err(format!("not sent: {}", error_text(e))),
        });
        result
    }

    /// `cancel()`.
    pub(crate) fn cancel_transaction(&self) {
        self.say(&format!(
            "  {}\n",
            self.paint.warn("Cancelled · nothing was sent")
        ));
    }

    /// `transfer(walletBackend, sendAll)`.
    pub(crate) fn transfer(&mut self, w: &OpenedOf<F>, send_all: bool) {
        let p = self.paint;
        let unlocked = unlocked(w);
        if send_all && unlocked <= MINIMUM_SEND {
            self.say(&format!(
                "  {}\n",
                p.warn(&format!(
                    "Nothing to send · the minimum is {} XKR and you have {} XKR",
                    format::amount_short(MINIMUM_SEND),
                    format::amount_short(unlocked)
                ))
            ));
            return;
        }
        self.say(&format!("  {}\n", p.dim("cancel stops at any question")));
        let Some(address) = self.get_address("  Address › ", true, true) else {
            self.cancel_transaction();
            return;
        };
        let mut payment_id = String::new();
        if is_standard_length(&address) {
            let Some(id) = self.get_payment_id(true) else {
                self.cancel_transaction();
                return;
            };
            payment_id = id;
        }
        let mut amount = unlocked
            .saturating_sub(node_fee(w))
            .saturating_sub(MINIMUM_FEE);
        if !send_all {
            let typed = self.get_amount("  Amount › ", true);
            let Some(typed) = typed else {
                self.cancel_transaction();
                return;
            };
            amount = typed;
        }
        self.send_transaction(w, &address, amount, &payment_id);
    }

    /// `sendTransaction`: checks the balance, confirms, sends; a transaction too large for a
    /// block is retried after `optimize` and then split.
    pub(crate) fn send_transaction(
        &mut self,
        w: &OpenedOf<F>,
        address: &str,
        amount: u64,
        payment_id: &str,
    ) {
        let p = self.paint;
        let unlocked = unlocked(w);
        let node_fee = node_fee(w);
        let needed = amount.saturating_add(MINIMUM_FEE).saturating_add(node_fee);
        if needed > unlocked {
            self.say(&format!(
                "  {} · {} XKR needed with the fees, {} XKR available\n",
                p.warn("Not enough XKR"),
                format::amount_short(needed),
                format::amount_short(unlocked)
            ));
            self.cancel_transaction();
            return;
        }
        if !self.confirm_transaction(w, address, amount, payment_id, node_fee) {
            self.cancel_transaction();
            return;
        }
        let mut result = self.guarded(|| {
            self.rt
                .block_on(w.wallet.send_basic(address, amount, payment_id))
        });
        if too_big(&result) {
            self.say(&format!(
                "{}{}",
                p.warn("Your transaction is too large to be accepted by the network!\n"),
                p.info(
                    "We're attempting to optimize your wallet,\nwhich hopefully make the \
                     transaction small enough to fit in a block.\nPlease wait, this will take \
                     some time...\n\n"
                )
            ));
            self.optimize(w);
            result = self.guarded(|| {
                self.rt
                    .block_on(w.wallet.send_basic(address, amount, payment_id))
            });
            if too_big(&result) {
                self.split_transaction(w, address, amount, payment_id);
                return;
            }
        }
        match result {
            Ok(hash) => self.say(&format!(
                "  {} {} · hash {}\n",
                p.good("✔"),
                p.bold("Sent"),
                hash.to_hex()
            )),
            Err(e) => self.say(&format!(
                "  {} {} · {}\n",
                p.warn("✗"),
                p.bold("Not sent"),
                p.warn(&error_text(&e))
            )),
        }
    }

    /// `confirmTransaction`: the fees and the payment ID, "Send … to …? [y/N]" and the
    /// password.
    fn confirm_transaction(
        &mut self,
        w: &OpenedOf<F>,
        address: &str,
        amount: u64,
        payment_id: &str,
        node_fee: u64,
    ) -> bool {
        let p = self.paint;
        let total = amount.saturating_add(MINIMUM_FEE).saturating_add(node_fee);
        let mut text = format!(
            "  {} {} XKR",
            p.dim("Fee"),
            format::amount_short(MINIMUM_FEE)
        );
        if node_fee != 0 {
            let _ = write!(
                text,
                " · {} {} XKR",
                p.dim("node fee"),
                format::amount_short(node_fee)
            );
        }
        let _ = writeln!(
            text,
            " · {} {} XKR",
            p.dim("total"),
            format::amount_short(total)
        );
        if !payment_id.is_empty() {
            let _ = writeln!(text, "  {} {payment_id}", p.dim("Payment ID"));
        }
        self.say(&text);
        let question = format!(
            "Send {} XKR to {}?",
            format::amount_short(amount),
            format::short_address(address)
        );
        self.ask_yes_no(&question, false) && self.confirm_password(&w.password, "  Password › ")
    }

    /// `splitTX`: sends the amount in parts, halving a part that is still too large and
    /// waiting for change to unlock between parts.
    fn split_transaction(&mut self, w: &OpenedOf<F>, address: &str, amount: u64, payment_id: &str) {
        let p = self.paint;
        self.say(&format!(
            "{}{}",
            p.info("Transaction is still too large to send, splitting into multiple chunks.\n\n"),
            p.warn(
                "It will slightly raise the fee you have to pay,\nand hence reduce the total \
                 amount you can send if\nyour balance cannot cover it.\n\nIf the node you are \
                 using charges a fee,\nyou will have to pay this fee for each transction.\n"
            )
        ));
        if !self.confirm("Is this OK?", true) {
            self.cancel_transaction();
            return;
        }
        let node_fee = node_fee(w);
        let fees = MINIMUM_FEE.saturating_add(node_fee);
        let mut available = unlocked(w);
        let mut sent = 0u64;
        let mut divider = 1u64;
        let mut number = 1u64;
        loop {
            let remainder = amount - sent;
            let mut part = remainder / divider;
            // an odd remainder: the last part takes what is left
            if part != remainder && remainder < part.saturating_mul(2) {
                part = remainder;
            }
            let mut needed = part.saturating_add(fees);
            if needed > available {
                part = available.saturating_sub(fees);
                needed = part.saturating_add(fees);
            }
            if part < MINIMUM_SEND {
                self.say(&p.warn("Failed to split up transaction, sorry.\n"));
                return;
            }
            while unlocked(w) < needed {
                self.say(&p.warn(&format!(
                    "Waiting for balance to unlock to send next transaction.\nWill try again \
                     in {} seconds...\n\n",
                    self.config.timing.wait.as_secs()
                )));
                std::thread::sleep(self.config.timing.wait);
            }
            let result = self.guarded(|| {
                self.rt
                    .block_on(w.wallet.send_basic(address, part, payment_id))
            });
            if too_big(&result) {
                divider = divider.saturating_mul(2);
                self.say(&p.info("Working...\n"));
                continue;
            }
            let hash = match result {
                Ok(hash) => hash,
                Err(e) => {
                    self.say(&format!(
                        "{}{}\nAborting, sorry...\n",
                        p.warn("Failed to send transaction: "),
                        error_text(&e)
                    ));
                    return;
                }
            };
            self.say(&format!(
                "{}\n",
                p.ok(&format!(
                    "Transaction number {number} has been sent!\nHash: {hash}\nAmount: {}\n\n",
                    format::amount(part)
                ))
            ));
            number += 1;
            sent += part;
            available = available.saturating_sub(needed);
            if sent >= amount {
                self.say(&p.info("All transactions have been sent!\n"));
                return;
            }
            divider = 1;
        }
    }

    /// The `optimize` command: asks first.
    pub(crate) fn optimize_command(&mut self, w: &OpenedOf<F>) {
        let p = self.paint;
        self.say(&format!(
            "Attempting to optimize your wallet to allow you to send large amounts at once.\n{}",
            p.warn("This may take a very long time!\n")
        ));
        if self.confirm("Do you want to proceed?", true) {
            self.optimize(w);
        } else {
            self.say(&format!("{}\n", p.warn("Cancelling optimization.")));
        }
    }

    /// `optimize`: rounds of fusion transactions until a round sends none.
    pub(crate) fn optimize(&self, w: &OpenedOf<F>) {
        let p = self.paint;
        let mut round = 1u32;
        loop {
            self.say(&p.info(&format!("Running optimization round {round}...\n\n")));
            if !self.optimize_round(w) {
                break;
            }
            round += 1;
        }
        self.say(&p.ok("Optimization complete!\n"));
    }

    /// `optimizeRound`: fusions until five attempts in a row send nothing, then waits until
    /// the balance has come back unlocked. True if any fusion was sent.
    fn optimize_round(&self, w: &OpenedOf<F>) -> bool {
        let p = self.paint;
        let mut failures = 0;
        let mut sent = 0u64;
        let initial = unlocked(w);
        while failures < 5 {
            match self.guarded(|| self.rt.block_on(w.wallet.fusion_basic())) {
                Err(e) if e.code() == ErrorCode::FullyOptimized => failures += 1,
                Err(e) => {
                    failures += 1;
                    self.say(&p.warn(&format!(
                        "Failed to send fusion transaction: {}\n",
                        error_text(&e)
                    )));
                }
                Ok(hash) => {
                    failures = 0;
                    sent += 1;
                    self.say(&format!(
                        "{}{}{}{}\n\n",
                        p.info("Sent fusion transaction #"),
                        p.info(&sent.to_string()),
                        p.ok("\nHash: "),
                        p.ok(&hash.to_hex())
                    ));
                }
            }
        }
        let mut current = unlocked(w);
        while current < initial {
            self.say(&format!(
                "{}{}{}{}{}{}{}",
                p.info("Waiting for balance to return and unlock:\n\nTotal balance: "),
                p.info(&format::amount(initial)),
                p.warn("\nLocked balance: "),
                p.warn(&format::amount(initial - current)),
                p.ok("\nUnlocked balance: "),
                p.ok(&format::amount(current)),
                p.info(&format!(
                    "\nWill check again in {} seconds...\n\n",
                    self.config.timing.wait.as_secs()
                ))
            ));
            std::thread::sleep(self.config.timing.wait);
            current = unlocked(w);
        }
        if sent != 0 {
            self.say(&p.ok("All fusion transactions confirmed!\n\n"));
        }
        sent != 0
    }
}
