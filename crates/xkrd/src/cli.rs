//! Command line.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use xkr_consensus::Network;

/// `--help` headings (the groups of the C++ `kryptokronad --help`, plus Hyperswarm and Hugin).
const P2P: &str = "P2P (legacy Levin/TCP)";
const HYPER: &str = "P2P (Hyperswarm)";
const RPC: &str = "RPC";
const STORAGE: &str = "Storage";
const CHAIN: &str = "Chain and verification";
const MINING: &str = "Mining";
const P2POOL: &str = "P2Pool (built-in)";
const HUGIN: &str = "Hugin relay";

/// Whether the built-in p2pool runs on mainnet without `--p2pool` / `--no-p2pool`
/// ([`Args::p2pool_enabled`]). The testnet needs `--p2pool` (and its own sidechain).
pub const P2POOL_DEFAULT: bool = true;
/// Whether the built-in p2pool also uses TCP to other p2pool nodes (C++ nodes) when neither
/// `--p2pool-tcp` nor `--no-p2pool-tcp` is given. Hyperswarm is always used.
pub const P2POOL_TCP_DEFAULT: bool = true;
const LOGGING: &str = "Logging and console";

/// Shown after the flags in `--help`.
const AFTER_HELP: &str = "Console commands (typed while the node runs in a terminal): help, \
                          status, height, print_pl, set_log <0-4>, fee-address [<address>], \
                          start_mining <address>|p2pool [threads], stop_mining, exit. \
                          `xkrd db --help` lists the database tools.";

/// Kryptokrona node.
///
/// Syncs the chain from legacy (C++) peers over Levin/TCP and Hyperswarm, verifies every block
/// and stores it, serves the C++-compatible RPC. In a terminal it prints the Kryptokrona header,
/// one line per new block (a progress line while syncing) and takes console commands like the
/// C++ `kryptokronad` (`--no-console` turns them off; they are off when stdin is not a
/// terminal); logs are quiet by default (`--log-level`). Run by a desktop app:
/// `--log-json` (JSON logs on stdout, at info by default), graceful shutdown on SIGINT/SIGTERM.
#[derive(Debug, Parser)]
#[command(
    name = "xkrd",
    version,
    about,
    long_about = None,
    after_help = AFTER_HELP,
    args_conflicts_with_subcommands = true
)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent command line on/off flags"
)]
pub struct Args {
    /// Tools instead of the node (`xkrd db stats|compact|train-dict|verify|prune|convert`).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Network: `mainnet` or `testnet` (the C++ `USE_TESTNET` network).
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    pub network: NetworkArg,

    /// Network and RPC runtime threads (default: min(available cores, 4)).
    #[arg(long, value_name = "N")]
    pub io_threads: Option<usize>,

    // ------------------------------------------------------------------ P2P (legacy)
    /// Peer to connect to and keep connected (repeatable), `host:port`.
    #[arg(long = "add-peer", value_name = "HOST:PORT", help_heading = P2P)]
    pub add_peer: Vec<String>,

    /// Connect only to these peers (repeatable), `host:port`.
    #[arg(long = "add-exclusive-node", value_name = "HOST:PORT", help_heading = P2P)]
    pub add_exclusive_node: Vec<String>,

    /// Seed node (repeatable), `host:port`; replaces the built-in list.
    #[arg(long = "seed-node", value_name = "HOST:PORT", help_heading = P2P)]
    pub seed_node: Vec<String>,

    /// Listen address for legacy peers.
    #[arg(long, value_name = "IP", default_value = "0.0.0.0", help_heading = P2P)]
    pub p2p_bind_ip: IpAddr,

    /// Listen port for legacy peers (default 11897, testnet 11898); 0 disables listening.
    #[arg(long, value_name = "PORT", help_heading = P2P)]
    pub p2p_bind_port: Option<u16>,

    /// Port announced to peers (behind NAT port forwarding); defaults to the listen port.
    #[arg(long, value_name = "PORT", help_heading = P2P)]
    pub p2p_external_port: Option<u16>,

    /// Do not announce a listening port (peers will not connect back).
    #[arg(long, help_heading = P2P)]
    pub hide_my_port: bool,

    /// Accept private (RFC 1918), link-local and shared (100.64/10) addresses from peer lists
    /// (always on for testnet). Loopback, unspecified, broadcast and multicast addresses are
    /// never accepted from remote peers.
    #[arg(long, help_heading = P2P)]
    pub allow_local_ip: bool,

    /// Outgoing connections to maintain.
    #[arg(long, value_name = "N", default_value_t = 8, help_heading = P2P)]
    pub out_peers: usize,

    /// Maximum incoming connections.
    #[arg(long, value_name = "N", default_value_t = 64, help_heading = P2P)]
    pub in_peers: usize,

    /// Maximum simultaneous incoming connections from one IP address, counting an IPv6 /64 as
    /// one address (loopback exempt).
    #[arg(long, value_name = "N", default_value_t = 4, help_heading = P2P)]
    pub max_conns_per_ip: usize,

    /// Seconds a misbehaving peer stays banned (24 h, Monero's `P2P_IP_BLOCKTIME`).
    #[arg(long, value_name = "SECONDS", default_value_t = 86_400, help_heading = P2P)]
    pub ban_time: u64,

    // ------------------------------------------------------------------ P2P (Hyperswarm)
    /// Disable the Hyperswarm transport (Levin over the DHT: no open ports, no seed nodes).
    #[arg(long, help_heading = HYPER)]
    pub no_hyper: bool,

    /// UDP port for the Hyperswarm network (node sync, Hugin, p2pool); default: a free port,
    /// holepunching needs no open port. Forward this UDP port only behind a very strict
    /// firewall. Not the legacy P2P port (--p2p-bind-port, TCP).
    #[arg(
        long,
        alias = "hyper-udp-port",
        value_name = "PORT",
        value_parser = clap::value_parser!(u16).range(1..),
        help_heading = HYPER
    )]
    pub port: Option<u16>,

    /// Directory of the Hyperswarm bridge (`src/worker.js`, which the Bare workers run, and
    /// `node_modules`). Default: `$XKR_BRIDGE_DIR`,
    /// then `bridge/` next to the executable or in its parent directories (`../../bridge` for
    /// `target/<profile>/xkrd`), if it belongs to the executable's owner and only they can
    /// change it.
    #[arg(long, value_name = "DIR", help_heading = HYPER)]
    pub hyper_bridge_dir: Option<PathBuf>,

    /// DHT bootstrap node (repeatable), `host:port`; default: the public Hyperswarm bootstrap
    /// nodes.
    #[arg(long = "hyper-bootstrap", value_name = "HOST:PORT", help_heading = HYPER)]
    pub hyper_bootstrap: Vec<String>,

    /// Maximum Hyperswarm peers.
    #[arg(long, value_name = "N", default_value_t = 32, help_heading = HYPER)]
    pub hyper_max_peers: usize,

    /// Only look up and dial Hyperswarm peers; do not announce (no incoming Hyperswarm peers).
    #[arg(long, help_heading = HYPER)]
    pub hyper_client_only: bool,

    // ------------------------------------------------------------------ RPC
    /// RPC listen address. The RPC has no authentication: with 0.0.0.0 anyone who can reach
    /// the port can use it (a warning is logged). Requests must name a loopback address,
    /// `localhost` or this IP in their `Host` header (any IP literal when bound to 0.0.0.0 or
    /// ::); other DNS names are refused with 403 unless `--rpc-allowed-host` lists them.
    #[arg(long, value_name = "IP", default_value = "127.0.0.1", help_heading = RPC)]
    pub rpc_bind_ip: IpAddr,

    /// RPC listen port (default 11898, testnet 11899); 0 disables the RPC server.
    #[arg(long, value_name = "PORT", help_heading = RPC)]
    pub rpc_bind_port: Option<u16>,

    /// Also accept this DNS name in the `Host` header of RPC requests (repeatable; port and
    /// case are ignored), for a node reached by name, for example behind a reverse proxy or
    /// under a LAN name.
    #[arg(long = "rpc-allowed-host", value_name = "NAME", help_heading = RPC)]
    pub rpc_allowed_hosts: Vec<String>,

    /// Allow this web origin (repeatable; `*` for any, `null` for `file://` and sandboxed
    /// pages). Its requests are served and get `Access-Control-Allow-Origin` like C++;
    /// requests from any other web origin are refused with 403. Not needed for wallets,
    /// miners or other non-browser clients, which send no `Origin`.
    #[arg(long = "enable-cors", value_name = "ORIGIN", help_heading = RPC)]
    pub enable_cors: Vec<String>,

    /// Answer the block explorer methods of `/json_rpc` (`f_blocks_list_json`, `f_block_json`,
    /// `f_transaction_json`, `f_on_transactions_pool_json`; C++ `--enable-blockexplorer`). An
    /// explorer needs the whole chain, so this is not accepted with `--prune` and the node
    /// does not start on a pruned database.
    #[arg(
        long,
        alias = "enable-blockexplorer",
        conflicts_with = "prune",
        help_heading = RPC
    )]
    pub explorer: bool,

    /// Remote node fee address reported by `/fee` (empty: no fee).
    #[arg(long, value_name = "ADDRESS", default_value = "", help_heading = RPC)]
    pub fee_address: String,

    /// Remote node fee in atomic units reported by `/fee`.
    #[arg(long, value_name = "AMOUNT", default_value_t = 0, help_heading = RPC)]
    pub fee_amount: u32,

    // ------------------------------------------------------------------ storage
    /// Data directory (default `~/.xkr`, testnet `~/.xkr/testnet`; `%APPDATA%\xkr` on Windows).
    #[arg(long, value_name = "DIR", help_heading = STORAGE)]
    pub data_dir: Option<PathBuf>,

    /// Database page cache in MiB.
    #[arg(long, value_name = "MIB", default_value_t = 512, help_heading = STORAGE)]
    pub db_cache_mb: usize,

    /// While syncing, batches are committed without fsync and a durable commit happens at
    /// least this often (seconds). Pages freed since the last durable commit cannot be reused,
    /// so a long interval grows the file during sync.
    #[arg(long, value_name = "SECONDS", default_value_t = 5, help_heading = STORAGE)]
    pub db_durable_secs: u64,

    /// Do not repack and compact the database automatically when the initial sync completes
    /// (`xkrd db compact` does the same by hand).
    #[arg(long, help_heading = STORAGE)]
    pub no_db_maintenance: bool,

    /// Stripe pruning (`docs/storage.md` "Stripe pruning"): keep the ring signatures of one
    /// randomly picked stripe of 4 096-block spans below min(last checkpoint, tip - 8 192) and
    /// drop the others (about 10 GB instead of 19 GB on mainnet). The seed is picked at the first
    /// start with this flag and stored; a full database started with it is pruned. A database
    /// that has a seed keeps pruning with it, with or without this flag.
    #[arg(long, help_heading = STORAGE)]
    pub prune: bool,

    /// Stripes of the seed `--prune` picks (2..=8, default 4). Only used when the seed is
    /// picked; a database that already has a seed keeps its own.
    #[arg(
        long,
        value_name = "N",
        requires = "prune",
        value_parser = clap::value_parser!(u8).range(2..=8),
        help_heading = STORAGE
    )]
    pub prune_stripes: Option<u8>,

    // ------------------------------------------------------------------ chain
    /// Skip proof-of-work verification up to the last built-in checkpoint (everything else
    /// is still verified).
    #[arg(long, help_heading = CHAIN)]
    pub fast_sync: bool,

    /// Verification threads for proof of work and signatures (default: available cores - 1).
    #[arg(long, value_name = "N", help_heading = CHAIN)]
    pub verify_threads: Option<usize>,

    /// Refuse chain reorganisations deeper than this many blocks (node policy). On a pruning
    /// or pruned database at most 7 168 (the newest 8 192 blocks stay whole); a higher value
    /// is clamped with a warning.
    #[arg(long, value_name = "BLOCKS", default_value_t = 1_000, help_heading = CHAIN)]
    pub max_reorg_depth: u64,

    /// Testnet only: schedule fork 13 (block major version 6) at this height; the next block is
    /// the first v6 block. 0 = not scheduled (the default). See `docs/forks.md`.
    #[arg(long, value_name = "HEIGHT", help_heading = CHAIN)]
    pub fork13_height: Option<u32>,

    /// Testnet only: schedule fork 14 (block major version 7, transaction version 2 with view
    /// tags and CLSAG) at this height; the next block is the first v7 block. 0 = not scheduled
    /// (the default). Schedule it after fork 13. See `docs/forks.md` and `docs/tx-v2.md`.
    #[arg(long, value_name = "HEIGHT", help_heading = CHAIN)]
    pub fork14_height: Option<u32>,

    /// Testnet only: blocks after fork 14 during which version-1 transactions are still valid
    /// (default: the testnet parameter, 20). 0 = version 2 only from the first v7 block.
    #[arg(long, value_name = "BLOCKS", help_heading = CHAIN)]
    pub fork14_grace: Option<u64>,

    // ------------------------------------------------------------------ mining
    /// Mine solo to this address with the built-in CPU miner (`SEKR…` or `Xkr…`).
    #[arg(long, value_name = "ADDRESS", help_heading = MINING)]
    pub mine: Option<String>,

    /// Mine with the built-in CPU miner via the built-in p2pool (on unless --no-p2pool; starts
    /// once p2pool runs). Shares pay the node's p2pool payout address, the fee address
    /// (`--hugin`, console `fee-address`). Console: `start_mining p2pool`.
    #[arg(long, conflicts_with = "mine", help_heading = MINING)]
    pub mine_p2pool: bool,

    /// Mining threads (default: half of the available cores, at least 1).
    #[arg(long, value_name = "N", help_heading = MINING)]
    pub mine_threads: Option<usize>,

    // ------------------------------------------------------------------ p2pool
    /// Run the built-in p2pool (`docs/mining.md`, "Built-in p2pool"): the Kryptokrona sidechain,
    /// a stratum server for local miners and the sidechain P2P network (outgoing TCP to other
    /// p2pool nodes and Hyperswarm). It starts once the node is synchronized and pays every share
    /// to the node's fee address (`--hugin`, console `fee-address`). On by default on mainnet;
    /// the testnet needs this flag.
    #[arg(long, overrides_with_all = ["p2pool", "no_p2pool"], help_heading = P2POOL)]
    pub p2pool: bool,

    /// Do not run the built-in p2pool.
    #[arg(long, overrides_with_all = ["p2pool", "no_p2pool"], help_heading = P2POOL)]
    pub no_p2pool: bool,

    /// Stratum listen address of the built-in p2pool, `ip:port`. The default takes local miners
    /// only; 0.0.0.0:3333 takes miners from the network (no authentication: any miner that
    /// reaches it mines for this node's payout address).
    #[arg(
        long,
        value_name = "IP:PORT",
        default_value_t = xkr_p2pool::stratum::DEFAULT_STRATUM_ADDR,
        help_heading = P2POOL
    )]
    pub p2pool_stratum: SocketAddr,

    /// Seconds a misbehaving stratum miner stays banned (C++ `--stratum-ban-time`).
    #[arg(long, value_name = "SECONDS", default_value_t = 600, help_heading = P2POOL)]
    pub p2pool_stratum_ban_time: u64,

    /// Fixed stratum difficulty from the sidechain instead of a per-miner difficulty from its
    /// share times (C++ `--no-autodiff`).
    #[arg(long, help_heading = P2POOL)]
    pub p2pool_no_autodiff: bool,

    /// Also connect to sidechain peers over TCP (other p2pool nodes, incl. the C++ p2pool):
    /// seeds, the peer list and `--p2pool-addpeers`. On by default for now; Hyperswarm is
    /// always used.
    #[arg(long, overrides_with_all = ["p2pool_tcp", "no_p2pool_tcp"], help_heading = P2POOL)]
    pub p2pool_tcp: bool,

    /// Reach sidechain peers over Hyperswarm only (no TCP seeds, dialing or listener).
    #[arg(long, overrides_with_all = ["p2pool_tcp", "no_p2pool_tcp"], help_heading = P2POOL)]
    pub no_p2pool_tcp: bool,

    /// Accept sidechain peers over TCP on this address, `ip:port` (C++ `--p2p`; 37889, mini
    /// 37888, nano 37890). Its port is also the one announced to peers. Without it the node
    /// only connects out (TCP) and over Hyperswarm.
    #[arg(long, value_name = "IP:PORT", help_heading = P2POOL)]
    pub p2pool_p2p: Option<SocketAddr>,

    /// Sidechain peers to connect to at start, `host:port` (repeatable or comma-separated; C++
    /// `--addpeers`).
    #[arg(
        long,
        value_name = "HOST:PORT",
        value_delimiter = ',',
        help_heading = P2POOL
    )]
    pub p2pool_addpeers: Vec<String>,

    /// Outgoing TCP connections to sidechain peers (C++ `--out-peers`).
    #[arg(long, value_name = "N", default_value_t = 10, help_heading = P2POOL)]
    pub p2pool_out_peers: usize,

    /// Incoming TCP connections from sidechain peers, with `--p2pool-p2p` (C++ `--in-peers`).
    #[arg(long, value_name = "N", default_value_t = 450, help_heading = P2POOL)]
    pub p2pool_in_peers: usize,

    /// Allow more than one sidechain connection per IP address (C++ `--p2p-allow-same-ip`).
    #[arg(long, help_heading = P2POOL)]
    pub p2pool_allow_same_ip: bool,

    /// Mine on the mini sidechain (C++ `--mini`).
    #[arg(long, conflicts_with_all = ["p2pool_nano", "p2pool_config"], help_heading = P2POOL)]
    pub p2pool_mini: bool,

    /// Mine on the nano sidechain (C++ `--nano`).
    #[arg(long, conflicts_with = "p2pool_config", help_heading = P2POOL)]
    pub p2pool_nano: bool,

    /// A custom sidechain from this JSON file (`name`, `password`, `block_time`, `min_diff`,
    /// `pplns_window`, `uncle_penalty`; C++ `--sidechain-config`). Every node of the sidechain
    /// needs the same file.
    #[arg(long, value_name = "FILE", help_heading = P2POOL)]
    pub p2pool_config: Option<PathBuf>,

    /// Testnet only: no PPLNS weight cap on the sidechain (every node of the sidechain must set
    /// it alike; `XKR_TESTNET_UNCAPPED_PPLNS=1` does the same).
    #[arg(long, help_heading = P2POOL)]
    pub p2pool_uncapped_pplns: bool,

    // ------------------------------------------------------------------ Hugin
    /// Do not run the Hugin relay. By default every node with the Hyperswarm transport is also a
    /// hugin-node relay for Hugin apps (`docs/hugin-relay.md`); `--no-hyper` turns it off too.
    #[arg(long, help_heading = HUGIN)]
    pub no_hugin: bool,

    /// The relay is on by default; this flag is accepted and does nothing.
    #[arg(long, hide = true, help_heading = HUGIN)]
    pub hugin_relay: bool,

    /// Fee address of this node: the Hugin relay submits its apps' shares to p2pool under it.
    /// Saved in `<data-dir>/hugin/identity.json` and used from then on (`fee-address` in the
    /// console changes it while the node runs). Without one the fee goes to the default
    /// donation address (nilsjr). A standard `SEKR…` or `Xkr…` address.
    #[arg(long = "hugin", alias = "hugin-payout", value_name = "ADDRESS", help_heading = HUGIN)]
    pub hugin: Option<String>,

    /// p2pool stratum node for the Hugin shares (repeatable, tried in order; the first is
    /// returned to when it is reachable again). Default: the local p2pool at 127.0.0.1:3333
    /// (on Linux only when its port is not held by another user), then the public p2pool nodes
    /// built into xkrd. `stratum+ssl://host:port` uses TLS.
    #[arg(long = "hugin-p2pool", value_name = "HOST:PORT", help_heading = HUGIN)]
    pub hugin_p2pool: Vec<String>,

    /// Share difficulty of the jobs the relay hands to Hugin apps (about one second per
    /// message on a phone at 200; at least 1). Only shares that also meet p2pool's target are
    /// submitted; the p2pool login asks for 1000 (p2pool's least), or N when it is higher.
    /// Other relays drop messages easier than 200.
    #[arg(
        long,
        value_name = "N",
        default_value_t = xkr_hugin::relay::DEFAULT_APP_DIFFICULTY,
        help_heading = HUGIN
    )]
    pub hugin_share_difficulty: u64,

    /// Most Hugin app connections (public and private topics together). Below it, new apps
    /// are paused while many apps fetch their stored messages (see --hugin-sync-slots and
    /// --hugin-queue-budget-mb); connected apps are never closed for that.
    #[arg(
        long,
        value_name = "N",
        default_value_t = xkr_hugin::relay::DEFAULT_MAX_CLIENTS,
        help_heading = HUGIN
    )]
    pub hugin_max_clients: usize,

    /// Most Hugin apps fetching their stored messages at once (history syncs); further sync
    /// requests wait in arrival order (at least 1).
    #[arg(
        long,
        value_name = "N",
        default_value_t = xkr_hugin::relay::DEFAULT_SYNC_SLOTS,
        help_heading = HUGIN
    )]
    pub hugin_sync_slots: usize,

    /// Send budget of all Hugin app connections together, in MiB: control packets,
    /// notifications and the history syncs being sent. A sync starts only when it fits; new
    /// apps are paused at 80 % and resumed below 60 % (at least 1).
    #[arg(
        long,
        value_name = "MIB",
        default_value_t = xkr_hugin::relay::DEFAULT_QUEUE_BUDGET / (1024 * 1024),
        help_heading = HUGIN
    )]
    pub hugin_queue_budget_mb: usize,

    /// Most Hugin app connections from one IP address or IPv6 /64, with 8 per /56 and 16 per
    /// /48 (at least N; 0: none of these limits). Apps whose address is not known are not
    /// limited by it; relay connections have their own limit of 4 per address.
    #[arg(
        long,
        value_name = "N",
        default_value_t = xkr_hugin::relay::DEFAULT_MAX_CLIENTS_PER_IP,
        help_heading = HUGIN
    )]
    pub hugin_max_clients_per_ip: usize,

    /// Private Hugin node: do not announce on the public-node topic (invited apps only).
    #[arg(long, help_heading = HUGIN)]
    pub hugin_private: bool,

    // ------------------------------------------------------------------ logging and console
    /// Log level, as in C++: 0 or 1 errors, 2 warnings (the default), 3 info, 4 debug, 5 trace.
    /// A tracing filter (`info`, `xkrd=debug,warn`, ...) works too. `RUST_LOG` overrides it;
    /// with `--log-json` the default is info. The header, the block lines and the sync
    /// progress are printed at every level (not with `--log-json`).
    #[arg(
        long,
        value_name = "0-5|FILTER",
        value_parser = parse_log_level,
        help_heading = LOGGING
    )]
    pub log_level: Option<LogLevel>,

    /// Also write the log to this file (appended, plain text, same level as the console).
    #[arg(long, value_name = "FILE", help_heading = LOGGING)]
    pub log_file: Option<PathBuf>,

    /// Structured JSON logs on stdout (for a desktop app); no header, block or progress lines.
    #[arg(long, help_heading = LOGGING)]
    pub log_json: bool,

    /// Disable the console commands (`help`, `status`, ...). They are always off when stdin is
    /// not a terminal.
    #[arg(long, help_heading = LOGGING)]
    pub no_console: bool,
}

impl Args {
    /// The tracing filter the node starts with: `--log-level`, else info with `--log-json`
    /// (the desktop app reads the info events) and warnings otherwise (C++ `logging::WARNING`,
    /// `DaemonConfiguration`'s default). `RUST_LOG` overrides it (`main::init_logging`).
    #[must_use]
    pub fn log_filter(&self) -> String {
        match &self.log_level {
            Some(level) => level.filter(),
            None if self.log_json => "info".to_owned(),
            None => cpp_level_filter(2).to_owned(),
        }
    }

    /// Whether the built-in p2pool runs: `--p2pool` or `--no-p2pool` (the later one on the
    /// command line wins), else [`P2POOL_DEFAULT`] on mainnet and off on the testnet.
    #[must_use]
    pub fn p2pool_enabled(&self) -> bool {
        let default = P2POOL_DEFAULT && matches!(self.network, NetworkArg::Mainnet);
        p2pool_choice(self.p2pool, self.no_p2pool, default)
    }

    /// Whether the built-in p2pool uses TCP besides Hyperswarm: the flag given last, else
    /// [`P2POOL_TCP_DEFAULT`].
    #[must_use]
    pub const fn p2pool_tcp_enabled(&self) -> bool {
        p2pool_choice(self.p2pool_tcp, self.no_p2pool_tcp, P2POOL_TCP_DEFAULT)
    }
}

/// `--p2pool` / `--no-p2pool` (at most one is set, clap resets the other) over the default.
const fn p2pool_choice(on: bool, off: bool, default: bool) -> bool {
    on || (!off && default)
}

/// `--log-level`: a C++ level number or a tracing filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    /// C++ `logging::Level` (`src/logging/ilogger.h`), 0..=5.
    Cpp(u8),
    /// A tracing `EnvFilter` directive list.
    Filter(String),
}

impl LogLevel {
    /// The tracing filter.
    #[must_use]
    pub fn filter(&self) -> String {
        match self {
            Self::Cpp(n) => cpp_level_filter(*n).to_owned(),
            Self::Filter(f) => f.clone(),
        }
    }
}

/// The tracing level of a C++ `logging::Level` (`src/logging/ilogger.h`: `FATAL` 0, `ERROR` 1,
/// `WARNING` 2, `INFO` 3, `DEBUGGING` 4, `TRACE` 5); tracing has no fatal level, so 0 is
/// errors too.
#[must_use]
pub fn cpp_level_filter(level: u8) -> &'static str {
    match level {
        0 | 1 => "error",
        2 => "warn",
        3 => "info",
        4 => "debug",
        _ => "trace",
    }
}

/// Parses `--log-level`: a number 0..=5 (C++ `DaemonConfiguration::logLevel`) or a valid
/// tracing filter.
pub fn parse_log_level(s: &str) -> Result<LogLevel, String> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return match s.parse::<u8>() {
            Ok(n) if n <= 5 => Ok(LogLevel::Cpp(n)),
            _ => Err(
                "a level number is 0-5 (0/1 errors, 2 warnings, 3 info, 4 debug, 5 trace)".into(),
            ),
        };
    }
    check_filter(s)?;
    Ok(LogLevel::Filter(s.to_owned()))
}

/// Whether `s` is a tracing filter `EnvFilter` accepts.
pub fn check_filter(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty log filter".into());
    }
    tracing_subscriber::EnvFilter::builder()
        .parse(s)
        .map(drop)
        .map_err(|e| format!("not a level number 0-5 or a log filter: {e}"))
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Chain database tools. Stop the node first (`stats` also works on a copy of a running
    /// node's file, and on databases of the previous schema version).
    #[command(subcommand)]
    Db(DbCommand),
    /// Start one Bare worker of the Hyperswarm bridge and stop it again, without joining the
    /// network: checks that libbare-kit, the bridge directory and its native addons load.
    /// Prints `bridge ok: <dir>`.
    BridgeCheck {
        /// Directory of the Hyperswarm bridge. Default: as for the node (`$XKR_BRIDGE_DIR`, then
        /// `bridge/` next to the executable).
        #[arg(long, value_name = "DIR")]
        hyper_bridge_dir: Option<PathBuf>,
    },
}

/// `xkrd db ...`.
#[derive(Debug, Subcommand)]
pub enum DbCommand {
    /// Per-table entries and bytes, page usage, file size and the raw chain size
    /// (sum of `block_size`), so the storage ratio is visible.
    Stats(DbArgs),
    /// Repack under-filled index tables and compact the file (redb `compact`).
    Compact {
        #[command(flatten)]
        db: DbArgs,
        /// Repack every repackable table and compact regardless of the thresholds.
        #[arg(long)]
        force: bool,
        /// Only compact (redb `compact`), do not repack tables.
        #[arg(long, conflicts_with = "force")]
        no_repack: bool,
    },
    /// Train a zstd dictionary over block records, store it in the database and re-compress
    /// every sealed chunk with it (see `crates/xkr-storage/dicts/README.md`).
    TrainDict {
        #[command(flatten)]
        db: DbArgs,
        /// Block records sampled, spread evenly over the chain.
        #[arg(long, value_name = "N", default_value_t = 20_000)]
        samples: usize,
        /// Maximum dictionary size in bytes.
        #[arg(long, value_name = "BYTES", default_value_t = 110_000)]
        max_dict_bytes: usize,
        /// Also write the dictionary to this file (e.g. `crates/xkr-storage/dicts/mainnet.zdict`).
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// Use this dictionary instead of training one.
        #[arg(long, value_name = "FILE")]
        dict: Option<PathBuf>,
        /// Only train (and write `--out`); do not touch the database.
        #[arg(long)]
        no_recompress: bool,
    },
    /// Walk every block and cross-check all tables (hashes, indexes, outputs, key images).
    Verify {
        #[command(flatten)]
        db: DbArgs,
        /// First height to check (global table counts are only compared for a full walk).
        #[arg(long, value_name = "HEIGHT", default_value_t = 0)]
        from: u64,
        /// Last height to check (default: the top block).
        #[arg(long, value_name = "HEIGHT")]
        to: Option<u64>,
    },
    /// Copy a schema v1 database (the per-transaction layout) into a new v2 database without
    /// re-verifying it, then run maintenance: before/after numbers on the same chain.
    Convert {
        /// The v1 `chain.redb` to read (not modified).
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
        // Where the new database goes (`--db` or `--data-dir`); it must not exist yet.
        #[command(flatten)]
        db: DbArgs,
        /// Blocks per write transaction.
        #[arg(long, value_name = "N", default_value_t = 500)]
        batch: usize,
    },
    /// Delete ring signatures: keep one stripe (`--stripes N [--stripe K]`, or the database's
    /// stored seed) or none (`--all`) below the safety bound min(last checkpoint, top - 8 192),
    /// rounded down to 4 096 (64 with `--all`). Run `xkrd db compact` afterwards to shrink the
    /// file.
    Prune {
        #[command(flatten)]
        db: DbArgs,
        /// Drop every signature below the bound, keeping no stripe (cannot be undone; a stripe
        /// cannot be kept afterwards without a resync).
        #[arg(long, conflicts_with_all = ["stripes", "stripe"])]
        all: bool,
        /// Stripes of the seed (2..=8; default 4 when only `--stripe` is given).
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(2..=8))]
        stripes: Option<u8>,
        /// The stripe to keep, 1..=N (default: a random one).
        #[arg(long, value_name = "K", value_parser = clap::value_parser!(u8).range(1..=8))]
        stripe: Option<u8>,
        /// Prune below this height instead of the safety bound. A height above the bound is
        /// refused unless `--force-below` is given too.
        #[arg(long, value_name = "HEIGHT")]
        below: Option<u64>,
        /// Allow `--below` above the safety bound (min(last checkpoint, top - 8192)): reorgs
        /// into the pruned blocks then fail (the node can fork off for good) and peers that
        /// assume the newest blocks whole get misses. For tests (testnet has no bound).
        #[arg(long, requires = "below")]
        force_below: bool,
    },
}

/// Where the database is.
#[derive(Debug, clap::Args)]
pub struct DbArgs {
    /// Network (selects the default data directory).
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    pub network: NetworkArg,
    /// Data directory containing `chain.redb`.
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// The database file itself (overrides `--data-dir`).
    #[arg(long, value_name = "FILE")]
    pub db: Option<PathBuf>,
    /// Database page cache in MiB.
    #[arg(long, value_name = "MIB", default_value_t = 512)]
    pub db_cache_mb: usize,
}

impl DbArgs {
    /// Path of the database file.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.db.clone().unwrap_or_else(|| {
            self.data_dir
                .clone()
                .unwrap_or_else(|| default_data_dir(self.network.into()))
                .join("chain.redb")
        })
    }
}

/// `--network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NetworkArg {
    /// Kryptokrona mainnet.
    Mainnet,
    /// The C++ `USE_TESTNET` network.
    Testnet,
}

impl From<NetworkArg> for Network {
    fn from(n: NetworkArg) -> Self {
        match n {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::Testnet,
        }
    }
}

/// The default data directory: `$HOME/.xkr` (`%APPDATA%\xkr` on Windows), with a `testnet`
/// subdirectory for testnet.
#[must_use]
pub fn default_data_dir(network: Network) -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("xkr"))
    } else {
        std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".xkr"))
    }
    .unwrap_or_else(|| PathBuf::from(".xkr"));
    match network {
        Network::Mainnet => base,
        Network::Testnet => base.join("testnet"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn explorer_needs_a_full_node() {
        assert!(
            Args::try_parse_from(["xkrd", "--explorer"])
                .unwrap()
                .explorer
        );
        assert!(!Args::try_parse_from(["xkrd"]).unwrap().explorer);
        // the C++ spelling
        assert!(
            Args::try_parse_from(["xkrd", "--enable-blockexplorer"])
                .unwrap()
                .explorer
        );
        assert!(Args::try_parse_from(["xkrd", "--explorer", "--prune"]).is_err());
    }

    #[test]
    fn command_line_is_consistent() {
        Args::command().debug_assert();
    }

    #[test]
    fn log_level_takes_cpp_numbers_and_filters() {
        let plain = Args::try_parse_from(["xkrd"]).unwrap();
        assert_eq!(plain.log_level, None);
        assert_eq!(plain.log_filter(), "warn");
        assert!(plain.log_file.is_none());
        assert!(!plain.no_console);
        // the desktop app keeps its info events
        let json = Args::try_parse_from(["xkrd", "--log-json"]).unwrap();
        assert_eq!(json.log_filter(), "info");

        for (arg, filter) in [
            ("0", "error"),
            ("1", "error"),
            ("2", "warn"),
            ("3", "info"),
            ("4", "debug"),
            ("5", "trace"),
        ] {
            let a = Args::try_parse_from(["xkrd", "--log-level", arg]).unwrap();
            assert_eq!(a.log_level, Some(LogLevel::Cpp(arg.parse().unwrap())));
            assert_eq!(a.log_filter(), filter);
        }
        // the C++ `--log-level=N` form; an explicit level wins over the JSON default
        let a = Args::try_parse_from(["xkrd", "--log-level=2", "--log-json"]).unwrap();
        assert_eq!(a.log_filter(), "warn");

        // tracing filters as before (the testnet suite passes `info` and `info,xkr_hugin=debug`)
        for f in ["info", "debug", "info,xkr_hugin=debug", "xkrd=trace,warn"] {
            let a = Args::try_parse_from(["xkrd", "--log-level", f]).unwrap();
            assert_eq!(a.log_level, Some(LogLevel::Filter(f.to_owned())));
            assert_eq!(a.log_filter(), f);
        }
        for bad in ["6", "300", "", "xkrd=notalevel"] {
            assert!(
                Args::try_parse_from(["xkrd", "--log-level", bad]).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn log_file_and_no_console() {
        let a =
            Args::try_parse_from(["xkrd", "--log-file", "/tmp/xkrd.log", "--no-console"]).unwrap();
        assert_eq!(a.log_file.as_deref(), Some(Path::new("/tmp/xkrd.log")));
        assert!(a.no_console);
        // node flags do not combine with the database tools
        assert!(Args::try_parse_from(["xkrd", "--no-console", "db", "stats"]).is_err());
    }

    #[test]
    fn help_is_grouped() {
        let cmd = Args::command();
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str();
            assert!(arg.get_help().is_some(), "--{id} has no help text");
            let heading = arg.get_help_heading();
            if matches!(id, "network" | "io_threads") {
                assert_eq!(heading, None, "--{id}");
            } else {
                assert!(heading.is_some(), "--{id} has no help heading");
            }
        }
        let help = Args::command().render_long_help().to_string();
        let mut at = 0;
        for heading in [
            P2P, HYPER, RPC, STORAGE, CHAIN, MINING, P2POOL, HUGIN, LOGGING,
        ] {
            let pos = help[at..]
                .find(&format!("{heading}:"))
                .unwrap_or_else(|| panic!("heading {heading} missing or out of order"));
            at += pos;
        }
        for flag in [
            "--log-level",
            "--log-file",
            "--no-console",
            "--add-peer",
            "--no-hugin",
            "--hugin-p2pool",
        ] {
            assert!(help.contains(flag), "{flag}");
        }
        assert!(help.contains("Console commands"));
        assert!(help.contains("fee-address"));
        // the hidden alias and the no-op flag still parse
        assert!(!help.contains("--hugin-relay"));
        assert!(!help.contains("--hugin-payout"));
        let old =
            Args::try_parse_from(["xkrd", "--hugin-relay", "--hugin-payout", "SEKRx"]).unwrap();
        assert!(old.hugin_relay && !old.no_hugin);
        assert_eq!(old.hugin.as_deref(), Some("SEKRx"));
    }

    /// `--port`: the one UDP port of the Hyperswarm network, `--hyper-udp-port` as a hidden
    /// alias, 1 to 65535.
    #[test]
    fn port_flag() {
        assert_eq!(Args::try_parse_from(["xkrd"]).unwrap().port, None);
        assert_eq!(
            Args::try_parse_from(["xkrd", "--port", "21890"])
                .unwrap()
                .port,
            Some(21890)
        );
        assert_eq!(
            Args::try_parse_from(["xkrd", "--hyper-udp-port", "21890"])
                .unwrap()
                .port,
            Some(21890)
        );
        assert_eq!(
            Args::try_parse_from(["xkrd", "--port=65535"]).unwrap().port,
            Some(65535)
        );
        for bad in ["0", "65536", "-1", "udp"] {
            assert!(
                Args::try_parse_from(["xkrd", "--port", bad]).is_err(),
                "{bad:?}"
            );
        }
        // the UDP port and the legacy TCP port are separate flags
        let both =
            Args::try_parse_from(["xkrd", "--port", "11897", "--p2p-bind-port", "11897"]).unwrap();
        assert_eq!(both.port, Some(11897));
        let cmd = Args::command();
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_id() == "port")
            .expect("--port");
        assert_eq!(arg.get_long(), Some("port"));
        assert_eq!(arg.get_help_heading(), Some(HYPER));
        let text = arg.get_help().expect("help").to_string();
        assert!(text.contains("holepunching needs no open port"), "{text}");
        assert!(text.contains("--p2p-bind-port, TCP"), "{text}");
        let help = Args::command().render_long_help().to_string();
        assert!(!help.contains("--hyper-udp-port"), "the alias is hidden");
    }

    #[test]
    fn hugin_flags() {
        let plain = Args::try_parse_from(["xkrd"]).unwrap();
        assert!(!plain.no_hugin);
        assert_eq!(plain.hugin, None);
        assert!(plain.hugin_p2pool.is_empty());
        assert_eq!(
            plain.hugin_share_difficulty,
            xkr_hugin::relay::DEFAULT_APP_DIFFICULTY
        );
        assert_eq!(plain.hugin_max_clients, 200);
        assert_eq!(plain.hugin_sync_slots, 8);
        assert_eq!(plain.hugin_queue_budget_mb, 256);
        assert_eq!(
            plain.hugin_max_clients_per_ip,
            xkr_hugin::relay::DEFAULT_MAX_CLIENTS_PER_IP
        );
        let a = Args::try_parse_from([
            "xkrd",
            "--no-hugin",
            "--hugin",
            "SEKRfee",
            "--hugin-p2pool",
            "127.0.0.1:3334",
            "--hugin-p2pool",
            "stratum+ssl://pool.example:4443",
        ])
        .unwrap();
        assert!(a.no_hugin);
        assert_eq!(a.hugin.as_deref(), Some("SEKRfee"));
        assert_eq!(
            a.hugin_p2pool,
            ["127.0.0.1:3334", "stratum+ssl://pool.example:4443"]
        );
        // `--hugin-pool` is gone
        assert!(Args::try_parse_from(["xkrd", "--hugin-pool", "self"]).is_err());
    }

    /// `--p2pool` / `--no-p2pool` (the later one wins; without either on for mainnet, off for
    /// the testnet) and the p2pool settings with their C++ defaults.
    #[test]
    fn p2pool_flags() {
        let plain = Args::try_parse_from(["xkrd"]).unwrap();
        assert_eq!(plain.p2pool_enabled(), P2POOL_DEFAULT);
        assert!(plain.p2pool_enabled());
        let testnet = Args::try_parse_from(["xkrd", "--network", "testnet"]).unwrap();
        assert!(!testnet.p2pool_enabled());
        let testnet_on =
            Args::try_parse_from(["xkrd", "--network", "testnet", "--p2pool"]).unwrap();
        assert!(testnet_on.p2pool_enabled());
        assert_eq!(plain.p2pool_tcp_enabled(), P2POOL_TCP_DEFAULT);
        let hyper_only = Args::try_parse_from(["xkrd", "--p2pool-tcp", "--no-p2pool-tcp"]).unwrap();
        assert!(!hyper_only.p2pool_tcp_enabled());
        let tcp = Args::try_parse_from(["xkrd", "--no-p2pool-tcp", "--p2pool-tcp"]).unwrap();
        assert!(tcp.p2pool_tcp_enabled());
        assert_eq!(plain.p2pool_stratum, "127.0.0.1:3333".parse().unwrap());
        assert_eq!(plain.p2pool_stratum_ban_time, 600);
        assert!(!plain.p2pool_no_autodiff);
        assert_eq!(plain.p2pool_p2p, None);
        assert!(plain.p2pool_addpeers.is_empty());
        assert_eq!((plain.p2pool_out_peers, plain.p2pool_in_peers), (10, 450));
        assert!(!plain.p2pool_allow_same_ip);
        assert!(!plain.p2pool_mini && !plain.p2pool_nano);
        assert_eq!(plain.p2pool_config, None);
        assert!(!plain.p2pool_uncapped_pplns);

        let on = |args: &[&str]| {
            let mut all = vec!["xkrd"];
            all.extend_from_slice(args);
            Args::try_parse_from(all).unwrap().p2pool_enabled()
        };
        assert!(on(&["--p2pool"]));
        assert!(!on(&["--no-p2pool"]));
        assert!(!on(&["--p2pool", "--no-p2pool"]));
        assert!(on(&["--no-p2pool", "--p2pool"]));
        assert!(on(&["--p2pool", "--p2pool"]));
        // the default only applies without either flag
        assert!(p2pool_choice(false, false, true));
        assert!(!p2pool_choice(false, true, true));
        assert!(p2pool_choice(true, false, false));
        assert!(!p2pool_choice(false, false, false));

        let a = Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--p2pool-stratum",
            "0.0.0.0:3334",
            "--p2pool-stratum-ban-time",
            "60",
            "--p2pool-no-autodiff",
            "--p2pool-p2p",
            "0.0.0.0:37888",
            "--p2pool-addpeers",
            "a.example:37888,b.example",
            "--p2pool-addpeers",
            "10.0.0.2:37888",
            "--p2pool-out-peers",
            "4",
            "--p2pool-in-peers",
            "20",
            "--p2pool-allow-same-ip",
            "--p2pool-mini",
        ])
        .unwrap();
        assert!(a.p2pool_enabled());
        assert_eq!(a.p2pool_stratum, "0.0.0.0:3334".parse().unwrap());
        assert_eq!(a.p2pool_stratum_ban_time, 60);
        assert!(a.p2pool_no_autodiff);
        assert_eq!(a.p2pool_p2p, Some("0.0.0.0:37888".parse().unwrap()));
        assert_eq!(
            a.p2pool_addpeers,
            ["a.example:37888", "b.example", "10.0.0.2:37888"]
        );
        assert_eq!((a.p2pool_out_peers, a.p2pool_in_peers), (4, 20));
        assert!(a.p2pool_allow_same_ip && a.p2pool_mini);

        let c = Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--p2pool-config",
            "p2pool/sidechain-testnet.json",
            "--p2pool-uncapped-pplns",
        ])
        .unwrap();
        assert_eq!(
            c.p2pool_config.as_deref(),
            Some(Path::new("p2pool/sidechain-testnet.json"))
        );
        assert!(c.p2pool_uncapped_pplns);

        // one sidechain at a time; addresses must be ip:port
        for bad in [
            &["--p2pool-mini", "--p2pool-nano"][..],
            &["--p2pool-mini", "--p2pool-config", "x.json"],
            &["--p2pool-nano", "--p2pool-config", "x.json"],
            &["--p2pool-stratum", "localhost:3333"],
            &["--p2pool-stratum", "3333"],
            &["--p2pool-p2p", "0.0.0.0"],
        ] {
            let mut all = vec!["xkrd"];
            all.extend_from_slice(bad);
            assert!(Args::try_parse_from(all).is_err(), "{bad:?}");
        }
        // the built-in miner: solo to an address, or via p2pool, not both
        assert!(!plain.mine_p2pool && plain.mine.is_none());
        let pool =
            Args::try_parse_from(["xkrd", "--p2pool", "--mine-p2pool", "--mine-threads", "2"])
                .unwrap();
        assert!(pool.mine_p2pool && pool.p2pool_enabled());
        assert_eq!((pool.mine.as_deref(), pool.mine_threads), (None, Some(2)));
        let solo = Args::try_parse_from(["xkrd", "--p2pool", "--mine", "SEKRx"]).unwrap();
        assert!(!solo.mine_p2pool);
        assert_eq!(solo.mine.as_deref(), Some("SEKRx"));
        assert!(Args::try_parse_from(["xkrd", "--mine", "SEKRx", "--mine-p2pool"]).is_err());

        let help = Args::command().render_long_help().to_string();
        assert!(help.contains("--mine-p2pool"));
        assert!(help.contains("start_mining"));
        for flag in [
            "--p2pool",
            "--no-p2pool",
            "--p2pool-stratum",
            "--p2pool-mini",
        ] {
            assert!(help.contains(flag), "{flag}");
        }
    }
}
