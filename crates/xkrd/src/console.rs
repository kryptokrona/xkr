//! Console commands on stdin while the node runs: C++ `DaemonCommandsHandler`
//! (`src/daemon/daemon_commands_handler.cpp`) on `common::ConsoleHandler`
//! (`src/common/console_handler.cpp`): `help`, `status`, `print_pl`, `set_log` and `exit` as
//! there, `start_mining` / `stop_mining` as there (plus `start_mining p2pool`), and `height`,
//! `fee-address` and `invite`. While the start-up question about the fee address
//! is open, an empty line keeps the address in effect. Started by `main` unless `--no-console` is given or stdin is not a
//! terminal (`DaemonConfiguration::noConsole`, `dch.start_handling()` in `src/daemon/daemon.cpp`).

use std::fmt::Write as _;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use xkr_chain::{Chain, unix_now};
use xkr_consensus::Network;
use xkr_consensus::params::DIFFICULTY_TARGET;
use xkr_hugin::{HuginStats, HuginStatsSnapshot};
use xkr_rpc::{HyperDhtState, HyperNat, HyperNetStatus, MiningStatus, NetStatus, P2poolStatus};
use xkr_storage::PruningSeed;
use xkr_types::Hash;

use crate::cli::{check_filter, cpp_level_filter};
use crate::fee::{self, DEFAULT_FEE_NAME, Fee, FeeAddress};
use crate::hyper::PrivateNode;
use crate::miner::{self, MineMode, MinerConfig, MinerControl};
use crate::p2pool::LinkState;
use crate::term::{self, Color, Style, Term};

/// The commands (the C++ `setHandler` names; `height` is xkrd's): name, short description for
/// `help`, one-line detail for `help <command>`.
const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "status",
        "node status at a glance",
        "status: heights and sync, peers, difficulty and hashrate, pool, uptime, database \
         size, pruning, mining, the Hugin relay and the built-in p2pool",
    ),
    (
        "height",
        "local and network height",
        "height: the local height against the network's, and the top block hash",
    ),
    (
        "print_pl",
        "peer lists",
        "print_pl: the white and gray peer lists (addresses) and the Hyperswarm connections",
    ),
    (
        "set_log",
        "change the log level",
        "set_log <0-4|filter>: 0 errors, 1 warnings, 2 info, 3 debug, 4 trace (as C++), or a \
         tracing filter such as xkrd=debug",
    ),
    (
        "fee-address",
        "show or set the fee address",
        "fee-address [<address>]: the address the node's shares are paid to (the Hugin \
         relay's p2pool shares, the built-in p2pool's payout); with an address: checked, \
         saved and used at once. Also hugin-address",
    ),
    (
        "start_mining",
        "mine solo to an address, or via p2pool",
        "start_mining <address>|p2pool [threads]: the built-in miner, solo to the address (blocks \
         pay it), or via the built-in p2pool (needs p2pool on, see xkrd --help; shares pay \
         the node's p2pool payout address, see fee-address). Threads: half the cores by default",
    ),
    (
        "stop_mining",
        "stop the built-in miner",
        "stop_mining: stops the built-in miner and waits for its threads",
    ),
    (
        "invite",
        "private node invite; invite new",
        "invite [new]: the private node's invite (xkrd --private), for --private-peer on \
         another node or a Hugin app; invite new writes a new secret and restarts the \
         Hyperswarm bridge, and old invites stop working",
    ),
    (
        "help",
        "this list; help <command> for more",
        "help [command]: the command list, or one command in detail",
    ),
    (
        "exit",
        "stop the node cleanly",
        "exit: stops the node cleanly (like Ctrl-C); the database is closed, so the next \
         start needs no check",
    ),
];

/// C++ `set_log` usage.
const SET_LOG_USE: &str = "use: set_log <log_level_number_0-4>";

/// `start_mining` usage (C++ `start_mining <addr> [threads=1]`, plus `p2pool`).
const START_MINING_USE: &str = "use: start_mining <address>|p2pool [threads]";

/// What `start_mining` mines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MineChoice {
    /// Solo to this address (C++ `start_mining <addr>`).
    Solo(String),
    /// Via the built-in p2pool, paid to the node's p2pool payout address.
    P2pool,
}

/// A parsed console line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    /// `help <command>`.
    HelpOn(String),
    Status,
    Height,
    PrintPl,
    /// `set_log` with the tracing filter to switch to.
    SetLog(String),
    /// `fee-address [<address>]` (also `fee_address`, `hugin-address`, `hugin_address`).
    FeeAddress(Option<String>),
    /// `start_mining <address>|p2pool [threads]`.
    StartMining {
        choice: MineChoice,
        threads: Option<usize>,
    },
    StopMining,
    /// `invite`: show the private node's invite.
    Invite,
    /// `invite new`: a new secret and invite.
    InviteNew,
    Exit,
}

/// Parses one line (C++ `ConsoleHandler::handleCommand`: words split on spaces, extra words
/// ignored). `Ok(None)` for an empty line; `Err` is the text to print.
pub fn parse(line: &str) -> Result<Option<Command>, String> {
    let mut words = line.split_whitespace();
    let Some(cmd) = words.next() else {
        return Ok(None);
    };
    let args: Vec<&str> = words.collect();
    let cmd = match cmd {
        "help" => match args.first() {
            Some(name) => Command::HelpOn((*name).to_owned()),
            None => Command::Help,
        },
        "status" => Command::Status,
        "height" => Command::Height,
        "print_pl" => Command::PrintPl,
        "exit" => Command::Exit,
        "set_log" => Command::SetLog(set_log_filter(&args)?),
        "fee-address" | "fee_address" | "hugin-address" | "hugin_address" => {
            Command::FeeAddress(args.first().map(|a| (*a).to_owned()))
        }
        "start_mining" => start_mining_args(&args)?,
        "stop_mining" => Command::StopMining,
        "invite" => match args.first() {
            Some(&"new") => Command::InviteNew,
            _ => Command::Invite,
        },
        other => return Err(format!("Unknown command: {other} (type help)")),
    };
    Ok(Some(cmd))
}

/// C++ `DaemonCommandsHandler::start_mining`: an address (or `p2pool`) and an optional thread
/// count of at least 1.
fn start_mining_args(args: &[&str]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Err(START_MINING_USE.to_owned());
    };
    let threads = match args.get(1) {
        None => None,
        Some(t) => match t.parse::<usize>() {
            Ok(n) if n > 0 => Some(n),
            _ => return Err(format!("wrong number of threads, {START_MINING_USE}")),
        },
    };
    let choice = if first.eq_ignore_ascii_case("p2pool") {
        MineChoice::P2pool
    } else {
        MineChoice::Solo((*first).to_owned())
    };
    Ok(Command::StartMining { choice, threads })
}

/// C++ `DaemonCommandsHandler::set_log`: one number 0-4 that becomes C++ level `n + 1`
/// (`set_log 0` errors ... `set_log 4` trace). A tracing filter works too.
fn set_log_filter(args: &[&str]) -> Result<String, String> {
    let [arg] = args else {
        return Err(SET_LOG_USE.to_owned());
    };
    if arg.bytes().all(|b| b.is_ascii_digit()) {
        match arg.parse::<u8>() {
            Ok(n) if n <= 4 => Ok(cpp_level_filter(n + 1).to_owned()),
            _ => Err(format!("wrong number range, {SET_LOG_USE}")),
        }
    } else {
        check_filter(arg)
            .map(|()| (*arg).to_owned())
            .map_err(|_| format!("wrong number format, {SET_LOG_USE}"))
    }
}

/// Applies a log filter text.
type SetFilter = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Switches the log filter at run time (`set_log`).
pub struct LogControl {
    set: SetFilter,
}

impl std::fmt::Debug for LogControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogControl").finish_non_exhaustive()
    }
}

impl LogControl {
    /// `set` installs a filter.
    #[must_use]
    pub fn new(set: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static) -> Self {
        Self { set: Box::new(set) }
    }

    /// Installs `filter`.
    pub fn set(&self, filter: &str) -> Result<(), String> {
        (self.set)(filter)
    }
}

/// Stops the node: `exit` and SIGINT/SIGTERM (the C++ signal handler calls
/// `DaemonCommandsHandler::exit` too, `src/daemon/daemon.cpp` `main`). The shutdown steps are
/// shown by [`crate::stopping`]; the log level stays as it is.
#[derive(Debug, Clone)]
pub struct ExitRequest {
    shutdown: CancellationToken,
}

impl ExitRequest {
    #[must_use]
    pub fn new(shutdown: CancellationToken) -> Self {
        Self { shutdown }
    }

    /// Whether the node is already stopping.
    #[must_use]
    pub fn requested(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// Shows the "Stopping the node" title and cancels the node's shutdown token.
    pub fn request(&self) {
        if self.requested() {
            return;
        }
        crate::stopping::begin();
        self.shutdown.cancel();
    }
}

/// What the console reads its answers from.
#[derive(Debug, Clone)]
pub struct NodeState {
    pub chain: Arc<Chain>,
    pub net: watch::Receiver<Arc<NetStatus>>,
    pub mining: watch::Receiver<Arc<MiningStatus>>,
    /// Counters of the Hugin relay, `None` with the relay off.
    pub hugin: Option<Arc<HuginStats>>,
    /// The fee address handle, `None` with the relay and p2pool off.
    pub fee: Option<Arc<FeeAddress>>,
    /// The built-in p2pool's status, `None` with p2pool off.
    pub p2pool: Option<watch::Receiver<Arc<P2poolStatus>>>,
    /// Starts and stops the built-in miner (`start_mining`, `stop_mining`).
    pub miner: Option<Arc<MinerControl>>,
    /// Whether the Hyperswarm transport runs (not `--no-hyper`).
    pub hyper: bool,
    /// The private node's secret (`--private`).
    pub private: Option<Arc<PrivateNode>>,
    pub version: String,
    /// Unix start time.
    pub start_time: u64,
    pub db_path: PathBuf,
}

/// The facts `status` and `height` print (what `/getinfo` and `/status` report).
#[derive(Debug, Clone)]
pub struct Status {
    pub version: String,
    pub network: Network,
    /// Block count (top index + 1), as C++ `status`.
    pub height: u64,
    pub network_height: u64,
    pub top_hash: Hash,
    pub outgoing: usize,
    pub incoming: usize,
    /// Hyperswarm connections, `None` with the transport off.
    pub hyper: Option<usize>,
    /// The Hyperswarm network's DHT node (`None`: off, or not reported yet).
    pub hyper_net: Option<HyperNetStatus>,
    pub white_peers: usize,
    pub gray_peers: usize,
    /// Difficulty of the next block.
    pub difficulty: u64,
    pub next_major_version: u8,
    pub pool_count: usize,
    pub downloading: bool,
    pub blocks_per_second: f64,
    pub uptime: u64,
    pub db_size: u64,
    pub pruning: PruningSeed,
    pub mining: MiningStatus,
    pub hugin: Option<HuginStatsSnapshot>,
    /// The built-in p2pool, `None` with p2pool off.
    pub p2pool: Option<P2poolStatus>,
}

impl NodeState {
    /// Reads the current status (C++ `on_get_info`; xkr-rpc `info_json` and `get_status`).
    pub fn status(&self) -> Result<Status, String> {
        let chain = &self.chain;
        let tip = chain.tip();
        let height = tip.height + 1;
        let net = self.net.borrow().clone();
        let reader = chain.reader().map_err(|e| e.to_string())?;
        let difficulty = chain.next_difficulty().map_err(|e| e.to_string())?;
        let fork = chain.fork_status().map_err(|e| e.to_string())?;
        Ok(Status {
            version: self.version.clone(),
            network: chain.network(),
            height,
            network_height: net.network_height.max(height),
            top_hash: tip.hash,
            outgoing: net.outgoing,
            incoming: net.incoming,
            hyper: self.hyper.then_some(net.hyper),
            hyper_net: net.hyper_net.filter(|_| self.hyper),
            white_peers: net.white_peers.len(),
            gray_peers: net.gray_peers.len(),
            difficulty,
            next_major_version: fork.next_block_major_version,
            pool_count: chain.pool().len(),
            downloading: net.downloading,
            blocks_per_second: net.blocks_per_second,
            uptime: unix_now().saturating_sub(self.start_time),
            db_size: std::fs::metadata(&self.db_path).map_or(0, |m| m.len()),
            pruning: reader.pruning_seed(),
            mining: self.mining.borrow().as_ref().clone(),
            hugin: self.hugin.as_deref().map(HuginStats::snapshot),
            p2pool: self.p2pool.as_ref().map(|p| p.borrow().as_ref().clone()),
        })
    }
}

/// A label padded to the value column.
fn label(style: Style, name: &str) -> String {
    style.paint(Color::Cyan, &format!("{name:<13}"))
}

/// `Height: 2 620 301/2 620 301 (100.00%)` (C++ `get_status_string`'s first part).
fn height_part(s: &Status) -> String {
    format!(
        "{}/{} ({}%)",
        term::group(s.height),
        term::group(s.network_height),
        term::sync_percentage(s.height, s.network_height)
    )
}

/// `status`: C++ `utilities::get_status_string` (`src/utilities/format_tools.cpp`) on one line
/// per topic, with the Hyperswarm (peers, then the DHT node: firewalled, UDP port), database,
/// pruning, mining and Hugin facts of `/status`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn format_status(s: &Status, style: Style) -> String {
    let network = match s.network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    };
    let state = if s.height >= s.network_height {
        style.paint(Color::Green, "synced")
    } else if s.downloading {
        style.paint(
            Color::Yellow,
            &format!("syncing ({:.0} blocks/s)", s.blocks_per_second),
        )
    } else {
        style.paint(Color::Yellow, "syncing")
    };
    let hyper = s.hyper.map_or_else(
        || "Hyperswarm off".to_owned(),
        |n| format!("{n} Hyperswarm"),
    );
    let mut out = String::new();
    let mut line = |name: &str, value: &str| {
        out.push_str(&label(style, name));
        out.push_str(value);
        out.push('\n');
    };
    line(
        "Height:",
        &format!("{} on {network}, {state}", height_part(s)),
    );
    line(
        "Peers:",
        &format!(
            "{} out + {} in (legacy), {hyper}; peer list {} white, {} gray",
            s.outgoing,
            s.incoming,
            term::group(s.white_peers as u64),
            term::group(s.gray_peers as u64)
        ),
    );
    if s.hyper.is_some() {
        line("Hyperswarm:", &hyper_net_text(s.hyper_net));
    }
    line(
        "Network:",
        &format!(
            "difficulty {}, hashrate {}, next block v{}",
            term::group(s.difficulty),
            term::hashrate(((s.difficulty + DIFFICULTY_TARGET / 2) / DIFFICULTY_TARGET) as f64),
            s.next_major_version
        ),
    );
    line(
        "Tx pool:",
        &match s.pool_count {
            1 => "1 transaction".to_owned(),
            n => format!("{} transactions", term::group(n as u64)),
        },
    );
    line(
        "Node:",
        &format!("xkrd {}, uptime {}", s.version, term::uptime(s.uptime)),
    );
    line(
        "Database:",
        &format!("{}, pruning: {}", term::bytes(s.db_size), s.pruning),
    );
    line("Mining:", &mining_text(&s.mining, style));
    match &s.hugin {
        None => line("Hugin relay:", "off"),
        Some(h) => {
            line("Hugin relay:", &hugin_text(h));
            line("Fee address:", &fee_text(h));
            line("p2pool:", &p2pool_text(h));
        }
    }
    match &s.p2pool {
        None => line("P2pool node:", "off"),
        Some(p) => {
            line("P2pool node:", &p2pool_node_text(p, style));
            if p.state == "running" {
                line("Pool peers:", &p2pool_peers_text(p));
                line("Pool miners:", &p2pool_miners_text(p));
            }
            line(
                "Payout:",
                &fee::Fee {
                    address: p.payout_address.clone(),
                    donation: p.payout_donation,
                }
                .describe(),
            );
        }
    }
    out
}

/// `default sidechain, height 29 277, difficulty 150 800 000` while running, else what it
/// waits for or why it stopped.
fn p2pool_node_text(p: &P2poolStatus, style: Style) -> String {
    match p.state.as_str() {
        "running" => format!(
            "{} sidechain, height {}, difficulty {}{}",
            p.sidechain,
            term::group(p.height),
            term::group(p.difficulty),
            if p.synced { "" } else { ", syncing" }
        ),
        "waiting" => format!(
            "{} sidechain, starts once the node is synchronized",
            p.sidechain
        ),
        "failed" => style.paint(
            Color::Red,
            &format!("failed: {}", p.error.as_deref().unwrap_or("unknown")),
        ),
        other => other.to_owned(),
    }
}

/// `3 out + 1 in (TCP), 2 Hyperswarm`.
fn p2pool_peers_text(p: &P2poolStatus) -> String {
    format!(
        "{} out + {} in (TCP), {} Hyperswarm",
        p.peers_out, p.peers_in, p.peers_hyper
    )
}

/// `7 wallets, 15.08 MH/s; stratum 1 miner, 1.23 KH/s, 2 shares found, 1 block`, or with the
/// stratum server off `7 wallets, 15.08 MH/s; stratum: off (127.0.0.1:3333 in use; set
/// --p2pool-stratum)`.
#[allow(clippy::cast_precision_loss)]
fn p2pool_miners_text(p: &P2poolStatus) -> String {
    let plural = |n: u64, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{} {many}", term::group(n))
        }
    };
    if let Some(reason) = &p.stratum_off {
        return format!(
            "{}, {}; stratum: off ({reason})",
            plural(p.miners, "wallet", "wallets"),
            term::hashrate(p.hashrate as f64),
        );
    }
    format!(
        "{}, {}; stratum {}, {}, {} found, {}",
        plural(p.miners, "wallet", "wallets"),
        term::hashrate(p.hashrate as f64),
        plural(p.stratum_miners, "miner", "miners"),
        term::hashrate(p.stratum_hashrate as f64),
        plural(p.shares_found, "share", "shares"),
        plural(p.blocks_found, "block", "blocks"),
    )
}

/// `firewalled yes, UDP port 49737` (`, strict NAT` when the NAT changes the public port per
/// destination, ` (--port)` for a fixed port, `, private node` with `--private`); `starting`
/// before the DHT is ready.
fn hyper_net_text(h: Option<HyperNetStatus>) -> String {
    let Some(h) = h else {
        return "starting".to_owned();
    };
    let mut text = hyper_dht_text(h);
    if h.private {
        text.push_str(", private node");
    }
    text
}

/// The DHT part of [`hyper_net_text`].
fn hyper_dht_text(h: HyperNetStatus) -> String {
    let port = h.udp_port.map_or_else(String::new, |p| {
        format!(
            "UDP port {p}{}",
            if h.port_fixed { " (--port)" } else { "" }
        )
    });
    match h.state {
        HyperDhtState::Failed => crate::hyper::off_reason().map_or_else(
            || "DHT not started (UDP port taken?)".to_owned(),
            |r| format!("off: {r}"),
        ),
        HyperDhtState::Starting if port.is_empty() => "starting".to_owned(),
        HyperDhtState::Starting => format!("starting, {port}"),
        HyperDhtState::Ready => {
            let mut t = format!(
                "firewalled {}",
                if h.firewalled == Some(true) {
                    "yes"
                } else {
                    "no"
                }
            );
            if h.nat == HyperNat::Random {
                t.push_str(", strict NAT");
            }
            if !port.is_empty() {
                let _ = write!(t, ", {port}");
            }
            t
        }
    }
}

/// `solo, 4 threads, 1.23 KH/s, block 2 620 302, 2 blocks found, 1 refused` or
/// `p2pool, 4 threads, 1.23 KH/s, block 2 620 302, 3 shares found, payout SEKR…`.
fn mining_text(m: &MiningStatus, style: Style) -> String {
    let count = |n: u64, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{} {many}", term::group(n))
        }
    };
    let mut text = if m.active {
        let p2pool = m.mode == "p2pool";
        let mut t = format!(
            "{}, {} threads, {}, block {}, ",
            if p2pool { "p2pool" } else { "solo" },
            m.threads,
            term::hashrate(m.hashrate),
            term::group(m.height),
        );
        if p2pool {
            let _ = write!(t, "{} found", count(m.shares_found, "share", "shares"));
            if !m.address.is_empty() {
                let _ = write!(t, ", payout {}", fee::short_address(&m.address));
            }
        } else {
            let _ = write!(t, "{} found", count(m.blocks_found, "block", "blocks"));
            if m.blocks_rejected > 0 {
                let _ = write!(t, ", {} refused", m.blocks_rejected);
            }
        }
        t
    } else if m.mode.is_empty() && m.address.is_empty() {
        "off".to_owned()
    } else {
        "stopped".to_owned()
    };
    if let Some(e) = &m.last_error {
        text.push_str(&style.paint(Color::Red, &format!(" (last error: {e})")));
    }
    text
}

/// `donation to nilsjr (SEKReSw2cP…kcBJZGc)`, the shortened address, or none.
fn fee_text(h: &HuginStatsSnapshot) -> String {
    fee::Fee {
        address: h.fee_address.clone(),
        donation: h.fee_donation,
    }
    .describe()
}

/// `127.0.0.1:3333 connected, shares 12 accepted, 5 submitted, 1 rejected`: shares that paid
/// for messages, sent to p2pool, refused by p2pool.
fn p2pool_text(h: &HuginStatsSnapshot) -> String {
    let state = match (&h.p2pool.host, h.p2pool.connected) {
        (Some(host), true) => format!("{host} connected"),
        (Some(host), false) => format!("{host} not connected"),
        (None, _) => "not connected".to_owned(),
    };
    format!(
        "{state}, shares {} accepted, {} submitted, {} rejected",
        h.shares_accepted, h.shares_submitted, h.shares_rejected_by_pool
    )
}

/// `12/200 apps (2 syncing, 1 queued), 4 relays (3 xkrd), 3 400 messages, pool ready`: apps
/// connected of the most allowed, history syncs being sent and waiting (`, 3 on allowance`
/// after them while apps have used up their sync allowance), and `, new apps paused` while new
/// app connections are paused.
fn hugin_text(h: &HuginStatsSnapshot) -> String {
    let apps = if h.apps_cap > 0 {
        format!("{}/{}", h.clients, h.apps_cap)
    } else {
        h.clients.to_string()
    };
    let on_allowance = if h.apps_on_allowance > 0 {
        format!(", {} on allowance", h.apps_on_allowance)
    } else {
        String::new()
    };
    format!(
        "{apps} apps ({} syncing, {} queued{on_allowance}), {} relays ({} xkrd), {} messages, {}{}",
        h.syncs_running,
        h.syncs_queued,
        h.nodes,
        h.xkrd_nodes,
        term::group(h.messages),
        if h.pool_ready {
            "pool ready"
        } else {
            "no pool job"
        },
        if h.admission_paused {
            ", new apps paused"
        } else {
            ""
        }
    )
}

/// `height`: `Height: 2 620 301/2 620 301 (100.00%), top block 2 620 300 <hash>`.
#[must_use]
pub fn format_height(s: &Status, style: Style) -> String {
    format!(
        "{}{}, top block {} {}\n",
        label(style, "Height:"),
        height_part(s),
        term::group(s.height - 1),
        style.paint(Color::Dim, &s.top_hash.to_string())
    )
}

/// `print_pl`: C++ `NodeServer::log_peerlist` (`src/p2p/net_node.cpp`), addresses only, and
/// the Hyperswarm connection count.
#[must_use]
pub fn format_peer_list(net: &NetStatus) -> String {
    let mut out = String::from("Peerlist white:\n");
    for p in &net.white_peers {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str("Peerlist gray:\n");
    for p in &net.gray_peers {
        out.push_str(p);
        out.push('\n');
    }
    let _ = writeln!(out, "Hyperswarm connections: {}", net.hyper);
    out
}

/// `help`: a short title and one `name  description` row per command (names bold,
/// descriptions dim), at most 60 columns. The C++ list (`DaemonCommandsHandler::
/// get_commands_str`) had a version line and usage strings instead.
fn help_text(version: &str, style: Style) -> String {
    let width = COMMANDS.iter().map(|(c, _, _)| c.len()).max().unwrap_or(0) + 2;
    let mut out = format!(
        "{} {}\n",
        style.paint(Color::Bold, "Console commands"),
        style.paint(Color::Dim, &format!("· xkrd {version}"))
    );
    for (cmd, short, _) in COMMANDS {
        let pad = " ".repeat(width - cmd.len());
        let _ = writeln!(
            out,
            "  {}{pad}{}",
            style.paint(Color::Bold, cmd),
            style.paint(Color::Dim, short)
        );
    }
    out
}

/// `help <command>`: the one-line detail.
fn help_on(name: &str) -> String {
    COMMANDS
        .iter()
        .find(|(cmd, _, _)| *cmd == name)
        .map_or_else(
            || format!("Unknown command: {name} (type help)\n"),
            |(_, _, detail)| format!("{detail}\n"),
        )
}

/// The answer to one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// `exit`: stop reading and stop the node after printing.
    pub exit: bool,
}

/// What the console answers from. The console starts with the header, long before the
/// database and the chain are open: until [`Shared::ready`], `status`, `height` and `print_pl`
/// answer with the start-up step under way ([`Shared::set_phase`]).
#[derive(Debug)]
pub struct Shared {
    node: OnceLock<NodeState>,
    phase: Mutex<String>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    /// Before start-up.
    #[must_use]
    pub fn new() -> Self {
        Self {
            node: OnceLock::new(),
            phase: Mutex::new("starting".to_owned()),
        }
    }

    /// A node that is up already.
    #[cfg(test)]
    #[must_use]
    pub fn with_node(node: NodeState) -> Self {
        let shared = Self::new();
        shared.ready(node);
        shared
    }

    /// The start-up step under way.
    pub fn set_phase(&self, step: &str) {
        step.clone_into(&mut self.phase.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// The node is up: answers come from `node` from now on.
    pub fn ready(&self, node: NodeState) {
        let _ = self.node.set(node);
    }

    /// The answer before [`Shared::ready`].
    fn starting(&self) -> String {
        let phase = self.phase.lock().unwrap_or_else(PoisonError::into_inner);
        format!("the node is starting: {phase}\n")
    }
}

/// `status`, `height` or `print_pl` on a running node.
fn node_reply(node: &NodeState, cmd: &Command, style: Style) -> String {
    match cmd {
        Command::PrintPl => format_peer_list(&node.net.borrow()),
        Command::Height => match node.status() {
            Ok(s) => format_height(&s, style),
            Err(e) => format!("Problem retrieving the node status: {e}\n"),
        },
        _ => match node.status() {
            Ok(s) => format_status(&s, style),
            Err(e) => format!("Problem retrieving the node status: {e}\n"),
        },
    }
}

/// `start_mining`: starts the built-in miner solo to an address, or via the built-in p2pool.
fn start_mining(
    node: &NodeState,
    choice: &MineChoice,
    threads: Option<usize>,
    style: Style,
) -> String {
    let Some(control) = &node.miner else {
        return "The built-in miner is not available\n".to_owned();
    };
    let threads = threads.unwrap_or_else(miner::default_threads);
    match choice {
        MineChoice::Solo(address) => {
            let Some(keys) = xkr_rpc::parse_miner_address(address) else {
                return format!("Not a valid SEKR/Xkr address: {address}\n");
            };
            let cfg = MinerConfig {
                mode: MineMode::Solo {
                    address: address.clone(),
                    miner: keys,
                },
                threads,
            };
            match control.start(cfg) {
                Ok(()) => format!(
                    "{} to {} · {threads} threads\n",
                    style.paint(Color::Green, "Mining solo"),
                    fee::short_address(address)
                ),
                Err(e) => format!("Mining not started: {e}\n"),
            }
        }
        MineChoice::P2pool => {
            let Some(link) = control.pool() else {
                return "p2pool is off: start xkrd with --node --hugin --p2pool (mainnet: no mode flags) to mine via p2pool\n".to_owned();
            };
            let waiting = match link.state() {
                LinkState::Waiting => true,
                LinkState::Running(_) => false,
                LinkState::Down(reason) => return format!("Mining not started: {reason}\n"),
            };
            let fee = match &node.fee {
                Some(f) => f.get(),
                None => Fee {
                    address: None,
                    donation: false,
                },
            };
            if fee.address.is_none() {
                let mut text = String::new();
                for line in miner::p2pool_payout_lines(&fee) {
                    let _ = writeln!(text, "{line}");
                }
                text.push_str("Mining not started\n");
                return text;
            }
            let cfg = MinerConfig {
                mode: MineMode::P2pool(link.clone()),
                threads,
            };
            match control.start(cfg) {
                Ok(()) => p2pool_start_text(threads, &fee, waiting, style),
                Err(e) => format!("Mining not started: {e}\n"),
            }
        }
    }
}

/// The answer to `start_mining p2pool` (at most 60 columns a line):
/// `Mining via p2pool · 4 threads · payout SEKReSw2cP…kcBJZGc`; with the donation default the
/// payout is `nilsjr` and two lines say so and how to change it; while p2pool waits for the
/// sync, one more line says the miner waits too.
fn p2pool_start_text(threads: usize, fee: &Fee, waiting: bool, style: Style) -> String {
    let payout = match &fee.address {
        Some(_) if fee.donation => DEFAULT_FEE_NAME.to_owned(),
        Some(a) => fee::short_address(a),
        None => "none".to_owned(),
    };
    let mut text = format!(
        "{} · {threads} threads · payout {payout}\n",
        style.paint(Color::Green, "Mining via p2pool")
    );
    if fee.donation {
        for line in miner::p2pool_payout_lines(fee) {
            let _ = writeln!(text, "{}", style.paint(Color::Yellow, &line));
        }
    }
    if waiting {
        text.push_str("p2pool starts once the node is synchronized;\n");
        text.push_str("mining waits for it.\n");
    }
    text
}

/// `stop_mining`.
fn stop_mining(node: &NodeState) -> String {
    match &node.miner {
        Some(control) if control.stop() => "Mining stopped\n".to_owned(),
        _ => "The miner is not running\n".to_owned(),
    }
}

/// `invite` and `invite new` (`renew`): the private node's invite on a line of its own, so it
/// can be copied whole.
fn invite(node: &NodeState, renew: bool, style: Style) -> String {
    let Some(private) = &node.private else {
        return "Not a private node (start xkrd with --private)\n".to_owned();
    };
    let app = |out: &mut String| {
        if let Some(app) = private.hugin_app_invite() {
            let _ = writeln!(
                out,
                "{}\n{app}",
                style.paint(Color::Dim, "Hugin app invite (relay only):")
            );
        }
    };
    let mut out = String::new();
    if !renew {
        let _ = writeln!(
            out,
            "{}\n{}",
            style.paint(Color::Dim, "Private node invite (for --private-peer):"),
            private.invite()
        );
        app(&mut out);
        return out;
    }
    match private.renew() {
        Ok(invite) => {
            let _ = writeln!(
                out,
                "{}\n{invite}",
                style.paint(Color::Green, "New private node invite:")
            );
            app(&mut out);
            let _ = writeln!(
                out,
                "{}",
                style.paint(Color::Dim, "Old invites stop working; Hyperswarm restarts.")
            );
            out
        }
        Err(e) => format!("Invite not changed: {e}\n"),
    }
}

/// The console (C++ `DaemonCommandsHandler`).
#[derive(Debug)]
pub struct Console {
    shared: Arc<Shared>,
    log: Option<Arc<LogControl>>,
    exit: ExitRequest,
    style: Style,
    version: String,
}

impl Console {
    #[must_use]
    pub fn new(
        shared: Arc<Shared>,
        log: Option<Arc<LogControl>>,
        exit: ExitRequest,
        style: Style,
        version: &str,
    ) -> Self {
        Self {
            shared,
            log,
            exit,
            style,
            version: version.to_owned(),
        }
    }

    /// The fee address handle of the running node.
    fn fee(&self) -> Option<&Arc<FeeAddress>> {
        self.shared.node.get().and_then(|n| n.fee.as_ref())
    }

    /// The reply to `line`, `None` for an empty one. An empty line answers the open fee address
    /// question (keep); any other command drops the question.
    #[must_use]
    pub fn execute(&self, line: &str) -> Option<Reply> {
        let parsed = parse(line);
        if let Some(fee) = self.fee().filter(|f| f.question_pending()) {
            match &parsed {
                Ok(None) => {
                    let text = match fee.keep() {
                        Ok(()) => format!(
                            "Keeping the donation to {DEFAULT_FEE_NAME}. Change it any time with \
                             fee-address <address>.\n"
                        ),
                        Err(e) => format!("Cannot save the answer: {e}\n"),
                    };
                    return Some(Reply { text, exit: false });
                }
                Ok(Some(Command::FeeAddress(Some(_)))) => {}
                _ => fee.drop_question(),
            }
        }
        let text = match parsed {
            Ok(None) => return None,
            Err(e) => format!("{e}\n"),
            Ok(Some(cmd)) => match cmd {
                Command::Help => help_text(&self.version, self.style),
                Command::HelpOn(name) => help_on(&name),
                Command::Status | Command::Height | Command::PrintPl => {
                    match self.shared.node.get() {
                        Some(node) => node_reply(node, &cmd, self.style),
                        None => self.shared.starting(),
                    }
                }
                Command::SetLog(filter) => match &self.log {
                    Some(log) => match log.set(&filter) {
                        Ok(()) => format!("Log level set to {filter}\n"),
                        Err(e) => format!("Cannot change the log level: {e}\n"),
                    },
                    None => "The log level cannot be changed in this mode\n".to_owned(),
                },
                Command::FeeAddress(address) => self.fee_address(address.as_deref()),
                Command::StartMining { choice, threads } => match self.shared.node.get() {
                    Some(node) => start_mining(node, &choice, threads, self.style),
                    None => self.shared.starting(),
                },
                Command::StopMining => match self.shared.node.get() {
                    Some(node) => stop_mining(node),
                    None => self.shared.starting(),
                },
                Command::Invite | Command::InviteNew => match self.shared.node.get() {
                    Some(node) => invite(node, cmd == Command::InviteNew, self.style),
                    None => self.shared.starting(),
                },
                Command::Exit => {
                    return Some(Reply {
                        text: String::new(),
                        exit: true,
                    });
                }
            },
        };
        Some(Reply { text, exit: false })
    }

    /// `fee-address [<address>]`.
    fn fee_address(&self, address: Option<&str>) -> String {
        let Some(fee) = self.fee() else {
            return match self.shared.node.get() {
                None => self.shared.starting(),
                Some(_) => "The Hugin relay and p2pool are off: no fee address in use\n".to_owned(),
            };
        };
        match address {
            None => {
                let f = fee.get();
                match &f.address {
                    None => "Fee address: none (shares are not submitted)\n".to_owned(),
                    Some(a) if f.donation => {
                        format!("Fee address: {a} (donation to {DEFAULT_FEE_NAME})\n")
                    }
                    Some(a) => format!("Fee address: {a}\n"),
                }
            }
            Some(text) => match fee.set(text) {
                Ok(f) => format!("Fee address set to {} (saved)\n", f.short()),
                Err(e) => format!("Fee address not changed: {e}\n"),
            },
        }
    }

    /// Reads commands from `input` until `exit` (then stops the node and returns `true`) or the
    /// end of the input (`false`; C++ `AsyncConsoleReader::consoleThread` ends there too).
    #[cfg(test)]
    pub fn run(&self, input: &mut impl BufRead, out: &mut impl Write) -> bool {
        self.run_with(input, out, &|_| {})
    }

    /// [`Console::run`], calling `entered` with every line read before answering it (the
    /// terminal panel moves under the echoed line, [`Term::entered`]).
    pub fn run_with(
        &self,
        input: &mut impl BufRead,
        out: &mut impl Write,
        entered: &dyn Fn(&str),
    ) -> bool {
        let mut line = String::new();
        loop {
            line.clear();
            match input.read_line(&mut line) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    let _ = out.write_all(b"Unknown command (not UTF-8)\n");
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
            entered(&line);
            if self.exit.requested() {
                return false;
            }
            let Some(reply) = self.execute(&line) else {
                continue;
            };
            let _ = out.write_all(reply.text.as_bytes());
            let _ = out.flush();
            if reply.exit {
                self.exit.request();
                return true;
            }
        }
    }
}

/// Starts the console thread on stdin. It is never joined: a blocked read ends with the
/// process.
pub fn spawn(console: Console, term: Term) -> io::Result<()> {
    std::thread::Builder::new()
        .name("xkr-console".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut out = term.writer();
            let entered = |line: &str| term.entered(line);
            if !console.run_with(&mut stdin.lock(), &mut out, &entered) && !console.exit.requested()
            {
                let _ = out.write_all(
                    b"Console closed (end of input); Ctrl-C or SIGTERM stops the node.\n",
                );
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   \n"), Ok(None));
        assert_eq!(parse("help"), Ok(Some(Command::Help)));
        assert_eq!(parse("  status  \n"), Ok(Some(Command::Status)));
        assert_eq!(parse("height"), Ok(Some(Command::Height)));
        assert_eq!(parse("print_pl extra"), Ok(Some(Command::PrintPl)));
        assert_eq!(parse("exit"), Ok(Some(Command::Exit)));
        assert_eq!(
            parse("set_log 0"),
            Ok(Some(Command::SetLog("error".into())))
        );
        assert_eq!(parse("set_log 1"), Ok(Some(Command::SetLog("warn".into()))));
        assert_eq!(parse("set_log 2"), Ok(Some(Command::SetLog("info".into()))));
        assert_eq!(
            parse("set_log 4"),
            Ok(Some(Command::SetLog("trace".into())))
        );
        assert_eq!(
            parse("set_log xkrd=debug,warn"),
            Ok(Some(Command::SetLog("xkrd=debug,warn".into())))
        );
        assert_eq!(parse("set_log"), Err(SET_LOG_USE.into()));
        assert_eq!(parse("set_log 1 2"), Err(SET_LOG_USE.into()));
        assert!(
            parse("set_log 5")
                .unwrap_err()
                .starts_with("wrong number range")
        );
        assert!(
            parse("set_log 999")
                .unwrap_err()
                .starts_with("wrong number range")
        );
        assert!(
            parse("set_log xkrd=notalevel")
                .unwrap_err()
                .starts_with("wrong number format")
        );
        assert_eq!(
            parse("Status"),
            Err("Unknown command: Status (type help)".into())
        );
        // start_mining <address>|p2pool [threads] (C++ `start_mining <addr> [threads=1]`)
        assert_eq!(
            parse("start_mining SEKRabc"),
            Ok(Some(Command::StartMining {
                choice: MineChoice::Solo("SEKRabc".into()),
                threads: None
            }))
        );
        assert_eq!(
            parse("start_mining SEKRabc 4"),
            Ok(Some(Command::StartMining {
                choice: MineChoice::Solo("SEKRabc".into()),
                threads: Some(4)
            }))
        );
        assert_eq!(
            parse("start_mining p2pool"),
            Ok(Some(Command::StartMining {
                choice: MineChoice::P2pool,
                threads: None
            }))
        );
        assert_eq!(
            parse("  start_mining P2Pool 2 extra\n"),
            Ok(Some(Command::StartMining {
                choice: MineChoice::P2pool,
                threads: Some(2)
            }))
        );
        assert_eq!(parse("start_mining"), Err(START_MINING_USE.into()));
        for bad in [
            "start_mining p2pool 0",
            "start_mining p2pool x",
            "start_mining SEKR -1",
        ] {
            assert!(
                parse(bad)
                    .unwrap_err()
                    .starts_with("wrong number of threads"),
                "{bad}"
            );
        }
        assert_eq!(parse("stop_mining"), Ok(Some(Command::StopMining)));
        assert_eq!(parse("invite"), Ok(Some(Command::Invite)));
        assert_eq!(parse(" invite  new \n"), Ok(Some(Command::InviteNew)));
        assert_eq!(parse("invite other"), Ok(Some(Command::Invite)));
        for name in [
            "fee-address",
            "fee_address",
            "hugin-address",
            "hugin_address",
        ] {
            assert_eq!(parse(name), Ok(Some(Command::FeeAddress(None))), "{name}");
            assert_eq!(
                parse(&format!("  {name}  SEKRabc extra\n")),
                Ok(Some(Command::FeeAddress(Some("SEKRabc".into())))),
                "{name}"
            );
        }
    }

    #[test]
    fn help_lists_every_command() {
        let h = help_text("1.2.3", Style::new(false));
        assert_eq!(
            h,
            "Console commands · xkrd 1.2.3\n\
             \x20 status        node status at a glance\n\
             \x20 height        local and network height\n\
             \x20 print_pl      peer lists\n\
             \x20 set_log       change the log level\n\
             \x20 fee-address   show or set the fee address\n\
             \x20 start_mining  mine solo to an address, or via p2pool\n\
             \x20 stop_mining   stop the built-in miner\n\
             \x20 invite        private node invite; invite new\n\
             \x20 help          this list; help <command> for more\n\
             \x20 exit          stop the node cleanly\n"
        );
        assert!(h.lines().all(|l| l.chars().count() <= 60));
        let coloured = help_text("1.2.3", Style::new(true));
        assert!(coloured.contains("\x1b[1mstatus\x1b[0m        \x1b[2mnode status at a glance"));
        // help <command>
        assert_eq!(
            parse("help status"),
            Ok(Some(Command::HelpOn("status".into())))
        );
        assert!(help_on("set_log").starts_with("set_log <0-4|filter>: 0 errors"));
        assert!(help_on("exit").contains("next start needs no check"));
        assert_eq!(help_on("bogus"), "Unknown command: bogus (type help)\n");
        assert!(help_on("fee-address").contains("hugin-address"));
        for (c, _, detail) in COMMANDS {
            assert!(detail.starts_with(c), "{c}");
            assert!(parse(&format!("{c} 2")).unwrap().is_some(), "{c}");
        }
    }

    /// `start_mining p2pool`: the payout in the start line; the donation default said with how
    /// to change it; a note while p2pool waits for the sync. At most 60 columns.
    #[test]
    fn p2pool_start_lines() {
        let plain = Style::new(false);
        let donation = Fee {
            address: Some(crate::fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        };
        let text = p2pool_start_text(4, &donation, false, plain);
        assert_eq!(
            text,
            "Mining via p2pool · 4 threads · payout nilsjr\n\
             p2pool pays the node's p2pool address, now a donation\n\
             to nilsjr. Set yours with: fee-address <address>\n"
        );
        let own = Fee {
            donation: false,
            ..donation
        };
        let text = p2pool_start_text(16, &own, true, plain);
        assert_eq!(
            text,
            "Mining via p2pool · 16 threads · payout SEKReSw2cP…kcBJZGc\n\
             p2pool starts once the node is synchronized;\n\
             mining waits for it.\n"
        );
        for line in text.lines() {
            assert!(line.chars().count() <= 60, "{line}");
        }
    }

    fn sample_status() -> Status {
        Status {
            version: "0.1.0".into(),
            network: Network::Mainnet,
            height: 2_620_301,
            network_height: 2_620_301,
            top_hash: Hash([0xab; 32]),
            outgoing: 8,
            incoming: 2,
            hyper: Some(5),
            hyper_net: Some(HyperNetStatus {
                state: HyperDhtState::Ready,
                udp_port: Some(49_737),
                port_fixed: false,
                firewalled: Some(true),
                nat: HyperNat::Consistent,
                private: false,
            }),
            white_peers: 250,
            gray_peers: 1_000,
            difficulty: 3_703_125,
            next_major_version: 6,
            pool_count: 3,
            downloading: false,
            blocks_per_second: 0.0,
            uptime: 2 * 3_600 + 13 * 60 + 5,
            db_size: 10_210_000_000,
            pruning: PruningSeed::new(4, 2).unwrap(),
            mining: MiningStatus::default(),
            hugin: None,
            p2pool: None,
        }
    }

    #[test]
    fn status_is_formatted() {
        let plain = Style::new(false);
        let s = sample_status();
        let text = format_status(&s, plain);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "Height:      2 620 301/2 620 301 (100.00%) on mainnet, synced",
                "Peers:       8 out + 2 in (legacy), 5 Hyperswarm; peer list 250 white, 1 000 gray",
                "Hyperswarm:  firewalled yes, UDP port 49737",
                "Network:     difficulty 3 703 125, hashrate 41.15 KH/s, next block v6",
                "Tx pool:     3 transactions",
                "Node:        xkrd 0.1.0, uptime 0d 2h 13m 5s",
                "Database:    10.21 GB, pruning: stripe 2/4 (0x0402)",
                "Mining:      off",
                "Hugin relay: off",
                "P2pool node: off",
            ]
        );
        assert!(!text.contains('\x1b'));

        // the built-in p2pool: waiting, running, failed
        let mut s = sample_status();
        s.p2pool = Some(P2poolStatus {
            state: "waiting".into(),
            sidechain: "default".into(),
            payout_address: Some(crate::fee::DEFAULT_FEE_ADDRESS.into()),
            payout_donation: true,
            ..P2poolStatus::default()
        });
        let text = format_status(&s, plain);
        assert!(text.ends_with(
            "P2pool node: default sidechain, starts once the node is synchronized\n\
             Payout:      donation to nilsjr (SEKReSw2cP…kcBJZGc)\n"
        ));
        s.p2pool = Some(P2poolStatus {
            state: "running".into(),
            sidechain: "mini".into(),
            height: 29_277,
            difficulty: 150_800_000,
            hashrate: 15_080_000,
            miners: 7,
            synced: true,
            peers_out: 3,
            peers_in: 1,
            peers_hyper: 2,
            stratum_miners: 1,
            stratum_hashrate: 1_230,
            shares_found: 2,
            blocks_found: 1,
            payout_address: Some(crate::fee::DEFAULT_FEE_ADDRESS.into()),
            ..P2poolStatus::default()
        });
        let text = format_status(&s, plain);
        assert!(
            text.ends_with(
                "P2pool node: mini sidechain, height 29 277, difficulty 150 800 000\n\
                 Pool peers:  3 out + 1 in (TCP), 2 Hyperswarm\n\
                 Pool miners: 7 wallets, 15.08 MH/s; stratum 1 miner, 1.23 KH/s, 2 shares found, \
                 1 block\n\
                 Payout:      SEKReSw2cP…kcBJZGc\n"
            ),
            "{text}"
        );
        // running without the stratum server (its address in use)
        s.p2pool = Some(P2poolStatus {
            state: "running".into(),
            sidechain: "mini".into(),
            height: 29_277,
            difficulty: 150_800_000,
            hashrate: 15_080_000,
            miners: 7,
            synced: true,
            stratum_off: Some("127.0.0.1:3333 in use; set --p2pool-stratum".into()),
            ..P2poolStatus::default()
        });
        let text = format_status(&s, plain);
        assert!(
            text.contains(
                "Pool miners: 7 wallets, 15.08 MH/s; stratum: off (127.0.0.1:3333 in use; set \
                 --p2pool-stratum)\n"
            ),
            "{text}"
        );
        s.p2pool = Some(P2poolStatus {
            state: "failed".into(),
            sidechain: "default".into(),
            error: Some("stratum 127.0.0.1:3333: address in use".into()),
            ..P2poolStatus::default()
        });
        let text = format_status(&s, plain);
        assert!(text.contains(
            "P2pool node: failed: stratum 127.0.0.1:3333: address in use\n\
             Payout:      none"
        ));

        // syncing, Hyperswarm off, miner and relay on
        let mut s = sample_status();
        s.height = 1_000;
        s.downloading = true;
        s.blocks_per_second = 812.4;
        s.hyper = None;
        s.hyper_net = None;
        s.mining = MiningStatus {
            active: true,
            mode: "solo".into(),
            address: "SEKR".into(),
            threads: 4,
            hashrate: 1_234.0,
            height: 2_620_302,
            blocks_found: 2,
            blocks_rejected: 1,
            ..MiningStatus::default()
        };
        s.hugin = Some(HuginStatsSnapshot {
            clients: 12,
            apps_cap: 200,
            syncs_running: 2,
            syncs_queued: 1,
            nodes: 4,
            xkrd_nodes: 3,
            messages: 3_400,
            pool_ready: true,
            shares_accepted: 12,
            shares_submitted: 5,
            shares_rejected_by_pool: 1,
            fee_address: Some(crate::fee::DEFAULT_FEE_ADDRESS.into()),
            fee_donation: true,
            p2pool: xkr_hugin::P2poolInfo {
                host: Some("127.0.0.1:3333".into()),
                connected: true,
            },
            ..HuginStatsSnapshot::default()
        });
        let text = format_status(&s, plain);
        assert!(text.contains("1 000/2 620 301 (0.04%) on mainnet, syncing (812 blocks/s)"));
        assert!(text.contains("8 out + 2 in (legacy), Hyperswarm off;"));
        assert!(
            !text.contains("Hyperswarm: "),
            "no DHT line with the transport off"
        );
        assert!(text.contains(
            "Mining:      solo, 4 threads, 1.23 KH/s, block 2 620 302, 2 blocks found, 1 refused"
        ));
        // via p2pool: shares and the payout address of the shares
        let pool = MiningStatus {
            active: true,
            mode: "p2pool".into(),
            address: crate::fee::DEFAULT_FEE_ADDRESS.into(),
            threads: 2,
            hashrate: 1_234.0,
            height: 2_620_302,
            shares_found: 3,
            ..MiningStatus::default()
        };
        assert_eq!(
            mining_text(&pool, plain),
            "p2pool, 2 threads, 1.23 KH/s, block 2 620 302, 3 shares found, payout \
             SEKReSw2cP…kcBJZGc"
        );
        let stopped = MiningStatus {
            active: false,
            ..pool
        };
        assert_eq!(mining_text(&stopped, plain), "stopped");
        assert!(text.contains(
            "Hugin relay: 12/200 apps (2 syncing, 1 queued), 4 relays (3 xkrd), 3 400 messages, \
             pool ready\n"
        ));
        let paused = HuginStatsSnapshot {
            admission_paused: true,
            ..s.hugin.clone().unwrap()
        };
        assert!(hugin_text(&paused).ends_with("pool ready, new apps paused"));
        let on_allowance = HuginStatsSnapshot {
            apps_on_allowance: 3,
            ..s.hugin.clone().unwrap()
        };
        assert!(
            hugin_text(&on_allowance)
                .starts_with("12/200 apps (2 syncing, 1 queued, 3 on allowance), 4 relays")
        );
        assert!(text.contains("Fee address: donation to nilsjr (SEKReSw2cP…kcBJZGc)\n"));
        assert!(text.contains(
            "p2pool:      127.0.0.1:3333 connected, shares 12 accepted, 5 submitted, 1 rejected\n"
        ));

        let coloured = format_status(&s, Style::new(true));
        assert!(coloured.contains("\x1b[1;36mHeight:"));
        assert!(coloured.contains("\x1b[1;33msyncing"));

        assert_eq!(
            format_height(&sample_status(), plain),
            format!(
                "Height:      2 620 301/2 620 301 (100.00%), top block 2 620 300 {}\n",
                "ab".repeat(32)
            )
        );
    }

    /// The `Hyperswarm:` line of `status`: the DHT node's state, firewalled or not, the UDP port.
    #[test]
    fn hyper_net_line() {
        let ready = HyperNetStatus {
            state: HyperDhtState::Ready,
            udp_port: Some(21_890),
            port_fixed: true,
            firewalled: Some(false),
            nat: HyperNat::Consistent,
            private: false,
        };
        assert_eq!(
            hyper_net_text(Some(ready)),
            "firewalled no, UDP port 21890 (--port)"
        );
        let strict = HyperNetStatus {
            port_fixed: false,
            firewalled: Some(true),
            nat: HyperNat::Random,
            udp_port: Some(49_737),
            ..ready
        };
        assert_eq!(
            hyper_net_text(Some(strict)),
            "firewalled yes, strict NAT, UDP port 49737"
        );
        assert_eq!(hyper_net_text(None), "starting");
        assert_eq!(hyper_net_text(Some(HyperNetStatus::default())), "starting");
        assert_eq!(
            hyper_net_text(Some(HyperNetStatus {
                udp_port: Some(49_737),
                ..HyperNetStatus::default()
            })),
            "starting, UDP port 49737"
        );
        assert_eq!(
            hyper_net_text(Some(HyperNetStatus {
                state: HyperDhtState::Failed,
                port_fixed: true,
                ..HyperNetStatus::default()
            })),
            "DHT not started (UDP port taken?)"
        );
        assert_eq!(
            hyper_net_text(Some(HyperNetStatus {
                private: true,
                ..ready
            })),
            "firewalled no, UDP port 21890 (--port), private node"
        );
        assert_eq!(
            hyper_net_text(Some(HyperNetStatus {
                private: true,
                ..HyperNetStatus::default()
            })),
            "starting, private node"
        );
        let mut s = sample_status();
        s.hyper_net = None;
        assert!(format_status(&s, Style::new(false)).contains("\nHyperswarm:  starting\n"));
        for h in [ready, strict] {
            let line = format!("Hyperswarm:  {}", hyper_net_text(Some(h)));
            assert!(line.chars().count() <= 60, "{line}");
        }
    }

    #[test]
    fn peer_list_is_formatted() {
        let net = NetStatus {
            white_peers: vec!["1.2.3.4:11897".into()],
            gray_peers: vec!["5.6.7.8:11897".into(), "9.9.9.9:11897".into()],
            hyper: 2,
            ..NetStatus::default()
        };
        assert_eq!(
            format_peer_list(&net),
            "Peerlist white:\n1.2.3.4:11897\nPeerlist gray:\n5.6.7.8:11897\n9.9.9.9:11897\n\
             Hyperswarm connections: 2\n"
        );
    }

    /// A fresh testnet chain in a temporary directory, removed on drop.
    struct TestChain {
        dir: PathBuf,
        chain: Option<Arc<Chain>>,
    }

    impl TestChain {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkrd-console-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store =
                xkr_storage::Store::open_with_cache(dir.join("chain.redb"), 16 << 20).unwrap();
            let pool = Arc::new(xkr_txpool::TxPool::new(xkr_txpool::PoolConfig::default()));
            let chain =
                Chain::open(store, xkr_chain::ChainConfig::new(Network::Testnet), pool).unwrap();
            Self {
                dir,
                chain: Some(Arc::new(chain)),
            }
        }
    }

    impl Drop for TestChain {
        fn drop(&mut self) {
            drop(self.chain.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The console on a scripted input against a fresh testnet node: every command answers,
    /// `set_log` reaches the log control, `exit` stops the node and ends the input there.
    #[test]
    fn scripted_session() {
        let tc = TestChain::new("session");
        let chain = tc.chain.clone().unwrap();
        let net = NetStatus {
            outgoing: 1,
            incoming: 0,
            hyper: 0,
            white_peers: vec!["10.0.0.1:11898".into()],
            network_height: 5,
            downloading: true,
            ..NetStatus::default()
        };
        let (_net_tx, net_rx) = watch::channel(Arc::new(net));
        let (_mining_tx, mining_rx) = watch::channel(Arc::new(MiningStatus::default()));
        let state = NodeState {
            chain,
            net: net_rx,
            mining: mining_rx,
            hugin: Some(Arc::new(HuginStats::default())),
            fee: None,
            p2pool: None,
            miner: None,
            hyper: true,
            private: None,
            version: "9.9.9".into(),
            start_time: unix_now(),
            db_path: tc.dir.join("chain.redb"),
        };
        let filters = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = {
            let filters = filters.clone();
            Arc::new(LogControl::new(move |f: &str| {
                filters.lock().unwrap().push(f.to_owned());
                Ok(())
            }))
        };
        let shutdown = CancellationToken::new();
        let console = Console::new(
            Arc::new(Shared::with_node(state)),
            Some(log),
            ExitRequest::new(shutdown.clone()),
            Style::new(false),
            "9.9.9",
        );

        let input = "\nhelp\nstatus\nheight\nprint_pl\nbogus\nset_log 3\nset_log 9\nexit\nstatus\n";
        let mut out = Vec::new();
        assert!(console.run(&mut io::Cursor::new(input), &mut out));
        let out = String::from_utf8(out).unwrap();

        assert!(out.starts_with("Console commands · xkrd 9.9.9\n"));
        assert!(out.contains("Height:      1/5 (20.00%) on testnet, syncing (0 blocks/s)\n"));
        assert!(out.contains("Peers:       1 out + 0 in (legacy), 0 Hyperswarm;"));
        assert!(out.contains("Node:        xkrd 9.9.9, uptime 0d 0h 0m"));
        assert!(out.contains("Database:    "));
        assert!(out.contains("pruning: full\n"));
        assert!(out.contains(
            "Hugin relay: 0 apps (0 syncing, 0 queued), 0 relays (0 xkrd), 0 messages, no pool job\n"
        ));
        assert!(out.contains("Fee address: none (shares are not submitted)\n"));
        assert!(
            out.contains(
                "p2pool:      not connected, shares 0 accepted, 0 submitted, 0 rejected\n"
            )
        );
        assert!(out.contains("Height:      1/5 (20.00%), top block 0 "));
        assert!(out.contains("Peerlist white:\n10.0.0.1:11898\nPeerlist gray:\n"));
        assert!(out.contains("Unknown command: bogus (type help)\n"));
        assert!(out.contains("Log level set to debug\n"));
        assert!(out.contains("wrong number range, use: set_log <log_level_number_0-4>\n"));
        // `exit` prints nothing itself (the shutdown steps follow)
        assert!(
            out.ends_with("wrong number range, use: set_log <log_level_number_0-4>\n"),
            "{out}"
        );
        // only one status block: the line after `exit` is not read
        assert_eq!(out.matches("Tx pool:").count(), 1);
        assert!(shutdown.is_cancelled());
        // `exit` leaves the log level alone
        assert_eq!(*filters.lock().unwrap(), ["debug"]);
    }

    /// The end of the input does not stop the node; `exit` does.
    #[test]
    fn eof_keeps_running_and_exit_stops() {
        let tc = TestChain::new("eof");
        let (_n, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_m, mining) = watch::channel(Arc::new(MiningStatus::default()));
        let state = NodeState {
            chain: tc.chain.clone().unwrap(),
            net,
            mining,
            hugin: None,
            fee: None,
            p2pool: None,
            miner: None,
            hyper: false,
            private: None,
            version: "1".into(),
            start_time: 0,
            db_path: tc.dir.join("missing.redb"),
        };
        let shutdown = CancellationToken::new();
        let console = Console::new(
            Arc::new(Shared::with_node(state)),
            None,
            ExitRequest::new(shutdown.clone()),
            Style::new(false),
            "1",
        );
        let mut out = Vec::new();
        assert!(!console.run(&mut io::Cursor::new("height\n"), &mut out));
        assert!(!shutdown.is_cancelled());
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("Height:      1/1 (100.00%)")
        );

        let mut out = Vec::new();
        assert!(!console.run(&mut io::Cursor::new("set_log 2\n"), &mut out));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "The log level cannot be changed in this mode\n"
        );

        let mut out = Vec::new();
        assert!(console.run(&mut io::Cursor::new("exit\n"), &mut out));
        assert!(shutdown.is_cancelled());
        // a second request changes nothing
        ExitRequest::new(shutdown).request();
    }

    /// Before the chain is open: `help` works, the node commands name the start-up step,
    /// `exit` stops the start-up; once ready the same console answers from the node.
    #[test]
    fn answers_while_starting() {
        let shared = Arc::new(Shared::new());
        let shutdown = CancellationToken::new();
        let console = Console::new(
            shared.clone(),
            None,
            ExitRequest::new(shutdown.clone()),
            Style::new(false),
            "2.0.0",
        );
        let mut out = Vec::new();
        assert!(!console.run(&mut io::Cursor::new("status\n"), &mut out));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "the node is starting: starting\n"
        );

        shared.set_phase("Storage");
        let mut out = Vec::new();
        assert!(!console.run(
            &mut io::Cursor::new("help\nstatus\nheight\nprint_pl\n"),
            &mut out
        ));
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("Console commands · xkrd 2.0.0\n"));
        assert_eq!(out.matches("the node is starting: Storage\n").count(), 3);
        assert!(!shutdown.is_cancelled());

        // ready: the same console answers from the node
        let tc = TestChain::new("starting");
        let (_n, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_m, mining) = watch::channel(Arc::new(MiningStatus::default()));
        shared.ready(NodeState {
            chain: tc.chain.clone().unwrap(),
            net,
            mining,
            hugin: None,
            fee: None,
            p2pool: None,
            miner: None,
            hyper: false,
            private: None,
            version: "2.0.0".into(),
            start_time: 0,
            db_path: tc.dir.join("chain.redb"),
        });
        let mut out = Vec::new();
        assert!(!console.run(&mut io::Cursor::new("height\n"), &mut out));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with("Height:      1/1 (100.00%)")
        );

        // `exit` during start-up cancels it
        let early = Console::new(
            Arc::new(Shared::new()),
            None,
            ExitRequest::new(shutdown.clone()),
            Style::new(false),
            "2.0.0",
        );
        let mut out = Vec::new();
        assert!(early.run(&mut io::Cursor::new("exit\n"), &mut out));
        assert!(out.is_empty());
        assert!(shutdown.is_cancelled());
    }

    /// `invite` prints the private node's invite on a line of its own; `invite new` writes a
    /// new one; a node without `--private` says so.
    #[test]
    fn invite_command() {
        let tc = TestChain::new("invite");
        let private = PrivateNode::load(&tc.dir).unwrap();
        let first = private.invite().to_string();
        let node = |private: Option<Arc<PrivateNode>>| {
            let (_n, net) = watch::channel(Arc::new(NetStatus::default()));
            let (_m, mining) = watch::channel(Arc::new(MiningStatus::default()));
            NodeState {
                chain: tc.chain.clone().unwrap(),
                net,
                mining,
                hugin: None,
                fee: None,
                p2pool: None,
                miner: None,
                hyper: true,
                private,
                version: "1".into(),
                start_time: 0,
                db_path: tc.dir.join("chain.redb"),
            }
        };
        let console = |state: NodeState| {
            Console::new(
                Arc::new(Shared::with_node(state)),
                None,
                ExitRequest::new(CancellationToken::new()),
                Style::new(false),
                "1",
            )
        };
        let c = console(node(Some(private.clone())));
        assert_eq!(
            session(&c, "invite\n"),
            format!("Private node invite (for --private-peer):\n{first}\n")
        );
        let out = session(&c, "invite new\n");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!(lines[0], "New private node invite:");
        assert_ne!(lines[1], first);
        assert_eq!(lines[1], private.invite().to_string());
        assert!(lines[1].parse::<xkr_p2p_hyper::Invite>().is_ok());
        assert_eq!(lines[2], "Old invites stop working; Hyperswarm restarts.");
        let plain = console(node(None));
        assert_eq!(
            session(&plain, "invite\ninvite new\n"),
            "Not a private node (start xkrd with --private)\n".repeat(2)
        );
    }

    /// A console on a fresh node whose fee address lives in `tc`'s directory.
    fn fee_console(tc: &TestChain, ask: bool) -> (Console, Arc<FeeAddress>) {
        let file = tc
            .dir
            .join("hugin")
            .join(xkr_hugin::identity::IDENTITY_FILE);
        let fee = FeeAddress::load(&file, Network::Testnet, None).unwrap();
        if ask {
            fee.ask();
        }
        let (_n, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_m, mining) = watch::channel(Arc::new(MiningStatus::default()));
        let state = NodeState {
            chain: tc.chain.clone().unwrap(),
            net,
            mining,
            hugin: Some(Arc::new(HuginStats::default())),
            fee: Some(fee.clone()),
            p2pool: None,
            miner: None,
            hyper: true,
            private: None,
            version: "1".into(),
            start_time: 0,
            db_path: tc.dir.join("chain.redb"),
        };
        let console = Console::new(
            Arc::new(Shared::with_node(state)),
            None,
            ExitRequest::new(CancellationToken::new()),
            Style::new(false),
            "1",
        );
        (console, fee)
    }

    fn session(console: &Console, input: &str) -> String {
        let mut out = Vec::new();
        assert!(!console.run(&mut io::Cursor::new(input.to_owned()), &mut out));
        String::from_utf8(out).unwrap()
    }

    /// The start-up question: Enter keeps the donation and is saved (not asked again).
    #[test]
    fn fee_question_enter_keeps_the_donation() {
        let tc = TestChain::new("fee-keep");
        let (console, fee) = fee_console(&tc, true);
        assert!(fee.should_ask());
        let out = session(&console, "\n\n");
        assert_eq!(
            out,
            "Keeping the donation to nilsjr. Change it any time with fee-address <address>.\n"
        );
        assert!(!fee.question_pending());
        assert!(!fee.should_ask());
        let file = tc
            .dir
            .join("hugin")
            .join(xkr_hugin::identity::IDENTITY_FILE);
        assert!(
            !FeeAddress::load(&file, Network::Testnet, None)
                .unwrap()
                .should_ask()
        );
    }

    /// `fee-address` shows, refuses a bad address (the question stays open), sets a good one
    /// (saved, applied, the question answered); `hugin_address` is the same command.
    #[test]
    fn fee_address_command() {
        let tc = TestChain::new("fee-set");
        let (console, fee) = fee_console(&tc, true);
        let mine = xkr_hugin::NodeIdentity::generate().address;
        let out = session(
            &console,
            &format!("fee-address\nfee-address bogus\nhugin_address {mine}\nfee_address\n"),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "Fee address: {} (donation to nilsjr)",
                crate::fee::DEFAULT_FEE_ADDRESS
            )
        );
        assert!(lines[1].starts_with("Fee address not changed: not a valid XKR address"));
        assert_eq!(
            lines[2],
            format!(
                "Fee address set to {} (saved)",
                crate::fee::short_address(&mine)
            )
        );
        assert_eq!(lines[3], format!("Fee address: {mine}"));
        assert_eq!(fee.get().address.as_deref(), Some(mine.as_str()));
        assert!(!fee.question_pending());
        assert!(!fee.should_ask());
        // an empty line now is just an empty line
        assert_eq!(session(&console, "\n"), "");
    }

    /// Any other command drops the question without answering it: asked again next start.
    #[test]
    fn other_commands_drop_the_question() {
        let tc = TestChain::new("fee-other");
        let (console, fee) = fee_console(&tc, true);
        let out = session(&console, "height\n\n");
        assert!(out.starts_with("Height:"), "{out}");
        assert!(!out.contains("Keeping"));
        assert!(!fee.question_pending());
        assert!(fee.should_ask());
        // without the relay
        let tc2 = TestChain::new("fee-off");
        let (_n, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_m, mining) = watch::channel(Arc::new(MiningStatus::default()));
        let console = Console::new(
            Arc::new(Shared::with_node(NodeState {
                chain: tc2.chain.clone().unwrap(),
                net,
                mining,
                hugin: None,
                fee: None,
                p2pool: None,
                miner: None,
                hyper: false,
                private: None,
                version: "1".into(),
                start_time: 0,
                db_path: tc2.dir.join("chain.redb"),
            })),
            None,
            ExitRequest::new(CancellationToken::new()),
            Style::new(false),
            "1",
        );
        assert_eq!(
            session(&console, "fee-address\n"),
            "The Hugin relay and p2pool are off: no fee address in use\n"
        );
    }
}
