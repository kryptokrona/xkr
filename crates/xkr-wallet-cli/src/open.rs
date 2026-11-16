//! Creating, opening and restoring wallets (`src/zedwallet++/open.cpp` `createWallet`,
//! `openWallet`, `importWalletFromSeed`, `importWalletFromKeys`, `importViewWallet`,
//! `getExistingWalletFileName`, `getNewWalletFileName`, `viewWalletMsg`), in the compact
//! style of the send flow: a title, `  Name ›`-style prompts, `  ✔ Restored my.wallet · …`
//! or `  ✗ <reason>`. The answers are asked in the C++ order and checked by the C++ rules,
//! so scripts written for the C++ wallets still answer the right questions.
//!
//! New wallets are `WalletBackend` files (format A, as `xkrwallet-beta` writes them); `open`
//! takes either format, so `WalletGreen` containers of the old `xkrwallet` keep their file (the
//! engine keeps its state next to it in `<file>.xkrstate`).

use std::sync::Arc;

use xkr_wallet::validate::validate_address;
use xkr_wallet::{ErrorCode, Node, Wallet, WalletError};
use xkr_wallet_core::secret_from_mnemonic;
use zeroize::Zeroizing;

use crate::ask::error_text;
use crate::format::short_address;
use crate::input::Tone;
use crate::session::{NodeFactory, Opened, OpenedOf, Session};
use crate::shutdown::file_kind;

/// The prompt of every new password.
const NEW_PASSWORD: &str = "  Password › ";

impl<F: NodeFactory> Session<F> {
    /// A connection to the daemon of `--remote-daemon`.
    fn connect_node(&self) -> Option<Node<F::Transport>> {
        match self
            .nodes
            .connect(&self.options.daemon_host, self.options.daemon_port)
        {
            Ok(node) => Some(node),
            Err(e) => {
                self.fail_line(&format!("No connection to the node: {}", error_text(&e)));
                None
            }
        }
    }

    /// Wraps an engine result: the opened wallet, or `  ✗ <reason>`.
    fn opened(
        &self,
        result: Result<Wallet<F::Transport>, WalletError>,
        location: String,
        password: Zeroizing<String>,
    ) -> Option<OpenedOf<F>> {
        match result {
            Ok(wallet) => Some(Opened {
                wallet: Arc::new(wallet),
                location,
                password,
            }),
            Err(e) => {
                self.fail_line(&error_text(&e));
                None
            }
        }
    }

    /// The "Wallet file" step around `work`, which opens or writes the file (the password
    /// derivation takes a moment).
    fn wallet_step(
        &self,
        verb: &str,
        location: &str,
        work: impl FnOnce() -> Result<Wallet<F::Transport>, WalletError>,
    ) -> Result<Wallet<F::Transport>, WalletError> {
        self.steps()
            .run("Wallet file", work, |result| match result {
                Ok(wallet) => Ok(format!("{verb} {location}, {}", file_kind(wallet.format()))),
                Err(e) => Err(error_text(e)),
            })
    }

    /// `getNewWalletFileName`: `  Name ›`, giving `<name>.wallet` that does not exist yet.
    fn get_new_wallet_file_name(&mut self) -> Option<String> {
        loop {
            let line = self.console.read_line("  Name › ", Tone::Bold)?;
            let name = line.trim();
            let file = format!("{name}.wallet");
            if name.is_empty() {
                self.fail_line("The name can't be empty");
            } else if self.path_of(&file).exists() {
                self.fail_line(&format!("{file} already exists · try another name"));
            } else {
                return Some(file);
            }
        }
    }

    /// `getExistingWalletFileName`: `name` or `name.wallet`; `--wallet-file` is tried first
    /// when `given`.
    fn get_existing_wallet_file_name(&mut self, given: bool) -> Option<String> {
        let mut initial = given;
        loop {
            let name = match (initial, &self.options.wallet_file) {
                (true, Some(file)) => file.clone(),
                _ => self.console.read_line("  Name › ", Tone::Bold)?,
            };
            initial = false;
            let name = name.trim().to_owned();
            let with_extension = format!("{name}.wallet");
            if name.is_empty() {
                self.fail_line("The name can't be empty");
            } else if self.path_of(&name).is_file() {
                return Some(name);
            } else if self.path_of(&with_extension).is_file() {
                return Some(with_extension);
            } else {
                self.fail_line(&format!("No wallet named {name} or {with_extension} here"));
            }
        }
    }

    /// `createWallet`: `  ✔ Created my.wallet`, then the address, the seed and the keys to
    /// write down.
    pub(crate) fn create_wallet(&mut self) -> Option<OpenedOf<F>> {
        self.flow_title("New wallet");
        let location = self.get_new_wallet_file_name()?;
        let password = self.console.read_new_password(NEW_PASSWORD)?;
        let node = self.connect_node()?;
        let result = self.wallet_step("created", &location, || {
            self.rt.block_on(Wallet::create(
                self.path_of(&location),
                password.as_str(),
                node,
                self.config.wallet.clone(),
            ))
        });
        let opened = self.opened(result, location, password)?;
        self.ok_line(&format!("Created {}", opened.location));
        self.prompt_save_keys(&opened);
        Some(opened)
    }

    /// `openWallet`: asks again after a wrong password (`--password` is used once only).
    pub(crate) fn open_wallet(&mut self) -> Option<OpenedOf<F>> {
        self.flow_title("Open a wallet");
        let given = std::mem::take(&mut self.given_open);
        let location = self.get_existing_wallet_file_name(given)?;
        let mut use_given_password = given;
        loop {
            let password = match (use_given_password, &self.options.password) {
                (true, Some(password)) => password.clone(),
                _ => self.console.read_password("  Password › ")?,
            };
            use_given_password = false;
            let node = self.connect_node()?;
            let result = self.wallet_step("opened", &location, || {
                self.rt.block_on(Wallet::open(
                    self.path_of(&location),
                    password.as_str(),
                    node,
                    self.config.wallet.clone(),
                ))
            });
            match result {
                Err(e) if e.code() == ErrorCode::WrongPassword => {
                    self.fail_line("Incorrect password! Try again.");
                }
                other => {
                    let opened = self.opened(other, location, password)?;
                    self.done_line("Opened", &opened);
                    return Some(opened);
                }
            }
        }
    }

    /// `  ✔ Restored my.wallet · SEKReX4M…yBZCf`.
    fn done_line(&self, verb: &str, opened: &OpenedOf<F>) {
        let mut text = format!(
            "{verb} {} · {}",
            opened.location,
            short_address(&opened.wallet.primary_address())
        );
        if opened.wallet.is_view_wallet() {
            text.push_str(" · view only");
        }
        self.ok_line(&text);
    }

    /// `importWalletFromSeed`.
    pub(crate) fn import_from_seed(&mut self) -> Option<OpenedOf<F>> {
        self.flow_title("Restore from a seed");
        let seed = loop {
            let line = Zeroizing::new(self.console.read_line("  Seed (25 words) › ", Tone::Bold)?);
            let seed = Zeroizing::new(line.trim().to_owned());
            match secret_from_mnemonic(&seed) {
                Ok(_) => break seed,
                Err(e) => self.fail_line(&e.to_string()),
            }
        };
        let location = self.get_new_wallet_file_name()?;
        let password = self.console.read_new_password(NEW_PASSWORD)?;
        let scan_height = self.get_scan_height()?;
        let node = self.connect_node()?;
        let result = self.wallet_step("imported", &location, || {
            self.rt.block_on(Wallet::import_seed(
                self.path_of(&location),
                password.as_str(),
                seed.as_str(),
                scan_height,
                node,
                self.config.wallet.clone(),
            ))
        });
        let opened = self.opened(result, location, password)?;
        self.done_line("Restored", &opened);
        Some(opened)
    }

    /// `importWalletFromKeys` (spend key first, as C++ asks).
    pub(crate) fn import_from_keys(&mut self) -> Option<OpenedOf<F>> {
        self.flow_title("Restore from private keys");
        let spend = self.get_private_key("  Private spend key › ")?;
        let view = self.get_private_key("  Private view key › ")?;
        let location = self.get_new_wallet_file_name()?;
        let password = self.console.read_new_password(NEW_PASSWORD)?;
        let scan_height = self.get_scan_height()?;
        let node = self.connect_node()?;
        let result = self.wallet_step("imported", &location, || {
            self.rt.block_on(Wallet::import_keys(
                self.path_of(&location),
                password.as_str(),
                spend,
                view,
                scan_height,
                node,
                self.config.wallet.clone(),
            ))
        });
        let opened = self.opened(result, location, password)?;
        self.done_line("Restored", &opened);
        Some(opened)
    }

    /// `importViewWallet`.
    pub(crate) fn import_view_wallet(&mut self) -> Option<OpenedOf<F>> {
        self.flow_title("View-only wallet");
        self.say(&format!(
            "  {}\n",
            self.paint
                .dim("A view-only wallet sees incoming transfers only and cannot send.")
        ));
        if !self.ask_yes_no("Go on?", true) {
            return None;
        }
        let view = self.get_private_key("  Private view key › ")?;
        let address = loop {
            let line = self.console.read_line("  Address › ", Tone::Bold)?;
            let address = line.trim();
            match validate_address(address, false) {
                Ok(_) => break address.to_owned(),
                Err(e) => self.fail_line(&format!("Invalid address: {}", e.message())),
            }
        };
        let location = self.get_new_wallet_file_name()?;
        let password = self.console.read_new_password(NEW_PASSWORD)?;
        let scan_height = self.get_scan_height()?;
        let node = self.connect_node()?;
        let result = self.wallet_step("imported", &location, || {
            self.rt.block_on(Wallet::import_view(
                self.path_of(&location),
                password.as_str(),
                view,
                &address,
                scan_height,
                node,
                self.config.wallet.clone(),
            ))
        });
        let opened = self.opened(result, location, password)?;
        self.done_line("Imported", &opened);
        self.say(&format!(
            "  {}\n",
            self.paint.dim(
                "Spent funds do not show in a view-only wallet, so its balance can look higher \
                 than it is."
            )
        ));
        Some(opened)
    }
}
