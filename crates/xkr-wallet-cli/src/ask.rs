//! The questions the commands ask (`src/zedwallet++/get_input.cpp` `getAddress`,
//! `getPaymentID`, `getHash`, `getAmountToAtomic`, `getDaemonAddress`;
//! `src/zedwallet++/utilities.cpp` `confirm`, `confirmPassword`, `getScanHeight`;
//! `src/zedwallet++/open.cpp` `getPrivateKey`).
//!
//! Each returns `None` when the user typed `cancel` (where allowed) or the input ended.

use xkr_consensus::params::CRYPTONOTE_NAME;
use xkr_types::{PublicKey, SecretKey};
use xkr_wallet::validate::{validate_address, validate_hash, validate_payment_id};
use xkr_wallet::{ErrorCode, WalletError};
use zeroize::Zeroizing;

use crate::args;
use crate::format;
use crate::input::Tone;
use crate::session::{NodeFactory, Session};

/// `wallet_config::numDecimalPlaces`.
const DECIMAL_PLACES: usize = 5;
/// `wallet_config::minimumSend`.
pub(crate) const MINIMUM_SEND: u64 = 1;

/// The leading decimal digits of `text` (after an optional `+`) as `std::stoull` reads them.
enum Unsigned {
    None,
    Value(u64),
    OutOfRange,
}

fn leading_u64(text: &str) -> Unsigned {
    let s = text.trim_start();
    let s = s.strip_prefix('+').unwrap_or(s);
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    if end == 0 {
        return Unsigned::None;
    }
    s[..end]
        .parse()
        .map_or(Unsigned::OutOfRange, Unsigned::Value)
}

/// An error's message, with what the node or the file said when there is more to tell.
pub(crate) fn error_text(e: &WalletError) -> String {
    match e.detail() {
        Some(detail) if e.code() != ErrorCode::WrongPassword => {
            format!("{} ({detail})", e.message())
        }
        _ => e.message().to_owned(),
    }
}

impl<F: NodeFactory> Session<F> {
    /// `zed_utilities::confirm`: `(Y/n)` or `(y/N)`; enter gives the default. The end of the
    /// input answers no.
    pub(crate) fn confirm(&mut self, message: &str, default_yes: bool) -> bool {
        let suffix = if default_yes { " (Y/n): " } else { " (y/N): " };
        loop {
            let Some(answer) = self
                .console
                .read_line(&format!("{message}{suffix}"), Tone::Info)
            else {
                return false;
            };
            match answer.chars().next().map(|c| c.to_ascii_lowercase()) {
                None => return default_yes,
                Some('y') => return true,
                Some('n') => return false,
                Some(_) => {
                    let p = self.paint;
                    self.say(&format!(
                        "{}{}{}\n",
                        p.warn("Bad input: "),
                        p.info(&answer),
                        p.warn(" - please enter either Y or N.")
                    ));
                }
            }
        }
    }

    /// A yes/no question in the compact style: `  Send 1.00 XKR to SEKReZCo…ZpBZn? [y/N] › `.
    /// Enter gives the default; the end of the input answers no.
    pub(crate) fn ask_yes_no(&mut self, question: &str, default_yes: bool) -> bool {
        let choices = if default_yes { "[Y/n]" } else { "[y/N]" };
        loop {
            let Some(answer) = self
                .console
                .read_line(&format!("  {question} {choices} › "), Tone::Bold)
            else {
                return false;
            };
            match answer.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
                None => return default_yes,
                Some('y') => return true,
                Some('n') => return false,
                Some(_) => self.say(&format!("  {}\n", self.paint.warn("Answer y or n"))),
            }
        }
    }

    /// `zed_utilities::confirmPassword`: asks until `password` is typed; false if the input
    /// ended.
    pub(crate) fn confirm_password(&mut self, password: &str, message: &str) -> bool {
        loop {
            let Some(typed) = self.console.read_password(message) else {
                return false;
            };
            if typed.as_str() == password {
                return true;
            }
            self.say(&format!(
                "{}\n",
                self.paint.warn("Incorrect password! Try again.")
            ));
        }
    }

    /// `  ✔ text` (a finished flow).
    pub(crate) fn ok_line(&self, text: &str) {
        self.say(&format!("  {} {text}\n", self.paint.good("✔")));
    }

    /// `  ✗ reason` (a refused answer or a failed flow).
    pub(crate) fn fail_line(&self, reason: &str) {
        let p = self.paint;
        self.say(&format!("  {} {}\n", p.warn("✗"), p.warn(reason)));
    }

    /// A flow's title: `  New wallet`.
    pub(crate) fn flow_title(&self, title: &str) {
        self.say(&format!("\n  {}\n", self.paint.bold(title)));
    }

    /// `zed_utilities::getScanHeight`: `  Scan from height (enter = 0) ›`; enter for 0,
    /// commas are ignored.
    pub(crate) fn get_scan_height(&mut self) -> Option<u64> {
        self.say(&format!(
            "  {}\n",
            self.paint
                .dim("a lower height finds older transactions; 0 scans everything")
        ));
        loop {
            let line = self
                .console
                .read_line("  Scan from height (enter = 0) › ", Tone::Bold)?;
            let text: String = line.trim().chars().filter(|c| *c != ',').collect();
            if text.is_empty() {
                return Some(0);
            }
            match leading_u64(&text) {
                Unsigned::Value(height) => return Some(height),
                Unsigned::OutOfRange => self.fail_line("Input is too large or too small!"),
                Unsigned::None => {
                    self.fail_line("Failed to parse height - input is not a number!");
                }
            }
        }
    }

    /// `getAddress`: a valid address (integrated ones only if allowed).
    pub(crate) fn get_address(
        &mut self,
        message: &str,
        integrated_allowed: bool,
        cancel_allowed: bool,
    ) -> Option<String> {
        loop {
            let line = self.console.read_line(message, Tone::Info)?;
            let address = line.trim();
            if address.is_empty() {
                continue;
            }
            if address == "cancel" && cancel_allowed {
                return None;
            }
            match validate_address(address, integrated_allowed) {
                Ok(_) => return Some(address.to_owned()),
                Err(e) => {
                    let p = self.paint;
                    self.say(&format!(
                        "  {}{}\n",
                        p.warn("Invalid address: "),
                        p.warn(e.message())
                    ));
                }
            }
        }
    }

    /// `getPaymentID`: empty for none. Asked only for a standard address (an integrated
    /// address carries its own).
    pub(crate) fn get_payment_id(&mut self, cancel_allowed: bool) -> Option<String> {
        let p = self.paint;
        self.say(&format!(
            "  {}\n",
            p.dim("If you were given a payment ID, you must use it, or the funds may be lost.")
        ));
        loop {
            let line = self
                .console
                .read_line("  Payment ID (enter for none) › ", Tone::Bold)?;
            let payment_id = line.trim();
            if payment_id == "cancel" && cancel_allowed {
                return None;
            }
            if payment_id.is_empty() {
                return Some(String::new());
            }
            match validate_payment_id(payment_id) {
                Ok(_) => return Some(payment_id.to_owned()),
                Err(e) => self.say(&format!(
                    "  {}{}\n",
                    p.warn("Invalid payment ID: "),
                    p.warn(e.message())
                )),
            }
        }
    }

    /// `getHash`: 64 hex characters.
    pub(crate) fn get_hash(&mut self, message: &str, cancel_allowed: bool) -> Option<String> {
        loop {
            let line = self.console.read_line(message, Tone::Info)?;
            let hash = line.trim();
            if hash == "cancel" && cancel_allowed {
                return None;
            }
            match validate_hash(hash) {
                Ok(_) => return Some(hash.to_owned()),
                Err(e) => {
                    let p = self.paint;
                    self.say(&format!(
                        "{}{}\n",
                        p.warn("Invalid hash: "),
                        p.warn(e.message())
                    ));
                }
            }
        }
    }

    /// `getAmountToAtomic`: XKR with at most five decimals, thousands separators allowed.
    pub(crate) fn get_amount(&mut self, message: &str, cancel_allowed: bool) -> Option<u64> {
        let p = self.paint;
        loop {
            let line = self.console.read_line(message, Tone::Info)?;
            if line.is_empty() {
                continue;
            }
            let mut text: String = line.trim().chars().filter(|c| *c != ',').collect();
            if text == "cancel" && cancel_allowed {
                return None;
            }
            let decimals = text.rfind('.').map_or(0, |i| text.len() - i - 1);
            if decimals > DECIMAL_PLACES {
                self.say(&p.warn(&format!(
                    "{CRYPTONOTE_NAME} transfers can have a max of {DECIMAL_PLACES} decimal \
                     places.\n"
                )));
                continue;
            }
            let dots = text.matches('.').count();
            text.retain(|c| c != '.');
            text.push_str(&"0".repeat(DECIMAL_PLACES - decimals));
            if dots > 1 || text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                self.say(
                    &p.warn("Failed to parse amount! Ensure you entered the value correctly.\n"),
                );
                continue;
            }
            match text.parse::<u64>() {
                Err(_) => self.say(&format!("{}\n", p.warn("Input is too large or too small!"))),
                Ok(amount) if amount < MINIMUM_SEND => self.say(&format!(
                    "{}{}{}",
                    p.warn("The minimum send allowed is "),
                    p.warn(&format::amount(MINIMUM_SEND)),
                    p.warn("!\n")
                )),
                Ok(amount) => return Some(amount),
            }
        }
    }

    /// `getDaemonAddress`: enter (or the end of the input) for `127.0.0.1` and the network's
    /// RPC port.
    pub(crate) fn get_daemon_address(&mut self) -> (String, u16) {
        let p = self.paint;
        let port = self.options.network.params().rpc_port;
        let default = || ("127.0.0.1".to_owned(), port);
        loop {
            self.say(&format!(
                "{}{}.\n\n",
                p.info(
                    "\nEnter the daemon address you want to use.\nYou can omit the port, and it \
                     will default to "
                ),
                p.info(&port.to_string())
            ));
            let Some(line) = self
                .console
                .read_line("Hit enter for the default of localhost: ", Tone::Plain)
            else {
                return default();
            };
            if line.is_empty() {
                return default();
            }
            match args::parse_daemon_address(line.trim(), port) {
                Some(address) => return address,
                None => self.say(&p.warn("\nInvalid daemon address! Try again.\n")),
            }
        }
    }

    /// `getPrivateKey`: 64 hex characters of a reduced scalar.
    pub(crate) fn get_private_key(&mut self, message: &str) -> Option<SecretKey> {
        loop {
            let line = Zeroizing::new(self.console.read_line(message, Tone::Bold)?);
            let text = line.trim();
            if text.len() != 64 {
                self.fail_line("Invalid private key, should be 64 characters! Try again.");
                continue;
            }
            let Ok(key) = text.parse::<SecretKey>() else {
                self.fail_line("Invalid private key, it is not a valid hex string! Try again.");
                continue;
            };
            if xkr_crypto::secret_key_to_public_key(&key).is_none() {
                self.fail_line(
                    "Invalid private key, is not on the ed25519 curve! Probably a typo - ensure \
                     you entered it correctly.",
                );
                continue;
            }
            return Some(key);
        }
    }

    /// A public spend key (64 hex characters of a valid point), for view wallets' addresses.
    pub(crate) fn get_public_key(&mut self, message: &str) -> Option<PublicKey> {
        loop {
            let line = self.console.read_line(message, Tone::Bold)?;
            match line.trim().parse::<PublicKey>() {
                Ok(key) if xkr_crypto::check_key(&key) => return Some(key),
                _ => self.say(&format!(
                    "  {}\n",
                    self.paint.warn(
                        "✗ Invalid public key, it should be 64 hex characters of a valid key! \
                         Try again."
                    )
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_like_stoull() {
        assert!(matches!(leading_u64("42"), Unsigned::Value(42)));
        assert!(matches!(leading_u64(" +7abc"), Unsigned::Value(7)));
        assert!(matches!(leading_u64("abc"), Unsigned::None));
        assert!(matches!(leading_u64("-1"), Unsigned::None));
        assert!(matches!(
            leading_u64("99999999999999999999999"),
            Unsigned::OutOfRange
        ));
    }
}
