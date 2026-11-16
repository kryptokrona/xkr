//! A run of the terminal wallet: the start menu, the node check, the foreground sync, the
//! transaction monitor and the command loop (`src/zedwallet++/zed_wallet.cpp` `main`,
//! `menu.cpp` `selectionScreen` / `checkNodeStatus` / `getAction` / `mainLoop`,
//! `command_dispatcher.cpp` `handleCommand` / `handleLaunchCommand`).
//!
//! The session runs on the calling thread and talks to the wallet engine through a tokio
//! runtime [`Handle`]; the engine's sync tasks run on that runtime. While a command runs it
//! holds the console lock, so the transaction monitor prints between commands only (as the
//! C++ `TransactionMonitor` mutex does).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use xkr_consensus::Network;
use xkr_daemon_client::{HttpTransport, Transport};
use xkr_wallet::{Node, Wallet, WalletConfig, WalletError};
use xkr_wallet_files::address_book::ADDRESS_BOOK_FILENAME;
use zeroize::Zeroizing;

use crate::args::{self, Options};
use crate::commands::{self, Command, Selection};
use crate::format::{self, amount_short, group, short_address};
use crate::input::{Console, Painter};
use crate::monitor::Monitor;
use crate::shutdown::{BookState, Closer, SendGate, ShutdownHandle, SignalOutcome};
use crate::steps::Steps;

/// Connections to daemons (`Nigel(daemonHost, daemonPort)`); tests use an in-process node.
pub trait NodeFactory: Send + Sync + 'static {
    /// The transport of the connections.
    type Transport: Transport + 'static;

    /// A connection to `host:port`.
    fn connect(&self, host: &str, port: u16) -> Result<Node<Self::Transport>, WalletError>;
}

/// Daemons over HTTP.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpNodes;

impl NodeFactory for HttpNodes {
    type Transport = HttpTransport;

    fn connect(&self, host: &str, port: u16) -> Result<Node<HttpTransport>, WalletError> {
        Node::http(host, port)
    }
}

/// Pauses of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    /// Between two progress lines of the foreground sync (`syncWallet`: 3 s).
    pub sync_poll: Duration,
    /// Between two checks while waiting for balance to unlock (`optimizeRound`, `splitTX`:
    /// 15 s).
    pub wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            sync_poll: Duration::from_secs(3),
            wait: Duration::from_secs(15),
        }
    }
}

/// Settings of a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Settings of the wallets the session opens (the network is taken from the options).
    pub wallet: WalletConfig,
    /// Pauses.
    pub timing: Timing,
    /// The directory relative wallet names, `.addressBook.json` and `transactions.csv` are in
    /// (the working directory for the program).
    pub dir: PathBuf,
    /// Save and exit on Ctrl-C (`tools::SignalHandler` in `zed_wallet.cpp`).
    pub handle_signals: bool,
}

impl SessionConfig {
    /// Defaults for `network`, files in the working directory, no signal handling.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            wallet: WalletConfig {
                network,
                ..WalletConfig::default()
            },
            timing: Timing::default(),
            dir: PathBuf::from("."),
            handle_signals: false,
        }
    }
}

/// An open wallet with the file name it was opened under and its password (the C++
/// `getWalletLocation` and `getWalletPassword`).
pub(crate) struct Opened<T> {
    pub wallet: Arc<Wallet<T>>,
    pub location: String,
    pub password: Zeroizing<String>,
}

/// [`Opened`] for the transport of `F`.
pub(crate) type OpenedOf<F> = Opened<<F as NodeFactory>::Transport>;

/// A run of the terminal wallet.
pub struct Session<F: NodeFactory> {
    pub(crate) options: Options,
    pub(crate) nodes: F,
    pub(crate) console: Console,
    pub(crate) rt: Handle,
    pub(crate) config: SessionConfig,
    pub(crate) paint: Painter,
    /// Held while a command runs; the transaction monitor prints only without it.
    pub(crate) lock: Arc<Mutex<()>>,
    pub(crate) monitor: Option<Monitor>,
    /// The next `open` uses `--wallet-file` and `--password`.
    pub(crate) given_open: bool,
    first_action: bool,
    /// Lets closing wait for a send that runs.
    pub(crate) gate: Arc<SendGate>,
    /// The address book's changes in this session.
    pub(crate) book: Arc<BookState>,
    /// Closing, for `exit` and the signals.
    shutdown: ShutdownHandle,
    /// The signal watcher runs (it is started with the first wallet).
    watching_signals: bool,
}

/// How the command loop ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// `exit` or the end of the input: close the wallet and quit the program.
    Quit,
    /// `back`: close the wallet and show the start menu.
    Back,
}

/// How the session goes on after the start menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// Show the sync before the menu.
    Foreground,
    /// A new wallet: sync in the background only.
    Background,
    /// The user chose to exit in the node-down menu.
    Quit,
}

impl<F: NodeFactory> std::fmt::Debug for Session<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("options", &self.options)
            .field("console", &self.console)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// The command prompt: the available balance, `  1 234.56 XKR › `.
#[must_use]
pub(crate) fn prompt_for(unlocked: u64) -> String {
    format!("  {} XKR › ", amount_short(unlocked))
}

impl<F: NodeFactory> Session<F> {
    /// A session with `options`, daemons from `nodes`, on `console`, running the wallet
    /// engine on `rt`.
    #[must_use]
    pub fn new(
        options: Options,
        nodes: F,
        console: Console,
        rt: Handle,
        config: SessionConfig,
    ) -> Self {
        let mut config = config;
        config.wallet.network = options.network;
        let paint = console.paint();
        let book = Arc::new(BookState::new(config.dir.join(ADDRESS_BOOK_FILENAME)));
        let shutdown = ShutdownHandle::new(console.out(), paint);
        Self {
            options,
            nodes,
            console,
            rt,
            config,
            paint,
            lock: Arc::new(Mutex::new(())),
            monitor: None,
            given_open: false,
            first_action: true,
            gate: Arc::new(SendGate::default()),
            book,
            shutdown,
            watching_signals: false,
        }
    }

    /// Closes the session's wallet as Ctrl+C or SIGTERM do (see
    /// [`ShutdownHandle::interrupt`]).
    #[must_use]
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    /// Runs until the user exits (or the input ends); the exit code of the program: 0, or 1
    /// if the wallet could not be saved.
    #[must_use]
    pub fn run(mut self) -> u8 {
        let header = self.paint.info(&args::header());
        self.say(&format!("{header}\n"));
        loop {
            let Some((mut opened, start)) = self.selection_screen() else {
                self.say("Thanks for stopping by...\n");
                return 0;
            };
            self.register_closer(&opened);
            if start == Start::Quit {
                return self.goodbye();
            }
            if self.config.handle_signals && !self.watching_signals {
                self.watch_signals();
                self.watching_signals = true;
            }
            let from = group(opened.wallet.status().wallet_block_count);
            if start == Start::Foreground {
                self.steps()
                    .step("Sync", || Ok(format!("from height {from}")));
                self.sync_wallet(&opened);
            } else {
                self.steps().step("Sync", || {
                    Ok(format!("in the background from height {from}"))
                });
            }
            let printer = self.console.printer();
            self.monitor = Some(Monitor::start(
                &opened.wallet,
                self.rt.clone(),
                Arc::clone(&self.lock),
                printer,
                self.paint,
            ));
            let leave = self.main_loop(&mut opened);
            if let Some(monitor) = self.monitor.take() {
                monitor.stop();
            }
            if leave == Leave::Quit {
                return self.goodbye();
            }
            // `back`: the same closing steps, then the start menu (a signal meanwhile quits)
            let (saved, quit) = self.shutdown.close_for_back();
            if quit {
                self.say("Thanks for stopping by...\n");
                return u8::from(!saved);
            }
        }
    }

    /// Closes the wallet (or waits for a signal's closing) and says goodbye.
    fn goodbye(&self) -> u8 {
        let saved = self.shutdown.close();
        self.say("Thanks for stopping by...\n");
        u8::from(!saved)
    }

    /// Writes `text` to the console.
    pub(crate) fn say(&self, text: &str) {
        self.console.write(text);
    }

    /// The path of a wallet, address book or CSV file name.
    pub(crate) fn path_of(&self, name: &str) -> PathBuf {
        self.config.dir.join(name)
    }

    /// Steps in the node's style on this console.
    pub(crate) fn steps(&self) -> Steps {
        Steps::new(
            self.console.out(),
            Steps::style_for(self.console.is_terminal(), self.paint),
            self.paint,
        )
    }

    /// Makes `opened` the wallet that closing saves.
    fn register_closer(&self, opened: &OpenedOf<F>) {
        let closer = Arc::new(Closer {
            wallet: Arc::clone(&opened.wallet),
            location: opened.location.clone(),
            rt: self.rt.clone(),
            gate: Arc::clone(&self.gate),
            book: Arc::clone(&self.book),
            steps: self.steps().closing(),
        });
        self.shutdown.register(Arc::new(move || closer.close()));
    }

    /// Ctrl+C, SIGTERM and SIGHUP close the wallet and exit (`zed_wallet.cpp` `shutdown`):
    /// the first one closes, the second shows a note, later ones are ignored; see
    /// [`ShutdownHandle::interrupt`].
    fn watch_signals(&self) {
        let handle = self.shutdown.clone();
        self.rt.spawn(async move {
            #[cfg(unix)]
            let (mut term, mut hangup) = {
                use tokio::signal::unix::{SignalKind, signal};
                (
                    signal(SignalKind::terminate()).ok(),
                    signal(SignalKind::hangup()).ok(),
                )
            };
            loop {
                #[cfg(unix)]
                let signalled = tokio::select! {
                    r = tokio::signal::ctrl_c() => r.is_ok(),
                    Some(()) = async {
                        match term.as_mut() {
                            Some(t) => t.recv().await,
                            None => std::future::pending().await,
                        }
                    } => true,
                    Some(()) = async {
                        match hangup.as_mut() {
                            Some(h) => h.recv().await,
                            None => std::future::pending().await,
                        }
                    } => true,
                };
                #[cfg(not(unix))]
                let signalled = tokio::signal::ctrl_c().await.is_ok();
                if !signalled {
                    return;
                }
                let handle = handle.clone();
                // closing blocks on the runtime, so it runs on a thread of its own
                std::thread::spawn(move || match handle.interrupt() {
                    SignalOutcome::Closed { saved } => {
                        handle.write("Thanks for stopping by...\n");
                        std::process::exit(i32::from(!saved));
                    }
                    SignalOutcome::NoWallet => std::process::exit(130),
                    SignalOutcome::PleaseWait | SignalOutcome::Ignored => {}
                });
            }
        });
    }

    /// The line above the balance and the commands: file, short address, sync.
    fn status_line(&self, opened: &OpenedOf<F>) -> String {
        let p = self.paint;
        let wallet = &opened.wallet;
        let status = wallet.status();
        let sync = if status.wallet_block_count >= status.network_block_count {
            format!("synced {}", group(status.wallet_block_count))
        } else {
            format!(
                "syncing {} / {}",
                group(status.wallet_block_count),
                group(status.network_block_count)
            )
        };
        let mut parts = vec![
            p.bold(&opened.location),
            short_address(&wallet.primary_address()),
            p.dim(&sync),
        ];
        if wallet.is_view_wallet() {
            parts.push(p.dim("view only"));
        }
        format!("\n  {}\n", parts.join(p.dim(" · ").as_str()))
    }

    /// Asks until a command of `available` is given: its C++ name and what followed it;
    /// `None` when the input ends.
    pub(crate) fn parse_command(
        &mut self,
        available: &[Command],
        prompt: &str,
        hint: &str,
    ) -> Option<(&'static str, String)> {
        let names = commands::names(available);
        loop {
            let line = self.console.read_command(prompt, &names)?;
            let p = self.paint;
            match commands::select(&line, available) {
                Selection::Empty => {}
                Selection::Command(key, rest) => return Some((key, rest)),
                Selection::BadNumber => self.say(&format!(
                    "  {} · {hint}\n",
                    p.warn(&format!("No entry {}", line.trim()))
                )),
                Selection::Unknown(name) => self.say(&format!(
                    "  {} · {hint}\n",
                    p.warn(&format!("Unknown command: {name}"))
                )),
            }
        }
    }

    /// `getAction`: `open` when a wallet file or password was given (the first time only),
    /// else the start menu.
    fn get_action(&mut self) -> Option<&'static str> {
        let given = self.first_action
            && (self.options.wallet_file.is_some() || self.options.password.is_some());
        self.first_action = false;
        if given {
            self.given_open = true;
            return Some("open");
        }
        self.say(&commands::menu(
            self.paint,
            "Welcome to your XKR wallet",
            commands::STARTUP,
        ));
        self.parse_command(
            commands::STARTUP,
            "  › ",
            "type a number or a name from the list",
        )
        .map(|(key, _)| key)
    }

    /// `handleLaunchCommand`.
    fn launch(&mut self, action: &str) -> Option<OpenedOf<F>> {
        match action {
            "create" => self.create_wallet(),
            "open" => self.open_wallet(),
            "seed_restore" => self.import_from_seed(),
            "key_restore" => self.import_from_keys(),
            "view_wallet" => self.import_view_wallet(),
            _ => None,
        }
    }

    /// `selectionScreen`: the open wallet and how to go on; `None` to exit.
    fn selection_screen(&mut self) -> Option<(OpenedOf<F>, Start)> {
        loop {
            let action = self.get_action()?;
            if action == "exit" {
                return None;
            }
            let Some(opened) = self.launch(action) else {
                self.say(&format!("  {}\n", self.paint.dim("back to the start menu")));
                continue;
            };
            self.node_step(&opened);
            if !self.check_node_status(&opened) {
                return Some((opened, Start::Quit));
            }
            let fee = opened.wallet.node().fee_amount;
            if fee != 0 {
                self.say(&format!(
                    "{}\n",
                    self.paint.warn(&format!(
                        "You have connected to a node that charges a fee to send \
                         transactions.\n\nThe fee for sending transactions is: {} per \
                         transaction.\n\nIf you don't want to pay the node fee, please \
                         relaunch xkrwallet and specify a different node or run your own.",
                        format::amount(fee)
                    ))
                ));
            }
            if action == "create" {
                return Some((opened, Start::Background));
            }
            return Some((opened, Start::Foreground));
        }
    }

    /// `checkNodeStatus`: false if the user chose to exit.
    fn check_node_status(&mut self, opened: &OpenedOf<F>) -> bool {
        loop {
            if opened.wallet.daemon_status().online {
                return true;
            }
            self.say(&format!(
                "{}\n",
                self.paint.warn(
                    "It looks like kryptokronad isn't open!\n\nEnsure kryptokronad is open and \
                     has finished syncing. (It will often not respond when syncing)\nIf it's \
                     still not working, try restarting kryptokronad (or try a different \
                     remote node).\nThe daemon sometimes gets stuck.\nAlternatively, perhaps \
                     kryptokronad can't communicate with any peers.\n\nThe wallet can't \
                     function fully until it can communicate with the network."
                )
            ));
            self.say(&commands::menu(self.paint, "", commands::NODE_DOWN));
            let Some((command, _)) = self.parse_command(
                commands::NODE_DOWN,
                "  › ",
                "type a number or a name from the list",
            ) else {
                return false;
            };
            match command {
                "exit" => return false,
                "continue" => return true,
                "swap_node" => self.swap_node(opened),
                // ask the same node again now
                _ => {
                    let node = opened.wallet.node();
                    if let Ok(again) = self.nodes.connect(&node.host, node.port) {
                        self.rt.block_on(opened.wallet.swap_node(again));
                    }
                }
            }
        }
    }

    /// The node after opening: its address and height, or that it does not answer.
    fn node_step(&self, opened: &OpenedOf<F>) {
        let daemon = opened.wallet.daemon_status();
        let node = opened.wallet.node();
        let host = if node.host.contains("://") {
            node.host.clone()
        } else {
            format!("{}:{}", node.host, node.port)
        };
        self.steps().step("Node", || {
            if daemon.online {
                Ok(format!(
                    "{host} · height {}",
                    group(daemon.local_block_count())
                ))
            } else {
                Err(format!("{host} · not answering"))
            }
        });
    }

    /// `mainLoop`: the status line and the commands, then a prompt with the balance, until
    /// `exit`, the end of the input or `back`.
    fn main_loop(&mut self, opened: &mut OpenedOf<F>) -> Leave {
        let available = commands::available(opened.wallet.is_view_wallet());
        self.say(&self.status_line(opened));
        self.say(&self.balance_lines(opened));
        self.say(&commands::groups(self.paint, &available));
        loop {
            let unlocked = opened.wallet.balance(None).map_or(0, |b| b.unlocked);
            let prompt = prompt_for(unlocked);
            let Some((command, rest)) =
                self.parse_command(&available, &prompt, "type help for the list")
            else {
                return Leave::Quit;
            };
            // a signal started closing the wallet: no more commands
            if self.shutdown.closing() {
                return Leave::Quit;
            }
            match command {
                "exit" => return Leave::Quit,
                "back" => return Leave::Back,
                _ => {}
            }
            let lock = Arc::clone(&self.lock);
            let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            self.handle_command(command, &rest, opened);
        }
    }

    /// `handleCommand`.
    fn handle_command(&mut self, command: &str, rest: &str, w: &mut OpenedOf<F>) {
        match command {
            "advanced" => self.help(w, ""),
            "address" => self.address(w),
            "balance" => self.balance(w),
            "backup" => self.backup(w),
            "help" => self.help(w, rest),
            "seed" => self.seed(w),
            "keys" => self.keys(w),
            "txs" => self.txs(w, rest),
            "transfer" => self.transfer(w, false),
            "ab_add" => self.ab_add(),
            "ab_delete" => self.ab_delete(),
            "ab_list" => self.ab_list(),
            "ab_send" => self.ab_send(w),
            "change_password" => self.change_password(w),
            "get_tx_private_key" => self.get_tx_private_key(w),
            "make_integrated_address" => self.make_integrated_address(),
            "incoming_transfers" => self.list_transfers(w, true, false),
            "list_transfers" => self.list_transfers(w, true, true),
            "optimize" => self.optimize_command(w),
            "outgoing_transfers" => self.list_transfers(w, false, true),
            "reset" => self.reset(w),
            "save" => self.save(w),
            "save_csv" => self.save_csv(w),
            "send_all" => self.transfer(w, true),
            "status" => self.status(w),
            "swap_node" => self.swap_node(w),
            "subwallets" => self.subwallets(w),
            "create_subwallet" => self.create_subwallet(w),
            "import_subwallet" => self.import_subwallet(w),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts() {
        assert_eq!(prompt_for(123_456_000), "  1 234.56 XKR › ");
        assert_eq!(prompt_for(0), "  0.00 XKR › ");
    }
}
