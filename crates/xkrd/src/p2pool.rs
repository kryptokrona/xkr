//! The built-in p2pool (`xkrd --node --p2pool`, on mainnet also without mode flags;
//! `docs/mining.md` "Built-in p2pool"): the sidechain, block template, stratum server and P2P
//! server of `xkr-p2pool`, fed from the node's own chain instead of the daemon RPC the C++
//! p2pool polls (`p2pool::p2pool`, `src/p2pool.cpp`).
//!
//! * **Start**: once the node is synchronized (the P2P status: `synced` and not downloading, as
//!   the built-in miner and the Hugin relay wait), so templates are never built on an old tip.
//!   Until then `/status` `p2pool.state` is `waiting`.
//! * **Main chain** ([`MainChainSource`]): the node's own chain ([`LocalChain`]) or, without
//!   the node (`xkrd --p2pool`), remote nodes on the Hyperswarm clients topic
//!   ([`crate::remote::RemoteNodes`]). [`MainChainState`] is the C++ header cache
//!   (`MainChainCache`) with the headers of the PPLNS span and the seed heights, read from the
//!   source on every new tip (`MainChainSource::subscribe_tip`, a push, no polling); the tip
//!   from the source's miner data.
//! * **Templates**: on a new main-chain tip, a new sidechain tip or possible uncle, a change of
//!   the payout address and every [`TEMPLATE_REFRESH`] (pool transactions; C++ refreshes on the
//!   same schedule): miner data -> [`template_miner_data`] -> `PoolTemplates::update` with the
//!   payout wallet -> `StratumServer::on_block` (every miner gets the new job).
//! * **Payout**: the node's fee address ([`crate::fee`], shared with the Hugin relay); without
//!   one no template is built.
//! * **Built-in miner** (`start_mining p2pool`, `--mine-p2pool`): [`PoolLink`] hands it the same
//!   template and share path as a stratum miner of this node, in process; its shares pay the
//!   payout address too.
//! * **Found work**: an own share goes into the sidechain on the stratum thread and is
//!   broadcast; a main-chain block (own, or a peer's share that meets the main-chain difficulty)
//!   goes to the source on its own task (the node's chain: `xkr_rpc::submit_mined_block`, P2P
//!   manager -> chain thread -> relay; remote nodes: `submitblock` to each), so the stratum side
//!   never waits for the chain.
//! * **P2P**: outgoing TCP to the sidechain's seeds and peers, a TCP listener only with
//!   `--p2pool-p2p`, and every Hyperswarm connection on the sidechain topic
//!   ([`crate::hyper::p2pool_topic`], joined once p2pool runs). The peer list lives in
//!   `<data dir>/p2pool/p2pool_peers.txt`.
//! * **Output**: "Share found" for this node's shares and "Block found by p2pool at height N"
//!   on the terminal (shares of other miners only in the log), the `p2pool: …` log lines, the
//!   console's `status` and `/status` `p2pool` ([`P2poolStatus`]). A found block the chain does
//!   not take is logged at debug when it came too late (height taken, transactions gone, not on
//!   the tip), otherwise as one warning per minute with a count (`RefusalLog`).
//! * **Stop** (in the node's stopping steps, before the Hyperswarm transport): the stratum
//!   server, the P2P server (peer list written), the sidechain and templates.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use xkr_chain::{
    AddBlockOutcome, Chain, ChainError, MainChainHeader, MinerData, SubmitError, TipInfo, unix_now,
};
use xkr_consensus::{Network, NetworkParams};
use xkr_p2p_hyper::HyperConn;
use xkr_p2pool::mainchain::SEEDHASH_EPOCH_BLOCKS;
use xkr_p2pool::p2p::{
    Direction, P2pConfig, P2pEvent, P2pServer, P2pServerConfig, P2pStats, SharedMainChain,
    SharedSideChain,
};
use xkr_p2pool::stratum::{ShareSink, StratumServer, StratumServerConfig};
use xkr_p2pool::template::{
    FoundBlock, MempoolTx, PoolNode, PoolTemplates, Template, TemplateError, TemplateMinerData,
};
use xkr_p2pool::{
    ChainMain, Difficulty, ForkHeights, MainChainCache, MainChainTip, MainChainView, MinerWallet,
    SideChain, SidechainConfig, SidechainEvent, SidechainOptions, seed_height, short_id,
};
use xkr_rpc::{NetStatus, P2poolStatus, RpcCommand};
use xkr_types::Hash;

use crate::fee::{self, Fee};
use crate::term::{self, Color, Style, Term};

/// Subdirectory of the data directory with the peer list (and later the share cache).
pub const P2POOL_DIR: &str = "p2pool";

/// A template is rebuilt at least this often (new pool transactions, the timestamp).
pub const TEMPLATE_REFRESH: Duration = Duration::from_secs(20);

/// `/status` `p2pool` and the console are refreshed this often (hashrates, peers).
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

/// Main-chain heights read again below the last tip on every new tip (reorganisations).
const REREAD_DEPTH: u64 = 64;

/// How long the P2P server gets to close its connections and write the peer list.
const P2P_STOP: Duration = Duration::from_secs(5);

/// The environment variable of the testnet scenarios for [`P2poolSettings::uncapped_pplns`].
pub const UNCAPPED_PPLNS_ENV: &str = "XKR_TESTNET_UNCAPPED_PPLNS";

// ---------------------------------------------------------------------------------------------
// settings

/// How the built-in p2pool runs (`--p2pool-*`).
#[derive(Debug, Clone)]
pub struct P2poolSettings {
    /// The sidechain (`--p2pool-mini`, `--p2pool-nano`, `--p2pool-config`, else the default).
    pub sidechain: SidechainConfig,
    /// Its consensus id.
    pub consensus_id: Hash,
    /// No PPLNS weight cap (testnet only, `--p2pool-uncapped-pplns`).
    pub uncapped_pplns: bool,
    /// The stratum server (`--p2pool-stratum`, `--p2pool-stratum-ban-time`,
    /// `--p2pool-no-autodiff`).
    pub stratum: StratumServerConfig,
    /// Listen address and connection limits (`--p2pool-p2p`, `--p2pool-out-peers`,
    /// `--p2pool-in-peers`, `--p2pool-allow-same-ip`).
    pub p2p: P2pConfig,
    /// Peers to connect to at start (`--p2pool-addpeers`).
    pub add_peers: Vec<String>,
    /// `<data dir>/p2pool` (the peer list); `None` keeps it in memory.
    pub dir: Option<PathBuf>,
    /// Connect to the sidechain's built-in seed peers.
    pub seeds: bool,
    /// Why the stratum address could not be bound at start-up
    /// ([`P2poolSettings::check_stratum`]), e.g. `127.0.0.1:3333 in use; set --p2pool-stratum`.
    /// p2pool then runs without its stratum server unless the address is free by the time it
    /// starts.
    pub stratum_off: Option<String>,
}

impl P2poolSettings {
    /// `sidechain` with the C++ defaults: stratum on `127.0.0.1:3333`, outgoing P2P only, the
    /// built-in seeds, no data directory.
    #[must_use]
    pub fn new(sidechain: SidechainConfig) -> Self {
        let p2p = P2pConfig {
            external_port: sidechain.default_p2p_port(),
            ..P2pConfig::default()
        };
        Self {
            consensus_id: sidechain.consensus_id(),
            sidechain,
            uncapped_pplns: false,
            stratum: StratumServerConfig::default(),
            p2p,
            add_peers: Vec::new(),
            dir: None,
            seeds: true,
            stratum_off: None,
        }
    }

    /// Tries the stratum address now (bound and released at once), so a port that is in use
    /// is shown in the start-up step instead of when p2pool starts. Sets
    /// [`P2poolSettings::stratum_off`] and returns `false` when it cannot be bound.
    pub fn check_stratum(&mut self) -> bool {
        match std::net::TcpListener::bind(self.stratum.listen) {
            Ok(listener) => {
                drop(listener);
                self.stratum_off = None;
                true
            }
            Err(e) => {
                self.stratum_off = Some(stratum_off_reason(self.stratum.listen, &e));
                false
            }
        }
    }

    /// From the command line; the peer list goes to `<data_dir>/p2pool`.
    pub fn from_args(
        args: &crate::cli::Args,
        network: Network,
        data_dir: &Path,
    ) -> Result<Self, String> {
        let sidechain = match &args.p2pool_config {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("--p2pool-config {}: {e}", path.display()))?;
                SidechainConfig::from_json(&text)
                    .map_err(|e| format!("--p2pool-config {}: {e}", path.display()))?
            }
            None if args.p2pool_mini => SidechainConfig::mini(),
            None if args.p2pool_nano => SidechainConfig::nano(),
            None => SidechainConfig::main(),
        };
        if args.p2pool_uncapped_pplns && network != Network::Testnet {
            return Err("--p2pool-uncapped-pplns is only accepted with --network testnet".into());
        }
        let env_uncapped = std::env::var(UNCAPPED_PPLNS_ENV).is_ok_and(|v| v.trim() == "1");
        let mut s = Self::new(sidechain);
        s.uncapped_pplns =
            network == Network::Testnet && (args.p2pool_uncapped_pplns || env_uncapped);
        s.stratum = StratumServerConfig {
            listen: args.p2pool_stratum,
            ban_time: Duration::from_secs(args.p2pool_stratum_ban_time),
            auto_diff: !args.p2pool_no_autodiff,
            ..StratumServerConfig::default()
        };
        s.p2p = P2pConfig {
            listen: args.p2pool_p2p,
            external_port: announced_port(args.p2pool_p2p, &s.sidechain),
            out_peers: args.p2pool_out_peers,
            in_peers: args.p2pool_in_peers,
            allow_same_ip: args.p2pool_allow_same_ip,
            ..P2pConfig::default()
        };
        s.add_peers.clone_from(&args.p2pool_addpeers);
        if !args.p2pool_tcp_enabled() {
            // Hyperswarm only: no seeds, no dialing, no listener.
            s.seeds = false;
            s.add_peers.clear();
            s.p2p.out_peers = 0;
            s.p2p.listen = None;
        }
        s.dir = Some(data_dir.join(P2POOL_DIR));
        Ok(s)
    }

    /// The P2P server settings: seeds, the default port and the peer file of the sidechain.
    fn server_config(&self) -> P2pServerConfig {
        let mut cfg =
            P2pServerConfig::for_sidechain(self.p2p.clone(), &self.sidechain, self.dir.as_deref());
        if !self.seeds {
            cfg.seeds.clear();
        }
        cfg.add_peers.clone_from(&self.add_peers);
        cfg
    }
}

/// The port announced to peers (`LISTEN_PORT`): the listen port, else the sidechain's default
/// port (C++ peers need a value in 1..65535).
fn announced_port(listen: Option<SocketAddr>, sidechain: &SidechainConfig) -> u16 {
    listen
        .map(|a| a.port())
        .filter(|p| *p != 0)
        .unwrap_or_else(|| sidechain.default_p2p_port())
}

/// `default`, `mini`, `nano` or the name of a custom sidechain.
#[must_use]
pub fn sidechain_label(sidechain: &SidechainConfig) -> String {
    if sidechain.is_mini() {
        "mini".to_owned()
    } else if sidechain.is_nano() {
        "nano".to_owned()
    } else if sidechain.is_default() {
        "default".to_owned()
    } else {
        sidechain.name.clone()
    }
}

/// The p2pool nodes of the Hugin relay when p2pool runs in this process: its own stratum server
/// (on loopback when it listens on every interface), then the public nodes. The relay connects
/// there directly; the port belongs to this process, so the port-owner check of the default
/// list is not needed.
#[must_use]
pub fn relay_nodes(listen: SocketAddr) -> Vec<String> {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    std::iter::once(SocketAddr::new(ip, listen.port()).to_string())
        .chain(public_relay_nodes())
        .collect()
}

/// Why the stratum server is off: `127.0.0.1:3333 in use; set --p2pool-stratum`.
#[must_use]
pub fn stratum_off_reason(listen: SocketAddr, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        format!("{listen} in use; set --p2pool-stratum")
    } else {
        format!("{listen}: {e}; set --p2pool-stratum")
    }
}

/// `stratum: off (127.0.0.1:3333 in use; set --p2pool-stratum)`, "stratum: off" in yellow.
#[must_use]
pub fn stratum_off_line(style: Style, reason: &str) -> String {
    format!("{} ({reason})", style.paint(Color::Yellow, "stratum: off"))
}

/// The p2pool nodes of the Hugin relay when the built-in p2pool's stratum is off: the public
/// nodes only.
#[must_use]
pub fn public_relay_nodes() -> Vec<String> {
    xkr_hugin::p2pool::PUBLIC_P2POOL_NODES
        .iter()
        .copied()
        .map(str::to_owned)
        .collect()
}

/// The detail of the "p2pool" start-up step:
/// `sidechain default · stratum 127.0.0.1:3333 · payout nilsjr (donation)`.
#[must_use]
pub fn step_detail(settings: &P2poolSettings, fee: &Fee) -> String {
    let payout = match &fee.address {
        None => "no payout address".to_owned(),
        Some(_) if fee.donation => format!("payout {} (donation)", fee::DEFAULT_FEE_NAME),
        Some(a) => format!("payout {}", fee::short_address(a)),
    };
    let stratum = if settings.stratum_off.is_some() {
        "stratum off".to_owned()
    } else {
        format!("stratum {}", settings.stratum.listen)
    };
    format!(
        "sidechain {} · {stratum} · {payout}",
        sidechain_label(&settings.sidechain)
    )
}

// ---------------------------------------------------------------------------------------------
// the main chain

/// The C++ main-chain header cache behind a lock: the P2P server reads it for every share it
/// checks, the template reads it under the sidechain lock, the tip feed writes it.
#[derive(Debug)]
pub struct MainChainState(RwLock<MainChainCache>);

impl MainChainState {
    /// An empty cache for `sidechain` (its PPLNS span of headers).
    #[must_use]
    pub fn new(sidechain: &SidechainConfig) -> Self {
        Self(RwLock::new(MainChainCache::new(
            sidechain.main_chain_headers_required(),
        )))
    }

    fn read(&self) -> RwLockReadGuard<'_, MainChainCache> {
        self.0.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, MainChainCache> {
        self.0.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl MainChainView for MainChainState {
    fn block_at_height(&self, height: u64) -> Option<ChainMain> {
        self.read().block_at_height(height)
    }

    fn block_by_hash(&self, id: &Hash) -> Option<ChainMain> {
        self.read().block_by_hash(id)
    }

    fn tip(&self) -> MainChainTip {
        self.read().tip()
    }
}

/// A chain header as the sidechain sees it (`ChainMain`; the reward is not used by the
/// sidechain and stays 0).
fn chain_main(h: &MainChainHeader) -> ChainMain {
    ChainMain {
        difficulty: Difficulty::from_u64(h.difficulty),
        height: h.height,
        timestamp: h.timestamp,
        reward: 0,
        id: h.hash,
    }
}

/// The seed heights `MainChainCache::cleanup` keeps for the next height `height`: the seed
/// height of `height` and every earlier one down to that of `height - required`.
fn seed_heights(height: u64, required: u64) -> Vec<u64> {
    let mut seed = seed_height(height);
    let oldest = if height > required {
        seed_height(height - required)
    } else {
        0
    };
    let mut out = vec![seed];
    while seed > oldest.max(SEEDHASH_EPOCH_BLOCKS - 1) {
        seed -= SEEDHASH_EPOCH_BLOCKS;
        out.push(seed);
    }
    out
}

/// C++ `handle_miner_data` and the header requests around it: the header ranges to read for
/// `data`, `[from, next)` with `from = max(next - required, read_to - REREAD_DEPTH)`, then one
/// range per seed height below `from` that the cache does not hold yet.
#[must_use]
pub(crate) fn header_ranges(
    main: &MainChainState,
    data: &MinerData,
    read_to: Option<u64>,
) -> Vec<(u64, u64)> {
    let next = data.height;
    let top = next.saturating_sub(1);
    let cache = main.read();
    let required = cache.headers_required();
    let start = next.saturating_sub(required);
    let from = read_to
        .map_or(start, |r| r.min(top).saturating_sub(REREAD_DEPTH))
        .max(start);
    let mut ranges = vec![(from, next)];
    ranges.extend(
        seed_heights(next, required)
            .into_iter()
            .filter(|s| *s < from && cache.block_at_height(*s).is_none_or(|c| c.id == Hash::ZERO))
            .map(|s| (s, s + 1)),
    );
    ranges
}

/// Puts the headers read for [`header_ranges`] into the cache and sets the tip of `data`.
/// Returns the top block index read.
#[must_use]
pub(crate) fn apply_headers(
    main: &MainChainState,
    data: &MinerData,
    headers: &[MainChainHeader],
) -> u64 {
    let next = data.height;
    let mut cache = main.write();
    for h in headers {
        cache.insert_header(chain_main(h));
    }
    cache.set_tip(MainChainTip {
        height: next,
        prev_id: data.prev_id,
        difficulty: Difficulty::from_u64(data.difficulty),
    });
    next.saturating_sub(1)
}

/// [`header_ranges`] read from the node's chain, then [`apply_headers`].
fn refresh_main_chain(
    chain: &Chain,
    main: &MainChainState,
    data: &MinerData,
    read_to: Option<u64>,
) -> Result<u64, ChainError> {
    let mut headers = Vec::new();
    for (start, end) in header_ranges(main, data, read_to) {
        headers.extend(chain.main_chain_headers(start, end)?);
    }
    Ok(apply_headers(main, data, &headers))
}

// ---------------------------------------------------------------------------------------------
// the source of the main chain

/// A main-chain block that a source took onto its main chain: its index and id.
pub type BlockOnChain = (u64, Hash);

/// Where p2pool gets the main chain from and where its found blocks go: the node's own chain
/// ([`LocalChain`]) or remote nodes ([`crate::remote::RemoteNodes`]).
pub trait MainChainSource: Send + Sync + fmt::Debug + 'static {
    /// The network's parameters (fork heights, unlock window, template versions).
    fn params(&self) -> &'static NetworkParams;

    /// A receiver that sees every new main-chain tip.
    fn subscribe_tip(&self) -> watch::Receiver<TipInfo>;

    /// The miner data of the next block and the headers p2pool needs around it, written into
    /// `main` ([`header_ranges`], [`apply_headers`]); returns the data and the top block index
    /// read.
    fn refresh(
        &self,
        main: Arc<MainChainState>,
        read_to: Option<u64>,
    ) -> BoxFuture<'static, Result<(MinerData, u64), String>>;

    /// Submits a found main-chain block (the `submitblock` blob): `Ok` with its index and id
    /// once it is on the main chain, else why not.
    fn submit_block(&self, blob: Vec<u8>) -> BoxFuture<'static, Result<BlockOnChain, Refusal>>;
}

/// The node's own chain: storage reads on blocking threads, found blocks the `submitblock` way
/// (P2P manager -> chain thread -> relay).
#[derive(Debug, Clone)]
pub struct LocalChain {
    chain: Arc<Chain>,
    commands: mpsc::UnboundedSender<RpcCommand>,
}

impl LocalChain {
    /// The chain, and the P2P manager's command channel for found blocks.
    #[must_use]
    pub fn new(chain: Arc<Chain>, commands: mpsc::UnboundedSender<RpcCommand>) -> Arc<Self> {
        Arc::new(Self { chain, commands })
    }
}

impl MainChainSource for LocalChain {
    fn params(&self) -> &'static NetworkParams {
        self.chain.params()
    }

    fn subscribe_tip(&self) -> watch::Receiver<TipInfo> {
        self.chain.subscribe_tip()
    }

    fn refresh(
        &self,
        main: Arc<MainChainState>,
        read_to: Option<u64>,
    ) -> BoxFuture<'static, Result<(MinerData, u64), String>> {
        let chain = self.chain.clone();
        async move {
            tokio::task::spawn_blocking(move || -> Result<(MinerData, u64), String> {
                let data = chain.miner_data().map_err(|e| e.to_string())?;
                let top =
                    refresh_main_chain(&chain, &main, &data, read_to).map_err(|e| e.to_string())?;
                Ok((data, top))
            })
            .await
            .map_err(|e| format!("the main-chain read failed: {e}"))?
        }
        .boxed()
    }

    fn submit_block(&self, blob: Vec<u8>) -> BoxFuture<'static, Result<BlockOnChain, Refusal>> {
        let (chain, commands) = (self.chain.clone(), self.commands.clone());
        async move {
            let raw = chain
                .assemble_mined_block(blob)
                .map_err(|e| submit_refusal(&e))?;
            match xkr_rpc::submit_mined_block(&chain, &commands, raw).await {
                Ok(r) => match outcome_refusal(&r.outcome) {
                    None => Ok((r.height.unwrap_or_else(|| chain.tip().height), r.hash)),
                    Some(refusal) => Err(refusal),
                },
                Err(e) => Err(Refusal {
                    stale: false,
                    reason: format!("not submitted: {e}"),
                }),
            }
        }
        .boxed()
    }
}

/// The template's view of the node's miner data (C++ `MinerData` from `get_miner_data`): the
/// fields as they are, the difficulty as a 128-bit value, the block minor version the node
/// writes into its own templates (`ForkSchedule::template_minor_version`), and the backlog
/// (`Chain::template_backlog`: what the node's own template would take) as `TxMempoolData`.
#[must_use]
pub fn template_miner_data(data: &MinerData, params: &NetworkParams) -> TemplateMinerData {
    TemplateMinerData {
        major_version: data.major_version,
        minor_version: params.forks().template_minor_version(data.major_version),
        height: data.height,
        prev_id: data.prev_id,
        difficulty: Difficulty::from_u64(data.difficulty),
        median_weight: data.median_weight,
        already_generated_coins: data.already_generated_coins,
        median_timestamp: data.median_timestamp,
        tx_backlog: data
            .tx_backlog
            .iter()
            .map(|t| MempoolTx {
                id: t.hash,
                weight: t.size as u64,
                fee: t.fee,
                time_received: t.received,
            })
            .collect(),
    }
}

/// The payout wallet of a fee address, `None` without one.
fn payout_wallet(fee: &Fee) -> Option<MinerWallet> {
    fee.address.as_deref().and_then(MinerWallet::parse)
}

/// Whether the node is synchronized enough for templates: what the built-in miner and the
/// Hugin relay wait for (`synced` and not downloading).
fn synced_now(net: &NetStatus) -> bool {
    net.synced && !net.downloading
}

/// A difficulty for `/status` (saturated at `u64::MAX`).
pub const fn difficulty_u64(d: Difficulty) -> u64 {
    if d.hi() == 0 { d.lo() } else { u64::MAX }
}

// ---------------------------------------------------------------------------------------------
// terminal lines

/// `Share found · height 29 277 · diff 150 800 000` (the sidechain height).
#[must_use]
pub fn share_found_line(style: Style, height: u64, difficulty: u64) -> String {
    format!(
        "{} · height {} · diff {}",
        style.paint(Color::Green, "Share found"),
        style.paint(Color::Bold, &term::group(height)),
        term::group(difficulty)
    )
}

/// `Block found by p2pool at height 2 620 300`.
#[must_use]
pub fn block_found_line(style: Style, height: u64) -> String {
    format!(
        "{} {}",
        style.paint(Color::Green, "Block found by p2pool at height"),
        style.paint(Color::Bold, &term::group(height))
    )
}

/// `p2pool running · stratum 127.0.0.1:3333`.
#[must_use]
pub fn running_line(style: Style, stratum: SocketAddr) -> String {
    format!(
        "{} · stratum {stratum}",
        style.paint(Color::Green, "p2pool running")
    )
}

/// `p2pool running · stratum off`, "stratum off" in yellow (the reason was shown already, or
/// follows on its own line).
#[must_use]
pub fn running_line_stratum_off(style: Style) -> String {
    format!(
        "{} · {}",
        style.paint(Color::Green, "p2pool running"),
        style.paint(Color::Yellow, "stratum off")
    )
}

// ---------------------------------------------------------------------------------------------
// refused found blocks

/// At most one "found blocks refused" warning per this many seconds.
const REFUSAL_LOG_PERIOD: u64 = 60;

/// Why the chain did not take a submitted p2pool block (C++ `p2pool::submit_block`,
/// `src/p2pool.cpp`, logs the daemon's error for each one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The block came too late: its height is taken, its transactions have left the pool or
    /// it does not build on the main-chain tip. Logged at debug.
    pub stale: bool,
    /// What the chain said.
    pub reason: String,
}

/// A block that could not be assembled: a listed transaction no longer in the pool is stale.
fn submit_refusal(e: &SubmitError) -> Refusal {
    Refusal {
        stale: matches!(e, SubmitError::TransactionAbsent(_)),
        reason: e.to_string(),
    }
}

/// The chain's answer to a submitted block; `None` when it reached the main chain.
fn outcome_refusal(outcome: &AddBlockOutcome) -> Option<Refusal> {
    let (stale, reason) = match outcome {
        AddBlockOutcome::AddedToMain | AddBlockOutcome::AddedAndSwitched => return None,
        AddBlockOutcome::AlreadyExists => (true, "block already known".to_owned()),
        AddBlockOutcome::Orphan => (true, "parent block unknown".to_owned()),
        AddBlockOutcome::AddedToAlternative => (
            true,
            "not on the main-chain tip (alternative block)".to_owned(),
        ),
        AddBlockOutcome::Invalid(r) => (false, r.to_string()),
    };
    Some(Refusal { stale, reason })
}

/// Counts refusals that are not stale and says them at most once per [`REFUSAL_LOG_PERIOD`].
#[derive(Debug, Default)]
struct RefusalLog {
    /// When the last warning was said (seconds since the epoch).
    said_at: Option<u64>,
    /// Refusals since then.
    count: u64,
}

impl RefusalLog {
    /// One refusal at `now`: the warning line when the period since the last one is over,
    /// counting every refusal since that one.
    fn note(&mut self, now: u64, reason: &str) -> Option<String> {
        self.count += 1;
        if self
            .said_at
            .is_some_and(|t| now < t.saturating_add(REFUSAL_LOG_PERIOD))
        {
            return None;
        }
        self.said_at = Some(now);
        let n = std::mem::take(&mut self.count);
        let blocks = if n == 1 { "block" } else { "blocks" };
        Some(format!(
            "p2pool: {n} found {blocks} refused in the last minute: {reason}"
        ))
    }
}

/// A stale refusal at debug; any other as a warning, at most one line per minute.
fn log_refusal(log: &Mutex<RefusalLog>, refusal: &Refusal, now: u64) {
    if refusal.stale {
        tracing::debug!(reason = %refusal.reason, "p2pool: stale found block not taken");
        return;
    }
    let line = log
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .note(now, &refusal.reason);
    if let Some(line) = line {
        tracing::warn!("{line}");
    }
}

// ---------------------------------------------------------------------------------------------
// the service

/// The built-in miner's way into the p2pool service (`start_mining p2pool`,
/// `--mine-p2pool`): the node's own templates and share path while p2pool runs. Shared between
/// the service, which sets it, and the miner, which reads it.
#[derive(Debug, Clone, Default)]
pub struct PoolLink(Arc<RwLock<LinkState>>);

/// Where the p2pool service is, as the miner sees it.
#[derive(Debug, Clone, Default)]
pub enum LinkState {
    /// Not started yet (the node is synchronizing).
    #[default]
    Waiting,
    /// Running: templates and the share path.
    Running(PoolAccess),
    /// Failed to start or stopped (the reason).
    Down(String),
}

impl PoolLink {
    /// The current state.
    #[must_use]
    pub fn state(&self) -> LinkState {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// p2pool will not run (it could not be started): a waiting miner stops waiting.
    pub fn fail(&self, reason: String) {
        self.set(LinkState::Down(reason));
    }

    fn set(&self, state: LinkState) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = state;
    }
}

/// The running p2pool's templates and share path, as a stratum miner of this node uses them
/// (no TCP): the node's current template, paid to the node's p2pool payout address, and the
/// stratum server's [`ShareSink`] (share into the sidechain and broadcast, "Share found" line,
/// main-chain block to the node).
#[derive(Clone)]
pub struct PoolAccess {
    templates: Arc<PoolTemplates>,
    sink: Arc<Sink>,
}

impl fmt::Debug for PoolAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolAccess")
            .field("templates", &self.templates)
            .finish_non_exhaustive()
    }
}

impl PoolAccess {
    /// The node's current template (`None` before the first one, e.g. without a payout
    /// address).
    #[must_use]
    pub fn current(&self) -> Option<Arc<Template>> {
        self.templates.current()
    }

    /// A result at the main-chain difficulty: the block goes to the node (on the service's
    /// task, does not block).
    pub fn submit_block(&self, template_id: u32, nonce: u32, extra_nonce: u32) {
        self.sink.submit_block(template_id, nonce, extra_nonce);
    }

    /// A result at the sidechain difficulty: the share goes into the sidechain and to the
    /// peers; `false` when the sidechain did not take it. Locks the sidechain briefly.
    #[must_use]
    pub fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool {
        self.sink.submit_share(template_id, nonce, extra_nonce)
    }
}

/// What the service needs from the node.
#[derive(Debug)]
pub struct P2poolNode {
    /// The main chain: miner data, headers, found blocks.
    pub source: Arc<dyn MainChainSource>,
    /// The sync status (the node's P2P status, or the remote nodes' as one): p2pool starts and
    /// builds templates only while synchronized.
    pub net: watch::Receiver<Arc<NetStatus>>,
    /// The fee address in effect (`FeeAddress::subscribe`): the payout wallet.
    pub fee: watch::Receiver<Fee>,
    /// Where the status goes (`/status` `p2pool`, the console).
    pub status: watch::Sender<Arc<P2poolStatus>>,
    /// The sidechain topic on the Hyperswarm network, with the Hyperswarm transport.
    pub hyper: Option<HyperLink>,
    /// The terminal for the "Share found" / "Block found" lines (`None` with `--log-json`).
    pub term: Option<Term>,
    /// Set while p2pool runs, for the built-in miner.
    pub link: PoolLink,
}

/// The Hyperswarm side of the sidechain (`crate::hyper::P2poolTopic` on the other end).
#[derive(Debug)]
pub struct HyperLink {
    /// Connections on the sidechain topic.
    pub conns: mpsc::UnboundedReceiver<HyperConn>,
    /// Set once p2pool runs: the topic is announced from then on.
    pub ready: watch::Sender<bool>,
}

/// Runs the built-in p2pool until `stop`: waits for the node to synchronize, starts, serves,
/// and stops in order (stratum, P2P with the peer list, sidechain) with one stopping step each.
pub async fn run(settings: P2poolSettings, node: P2poolNode, stop: CancellationToken) {
    let P2poolNode {
        source,
        mut net,
        fee,
        status,
        hyper,
        term,
        link,
    } = node;
    let base = base_status(&settings, &fee.borrow());
    status.send_replace(Arc::new(base.clone()));
    tracing::info!(
        "p2pool: consensus id {} (sidechain {})",
        short_id(&settings.consensus_id),
        sidechain_label(&settings.sidechain)
    );
    if settings.uncapped_pplns {
        tracing::info!("p2pool: testnet sidechain, PPLNS weight not capped");
    }

    // Wait for the node to be synchronized (the payout shown meanwhile follows the fee address).
    let mut fee_changes = fee.clone();
    let mut fee_open = true;
    while !synced_now(&net.borrow_and_update()) {
        tokio::select! {
            () = stop.cancelled() => {
                status.send_modify(|s| Arc::make_mut(s).state = "stopped".into());
                link.set(LinkState::Down("p2pool stopped".into()));
                return;
            }
            r = net.changed() => {
                if r.is_err() {
                    link.set(LinkState::Down("p2pool stopped".into()));
                    return;
                }
            }
            r = fee_changes.changed(), if fee_open => {
                fee_open = r.is_ok();
                let f = fee_changes.borrow_and_update().clone();
                status.send_modify(|s| {
                    let s = Arc::make_mut(s);
                    s.payout_address = f.address;
                    s.payout_donation = f.donation;
                });
            }
        }
    }
    tracing::info!("p2pool: the node is synchronized, starting");

    let (hyper_conns, hyper_ready) = match hyper {
        Some(h) => (Some(h.conns), Some(h.ready)),
        None => (None, None),
    };
    let parts = Parts {
        source,
        net,
        fee,
        status,
        term,
        link: link.clone(),
    };
    let (mut running, channels) = match Running::start(&settings, parts).await {
        Ok(r) => r,
        Err((e, status)) => {
            tracing::error!(error = %e, "p2pool could not start");
            link.set(LinkState::Down(format!("p2pool failed: {e}")));
            status.send_replace(Arc::new(P2poolStatus {
                state: "failed".into(),
                error: Some(e),
                ..base
            }));
            return;
        }
    };
    if let Some(ready) = &hyper_ready {
        ready.send_replace(true);
    }
    running.serve(&stop, channels, hyper_conns).await;
    if let Some(ready) = &hyper_ready {
        ready.send_replace(false);
    }
    running.stop().await;
}

/// The status before p2pool runs.
fn base_status(settings: &P2poolSettings, fee: &Fee) -> P2poolStatus {
    P2poolStatus {
        state: "waiting".into(),
        sidechain: sidechain_label(&settings.sidechain),
        consensus_id: hex::encode(settings.consensus_id.0),
        difficulty: difficulty_u64(settings.sidechain.min_difficulty()),
        payout_address: fee.address.clone(),
        payout_donation: fee.donation,
        stratum: settings
            .stratum_off
            .is_none()
            .then(|| settings.stratum.listen.to_string()),
        stratum_off: settings.stratum_off.clone(),
        ..P2poolStatus::default()
    }
}

/// The node's side of a running service, before it started.
struct Parts {
    source: Arc<dyn MainChainSource>,
    net: watch::Receiver<Arc<NetStatus>>,
    fee: watch::Receiver<Fee>,
    status: watch::Sender<Arc<P2poolStatus>>,
    term: Option<Term>,
    link: PoolLink,
}

/// What the stratum thread and the sidechain hand to the service loop.
#[derive(Debug)]
enum Cmd {
    /// Sidechain events after an own share: broadcast it, rebuild the template.
    Sidechain(Vec<SidechainEvent>),
    /// A main-chain block from a template: submit it.
    SubmitBlock(FoundBlock),
    /// An own share is in the sidechain.
    ShareFound { template_id: u32, id: Hash },
    /// A block found by p2pool is on the main chain now (`height`: its index).
    BlockAdded { height: u64, id: Hash },
}

/// The node as the template sees it ([`PoolNode`]).
struct Node {
    sidechain: SharedSideChain,
    main: Arc<MainChainState>,
    cmds: mpsc::UnboundedSender<Cmd>,
    p2p_stats: OnceLock<watch::Receiver<P2pStats>>,
    had_peers: AtomicBool,
}

/// Locks the sidechain, taking over a poisoned lock.
fn lock(sc: &SharedSideChain) -> MutexGuard<'_, SideChain> {
    sc.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PoolNode for Node {
    fn read_sidechain(&self, f: &mut dyn FnMut(&SideChain, &dyn MainChainView)) {
        let sc = lock(&self.sidechain);
        let main = self.main.read();
        f(&sc, &*main);
    }

    fn write_sidechain(&self, f: &mut dyn FnMut(&mut SideChain, &dyn MainChainView)) {
        let mut sc = lock(&self.sidechain);
        let main = self.main.read();
        f(&mut sc, &*main);
    }

    fn on_sidechain_events(&self, events: Vec<SidechainEvent>) {
        let _ = self.cmds.send(Cmd::Sidechain(events));
    }

    fn submit_mainchain_block(&self, block: FoundBlock) {
        let _ = self.cmds.send(Cmd::SubmitBlock(block));
    }

    /// C++ `P2PServer::disconnected`: there were good peers and there are none now.
    fn disconnected(&self) -> bool {
        let Some(stats) = self.p2p_stats.get() else {
            return false;
        };
        if stats.borrow().peers_good > 0 {
            self.had_peers.store(true, Ordering::Relaxed);
            return false;
        }
        self.had_peers.load(Ordering::Relaxed)
    }
}

/// Where the stratum server hands found work: the template, plus a note to the service loop
/// for the "Share found" line.
struct Sink {
    templates: Arc<PoolTemplates>,
    cmds: mpsc::UnboundedSender<Cmd>,
}

impl ShareSink for Sink {
    fn submit_block(&self, template_id: u32, nonce: u32, extra_nonce: u32) {
        ShareSink::submit_block(&*self.templates, template_id, nonce, extra_nonce);
    }

    fn submit_share(&self, template_id: u32, nonce: u32, extra_nonce: u32) -> bool {
        match self
            .templates
            .submit_share_at(template_id, nonce, extra_nonce, unix_now())
        {
            Ok(id) => {
                let _ = self.cmds.send(Cmd::ShareFound { template_id, id });
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, "p2pool: own share not added");
                false
            }
        }
    }
}

/// The receivers the service loop owns.
struct Channels {
    p2p: mpsc::UnboundedReceiver<P2pEvent>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
}

/// What a template rebuild did (on a blocking thread).
struct Refreshed {
    top: u64,
    template: Option<Result<Arc<Template>, TemplateError>>,
}

/// The running p2pool.
struct Running {
    parts: Parts,
    sidechain: SharedSideChain,
    main: Arc<MainChainState>,
    templates: Arc<PoolTemplates>,
    /// `None`: its address could not be bound (`base.stratum_off` says why); p2pool runs
    /// without it (sidechain and P2P only).
    stratum: Option<StratumServer>,
    p2p: P2pServer,
    keeps_peer_file: bool,
    cmds: mpsc::UnboundedSender<Cmd>,
    base: P2poolStatus,
    block_time: u64,
    unlock_window: u64,
    /// Main-chain top index of the last header read.
    read_to: Option<u64>,
    shares_found: u64,
    blocks_found: u64,
    /// The last template error said (said again only when it changes).
    template_error: Option<String>,
    /// "No payout address" was said.
    no_payout_said: bool,
    /// Rate limit of the "found blocks refused" warnings, shared with the submit tasks.
    refusals: Arc<Mutex<RefusalLog>>,
}

impl Running {
    /// Builds the sidechain and the main-chain cache, binds the stratum server, starts the P2P
    /// server and builds the first template. A stratum address that cannot be bound does not
    /// stop it: p2pool runs without the stratum server. On failure the status sender comes
    /// back with the error.
    async fn start(
        settings: &P2poolSettings,
        parts: Parts,
    ) -> Result<(Self, Channels), (String, watch::Sender<Arc<P2poolStatus>>)> {
        let params = parts.source.params();
        let unlock_window = u64::from(params.mined_money_unlock_window);
        let options = SidechainOptions {
            uncapped_pplns: settings.uncapped_pplns,
            ..SidechainOptions::new(
                settings.sidechain.clone(),
                ForkHeights::from_params(params),
                unlock_window,
            )
        };
        let sidechain: SharedSideChain = Arc::new(Mutex::new(SideChain::new(options)));
        let main = Arc::new(MainChainState::new(&settings.sidechain));
        let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
        let node = Arc::new(Node {
            sidechain: sidechain.clone(),
            main: main.clone(),
            cmds: cmds_tx.clone(),
            p2p_stats: OnceLock::new(),
            had_peers: AtomicBool::new(false),
        });
        let templates = Arc::new(PoolTemplates::new(node.clone()));
        let sink = Arc::new(Sink {
            templates: templates.clone(),
            cmds: cmds_tx.clone(),
        });
        let access = PoolAccess {
            templates: templates.clone(),
            sink: sink.clone(),
        };
        if let Some(dir) = &settings.dir
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return Err((format!("{}: {e}", dir.display()), parts.status));
        }
        let listen = settings.stratum.listen;
        let (stratum, stratum_off) =
            match StratumServer::bind(settings.stratum, templates.clone(), sink).await {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(stratum_off_reason(listen, &e))),
            };
        let stratum_addr = stratum.as_ref().map(StratumServer::local_addr);
        // The main-chain view is filled before the P2P server starts, so the first peers' tips
        // are checked against the node's real chain.
        let read_to = parts
            .source
            .refresh(main.clone(), None)
            .await
            .ok()
            .map(|(_, top)| top);
        let shared_main: SharedMainChain = main.clone();
        let (p2p, p2p_events) = match P2pServer::start(
            settings.server_config(),
            sidechain.clone(),
            shared_main,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                if let Some(stratum) = &stratum {
                    stratum.shutdown();
                }
                let at = settings
                    .p2p
                    .listen
                    .map_or_else(|| "P2P".to_owned(), |a| format!("P2P {a}"));
                return Err((format!("{at}: {e}"), parts.status));
            }
        };
        let _ = node.p2p_stats.set(p2p.subscribe_stats());
        let base = P2poolStatus {
            state: "running".into(),
            stratum: stratum_addr.map(|a| a.to_string()),
            stratum_off: stratum_off.clone(),
            ..base_status(settings, &parts.fee.borrow())
        };
        let mut running = Self {
            parts,
            sidechain,
            main,
            templates,
            stratum,
            p2p,
            keeps_peer_file: settings.dir.is_some(),
            cmds: cmds_tx,
            base,
            block_time: settings.sidechain.block_time,
            unlock_window,
            read_to,
            shares_found: 0,
            blocks_found: 0,
            template_error: None,
            no_payout_said: false,
            refusals: Arc::new(Mutex::new(RefusalLog::default())),
        };
        running.rebuild().await;
        running.publish();
        // The built-in miner mines on the same templates from now on.
        running.parts.link.set(LinkState::Running(access));
        tracing::info!(
            stratum = ?stratum_addr,
            p2p = ?settings.p2p.listen,
            "p2pool: running"
        );
        let style = running.style();
        match (stratum_addr, &stratum_off) {
            (Some(addr), _) => running.say(&running_line(style, addr)),
            (None, Some(reason)) => {
                running.say(&running_line_stratum_off(style));
                // Shown once: the start-up step already said it when the address was in use
                // then too.
                if settings.stratum_off.as_deref() != Some(reason.as_str()) {
                    running.say(&format!("  {}", stratum_off_line(style, reason)));
                }
                running.stratum_off_logged(reason);
            }
            (None, None) => {}
        }
        Ok((
            running,
            Channels {
                p2p: p2p_events,
                cmds: cmds_rx,
            },
        ))
    }

    fn style(&self) -> Style {
        self.parts
            .term
            .as_ref()
            .map_or_else(|| Style::new(false), Term::style)
    }

    /// A line on the terminal (not with `--log-json`).
    fn say(&self, line: &str) {
        if let Some(t) = &self.parts.term {
            t.print(line);
        }
    }

    /// The log event for a stratum server that is off: a warning for `--log-json` (the
    /// desktop app), else info (the terminal shows it as a line, not as a log line).
    fn stratum_off_logged(&self, reason: &str) {
        if self.parts.term.is_none() {
            tracing::warn!(reason, "p2pool: stratum off, running without it");
        } else {
            tracing::info!(reason, "p2pool: stratum off, running without it");
        }
    }

    /// The service loop: new main-chain tips, payout changes, P2P events, found work,
    /// Hyperswarm connections and the refresh timers, until `stop`.
    async fn serve(
        &mut self,
        stop: &CancellationToken,
        mut channels: Channels,
        mut hyper: Option<mpsc::UnboundedReceiver<HyperConn>>,
    ) {
        let mut tip = self.parts.source.subscribe_tip();
        let mut fee_changes = self.parts.fee.clone();
        let (mut tip_open, mut fee_open) = (true, true);
        let mut refresh = tokio::time::interval(TEMPLATE_REFRESH);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        refresh.reset();
        let mut status_tick = tokio::time::interval(STATUS_INTERVAL);
        status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let rebuild = tokio::select! {
                () = stop.cancelled() => break,
                r = tip.changed(), if tip_open => {
                    tip_open = r.is_ok();
                    tip_open
                }
                r = fee_changes.changed(), if fee_open => {
                    fee_open = r.is_ok();
                    if fee_open {
                        self.payout_changed();
                    }
                    fee_open
                }
                Some(ev) = channels.p2p.recv() => self.on_p2p_event(ev),
                Some(cmd) = channels.cmds.recv() => self.on_cmd(cmd),
                Some(conn) = next_conn(&mut hyper) => {
                    self.attach(conn);
                    false
                }
                _ = refresh.tick() => true,
                _ = status_tick.tick() => false,
            };
            if rebuild {
                self.rebuild().await;
            }
            self.publish();
        }
    }

    /// A new payout address: the next template pays it, the share counters start again
    /// (C++ `reset_share_counters`).
    fn payout_changed(&mut self) {
        let payout = self.parts.fee.borrow().clone();
        let shown = payout
            .address
            .as_deref()
            .map_or_else(|| "none".to_owned(), fee::short_address);
        tracing::info!(
            payout = %shown,
            donation = payout.donation,
            "p2pool: payout address changed"
        );
        if let Some(stratum) = &self.stratum {
            stratum.reset_share_counters();
        }
        self.no_payout_said = false;
    }

    /// Reads the new main-chain headers and builds a template for the payout wallet (on a
    /// blocking thread), then gives every miner a job. The current template stays when no new
    /// one can be built.
    async fn rebuild(&mut self) {
        if !synced_now(&self.parts.net.borrow()) {
            return;
        }
        let wallet = payout_wallet(&self.parts.fee.borrow());
        let source = self.parts.source.clone();
        let params = source.params();
        let main = self.main.clone();
        let templates = self.templates.clone();
        let read_to = self.read_to;
        let (data, top) = match source.refresh(main, read_to).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "p2pool: the main chain could not be read");
                return;
            }
        };
        let job = tokio::task::spawn_blocking(move || Refreshed {
            top,
            template: wallet
                .map(|w| templates.update(&template_miner_data(&data, params), &w, unix_now())),
        })
        .await;
        let refreshed = match job {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "p2pool: the template task failed");
                return;
            }
        };
        self.read_to = Some(refreshed.top);
        match refreshed.template {
            Some(Ok(t)) => {
                self.template_error = None;
                if let Some(stratum) = &self.stratum {
                    stratum.on_block();
                }
                tracing::debug!(
                    height = t.height(),
                    sidechain_height = t.sidechain_height(),
                    difficulty = %t.block.difficulty,
                    reward = t.reward,
                    txs = t.block.transactions.len().saturating_sub(1),
                    "p2pool: new block template"
                );
            }
            Some(Err(e)) => {
                let text = e.to_string();
                if self.template_error.as_deref() != Some(text.as_str()) {
                    tracing::warn!(error = %text, "p2pool: no new block template");
                    self.template_error = Some(text);
                }
            }
            None => {
                if !self.no_payout_said {
                    tracing::warn!("p2pool: no payout address, no block templates");
                    self.no_payout_said = true;
                }
            }
        }
    }

    /// One event of the P2P server; `true`: rebuild the template.
    fn on_p2p_event(&self, ev: P2pEvent) -> bool {
        match ev {
            P2pEvent::Sidechain(SidechainEvent::NewTip {
                height,
                next_difficulty,
                ..
            }) => {
                tracing::info!("p2pool: new tip height={height} diff={next_difficulty}");
                true
            }
            P2pEvent::Sidechain(SidechainEvent::PossibleUncle(_)) => true,
            P2pEvent::Sidechain(_) => false,
            P2pEvent::ShareAdded { id, height, from } => {
                let from = from.map_or_else(|| "?".to_owned(), |p| p.to_string());
                tracing::info!("p2pool: share added height={height} id={id} from={from}");
                false
            }
            P2pEvent::SubmitToDaemon(b) => {
                let blob = b
                    .serialize_mainchain_data(self.unlock_window, b.nonce, b.extra_nonce)
                    .bytes;
                self.spawn_submit(blob);
                false
            }
            P2pEvent::BlockFound { id, main } => {
                tracing::info!(
                    "p2pool: block found height={} id={id} (share of a peer)",
                    main.height
                );
                false
            }
            P2pEvent::PeerRejected { peer, reason } => {
                tracing::info!("p2pool: peer {peer} rejected: {reason}");
                false
            }
            P2pEvent::Synchronized => {
                tracing::info!("p2pool: synchronized");
                true
            }
        }
    }

    /// One note from the stratum side or a submission; `true`: rebuild the template.
    fn on_cmd(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Sidechain(events) => {
                let mut rebuild = false;
                for ev in events {
                    match ev {
                        SidechainEvent::Broadcast(id) => self.p2p.broadcast(id),
                        SidechainEvent::NewTip { .. } | SidechainEvent::PossibleUncle(_) => {
                            rebuild = true;
                        }
                        _ => {}
                    }
                }
                self.p2p.sidechain_changed();
                rebuild
            }
            Cmd::SubmitBlock(found) => {
                tracing::info!(
                    height = found.height,
                    sidechain_id = %found.sidechain_id,
                    "p2pool: main-chain block found by a local miner, submitting"
                );
                self.spawn_submit(found.blob);
                false
            }
            Cmd::ShareFound { template_id, id } => {
                self.shares_found += 1;
                let (height, difficulty) =
                    self.templates.template(template_id).map_or((0, 0), |t| {
                        (t.sidechain_height(), difficulty_u64(t.block.difficulty))
                    });
                tracing::info!("p2pool: share found height={height} diff={difficulty} id={id}");
                self.say(&share_found_line(self.style(), height, difficulty));
                false
            }
            Cmd::BlockAdded { height, id } => {
                self.blocks_found += 1;
                tracing::info!("p2pool: block found height={height} id={id}");
                self.say(&block_found_line(self.style(), height));
                false
            }
        }
    }

    /// Submits a found main-chain block on its own task (the `submitblock` path); a block that
    /// reaches the main chain comes back as [`Cmd::BlockAdded`] (the only path to "Block found
    /// by p2pool"). A refusal goes to [`log_refusal`].
    fn spawn_submit(&self, blob: Vec<u8>) {
        let source = self.parts.source.clone();
        let cmds = self.cmds.clone();
        let refusals = self.refusals.clone();
        tokio::spawn(async move {
            match source.submit_block(blob).await {
                Ok((height, id)) => {
                    let _ = cmds.send(Cmd::BlockAdded { height, id });
                }
                Err(r) => log_refusal(&refusals, &r, unix_now()),
            }
        });
    }

    /// A Hyperswarm connection on the sidechain topic: to the P2P server, with the remote DHT
    /// key for bans and one connection per peer.
    fn attach(&self, conn: HyperConn) {
        let (initiator, key) = {
            let info = conn.info();
            (info.initiator, info.remote_public_key)
        };
        let direction = if initiator {
            Direction::Outgoing
        } else {
            Direction::Incoming
        };
        self.p2p.attach(conn, direction, key);
    }

    /// The current numbers to `/status` and the console.
    fn publish(&self) {
        let (height, difficulty, miners, shares) = {
            let sc = lock(&self.sidechain);
            (
                sc.chain_tip().map_or(0, |t| t.sidechain_height),
                difficulty_u64(sc.difficulty()),
                sc.miner_count() as u64,
                sc.len() as u64,
            )
        };
        let p2p = self.p2p.stats();
        let (stratum_miners, stratum_hashrate) = self.stratum.as_ref().map_or((0, 0), |s| {
            let stats = s.stats();
            (u64::from(stats.connections), stats.hashrate_15m)
        });
        let fee = self.parts.fee.borrow().clone();
        let status = P2poolStatus {
            height,
            difficulty,
            hashrate: difficulty.checked_div(self.block_time).unwrap_or(0),
            miners,
            shares,
            synced: p2p.synced,
            peers_out: p2p.peers_out as u64,
            peers_in: p2p.peers_in as u64,
            peers_hyper: p2p.peers_hyper as u64,
            stratum_miners,
            stratum_hashrate,
            shares_found: self.shares_found,
            blocks_found: self.blocks_found,
            payout_address: fee.address,
            payout_donation: fee.donation,
            ..self.base.clone()
        };
        self.parts.status.send_if_modified(|s| {
            if **s == status {
                false
            } else {
                *s = Arc::new(status);
                true
            }
        });
    }

    /// The stop steps: the stratum server, the P2P server (peer list written), the sidechain.
    async fn stop(self) {
        self.parts
            .link
            .set(LinkState::Down("p2pool stopped".into()));
        let Self {
            parts,
            sidechain,
            main,
            templates,
            stratum,
            p2p,
            keeps_peer_file,
            ..
        } = self;
        if let Some(stratum) = stratum {
            let step = crate::stopping::step("p2pool stratum");
            let addr = stratum.local_addr();
            let miners = stratum.connections();
            stratum.shutdown();
            drop(stratum);
            step.done_with(format!("{addr} closed, {miners} miners"));
        }

        let step = crate::stopping::step("p2pool P2P");
        let stats = p2p.stats();
        let peers = stats.peers_out + stats.peers_in + stats.peers_hyper;
        match tokio::time::timeout(P2P_STOP, p2p.stop()).await {
            Ok(()) if keeps_peer_file => {
                step.done_with(format!("{peers} peers closed, peer list saved"));
            }
            Ok(()) => step.done_with(format!("{peers} peers closed")),
            Err(_) => step.timed_out(P2P_STOP),
        }

        let step = crate::stopping::step("p2pool sidechain");
        drop(templates);
        let (height, shares) = {
            let sc = lock(&sidechain);
            (sc.chain_tip().map_or(0, |t| t.sidechain_height), sc.len())
        };
        drop(sidechain);
        drop(main);
        step.done_with(format!(
            "{} shares, height {}",
            term::group(shares as u64),
            term::group(height)
        ));
        parts
            .status
            .send_modify(|s| Arc::make_mut(s).state = "stopped".into());
        tracing::info!("p2pool: stopped");
    }
}

/// The next Hyperswarm connection on the sidechain topic (never, without the transport).
async fn next_conn(rx: &mut Option<mpsc::UnboundedReceiver<HyperConn>>) -> Option<HyperConn> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use xkr_chain::{ChainConfig, MinerAddress, set_merge_mining_tag};
    use xkr_stratum::{ShareSubmit, StratumConfig, StratumPool, SubmitOutcome};
    use xkr_txpool::{PoolConfig, PoolTxSummary, TxPool};
    use xkr_types::RawBlock;

    use super::*;

    const WAIT: Duration = Duration::from_secs(60);

    #[test]
    fn miner_data_maps_to_the_template() {
        let data = MinerData {
            major_version: 7,
            height: 3_000_005,
            prev_id: Hash([3; 32]),
            difficulty: 5_000_000_000,
            median_weight: 100_000,
            already_generated_coins: 987_654_321,
            median_timestamp: 1_800_000_000,
            unlock_window: 20,
            tx_backlog: vec![
                PoolTxSummary {
                    hash: Hash([1; 32]),
                    size: 2_000,
                    fee: 10_000,
                    received: 1_799_999_000,
                },
                PoolTxSummary {
                    hash: Hash([2; 32]),
                    size: 300,
                    fee: 10,
                    received: 1_799_999_900,
                },
            ],
        };
        let params = Network::Mainnet.params();
        let t = template_miner_data(&data, params);
        assert_eq!(t.major_version, 7);
        assert_eq!(
            t.minor_version,
            params.forks().template_minor_version(7),
            "the minor version of the node's own templates"
        );
        assert_eq!(t.height, 3_000_005);
        assert_eq!(t.prev_id, Hash([3; 32]));
        assert_eq!(t.difficulty, Difficulty::from_u64(5_000_000_000));
        assert_eq!(t.median_weight, 100_000);
        assert_eq!(t.already_generated_coins, 987_654_321);
        assert_eq!(t.median_timestamp, 1_800_000_000);
        assert_eq!(
            t.tx_backlog,
            [
                MempoolTx {
                    id: Hash([1; 32]),
                    weight: 2_000,
                    fee: 10_000,
                    time_received: 1_799_999_000,
                },
                MempoolTx {
                    id: Hash([2; 32]),
                    weight: 300,
                    fee: 10,
                    time_received: 1_799_999_900,
                },
            ]
        );
        // a v1 block has minor version 0
        let old = MinerData {
            major_version: 1,
            ..data
        };
        assert_eq!(template_miner_data(&old, params).minor_version, 0);
    }

    #[test]
    fn seed_heights_follow_the_cache_cleanup() {
        assert_eq!(seed_heights(100, 960), [0]);
        assert_eq!(seed_heights(2_620_000, 960), [2_619_392, 2_617_344]);
        assert_eq!(seed_heights(2_621_500, 960), [2_619_392]);
        // the cache keeps exactly these seeds
        let mut c = MainChainCache::new(960);
        let next = 2_620_000u64;
        for h in (2_600_000..next).chain([2_619_392, 2_617_344, 2_615_296]) {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&h.to_le_bytes());
            c.insert_header(ChainMain {
                height: h,
                id: Hash(id),
                ..ChainMain::default()
            });
        }
        c.set_tip(MainChainTip {
            height: next,
            ..MainChainTip::default()
        });
        for s in seed_heights(next, 960) {
            assert!(c.block_at_height(s).is_some(), "{s}");
        }
        assert!(c.block_at_height(2_615_296).is_none());
    }

    #[test]
    fn settings_from_the_command_line() {
        use clap::Parser as _;
        let args = crate::cli::Args::try_parse_from(["xkrd", "--p2pool"]).unwrap();
        let s = P2poolSettings::from_args(&args, Network::Mainnet, Path::new("/d")).unwrap();
        assert_eq!(s.sidechain, SidechainConfig::main());
        assert_eq!(s.consensus_id, SidechainConfig::main().consensus_id());
        assert_eq!(s.stratum, StratumServerConfig::default());
        assert_eq!(s.p2p.listen, None);
        assert_eq!(s.p2p.external_port, 37_889);
        assert_eq!((s.p2p.out_peers, s.p2p.in_peers), (10, 450));
        assert_eq!(s.dir.as_deref(), Some(Path::new("/d/p2pool")));
        assert!(s.seeds && !s.uncapped_pplns);
        let cfg = s.server_config();
        assert!(!cfg.seeds.is_empty(), "the default sidechain has seeds");
        assert_eq!(
            cfg.peer_file.as_deref(),
            Some(Path::new("/d/p2pool/p2pool_peers.txt"))
        );
        let fee = Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        };
        assert_eq!(
            step_detail(&s, &fee),
            "sidechain default · stratum 127.0.0.1:3333 · payout nilsjr (donation)"
        );
        let own = Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: false,
        };
        assert_eq!(
            step_detail(&s, &own),
            "sidechain default · stratum 127.0.0.1:3333 · payout SEKReSw2cP…kcBJZGc"
        );

        let args = crate::cli::Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--p2pool-mini",
            "--p2pool-p2p",
            "0.0.0.0:37900",
            "--p2pool-stratum",
            "0.0.0.0:3400",
            "--p2pool-no-autodiff",
            "--p2pool-stratum-ban-time",
            "30",
            "--p2pool-addpeers",
            "10.0.0.2:37888",
        ])
        .unwrap();
        let s = P2poolSettings::from_args(&args, Network::Mainnet, Path::new("/d")).unwrap();
        assert_eq!(sidechain_label(&s.sidechain), "mini");
        assert_eq!(s.p2p.listen, Some("0.0.0.0:37900".parse().unwrap()));
        assert_eq!(s.p2p.external_port, 37_900);
        assert!(!s.stratum.auto_diff);
        assert_eq!(s.stratum.ban_time, Duration::from_secs(30));
        assert_eq!(s.server_config().add_peers, ["10.0.0.2:37888"]);
        assert_eq!(
            relay_nodes(s.stratum.listen)[0],
            "127.0.0.1:3400",
            "the relay reaches a stratum on every interface over loopback"
        );
        assert_eq!(relay_nodes("[::]:3333".parse().unwrap())[0], "[::1]:3333");
        assert_eq!(
            relay_nodes("10.1.2.3:3333".parse().unwrap())[0],
            "10.1.2.3:3333"
        );

        let nano = crate::cli::Args::try_parse_from(["xkrd", "--p2pool", "--p2pool-nano"]).unwrap();
        let s = P2poolSettings::from_args(&nano, Network::Mainnet, Path::new("/d")).unwrap();
        assert_eq!(sidechain_label(&s.sidechain), "nano");
        assert_eq!(s.p2p.external_port, 37_890);

        // uncapped PPLNS only on the testnet
        let uncapped =
            crate::cli::Args::try_parse_from(["xkrd", "--p2pool", "--p2pool-uncapped-pplns"])
                .unwrap();
        assert!(P2poolSettings::from_args(&uncapped, Network::Mainnet, Path::new("/d")).is_err());
        assert!(
            P2poolSettings::from_args(&uncapped, Network::Testnet, Path::new("/d"))
                .unwrap()
                .uncapped_pplns
        );

        // a custom sidechain from a file (the testnet suite's)
        let file =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../p2pool/sidechain-testnet.json");
        let custom = crate::cli::Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--p2pool-config",
            file.to_str().unwrap(),
        ])
        .unwrap();
        let s = P2poolSettings::from_args(&custom, Network::Testnet, Path::new("/d")).unwrap();
        assert_eq!(s.sidechain.name, "xkr-testnet-suite");
        assert_eq!(sidechain_label(&s.sidechain), "xkr-testnet-suite");
        assert_eq!(s.sidechain.pplns_window, 60);
        assert!(
            s.server_config().seeds.is_empty(),
            "a custom sidechain has no seeds"
        );
        let missing = crate::cli::Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--p2pool-config",
            "/nonexistent/sidechain.json",
        ])
        .unwrap();
        assert!(
            P2poolSettings::from_args(&missing, Network::Testnet, Path::new("/d"))
                .unwrap_err()
                .starts_with("--p2pool-config /nonexistent/sidechain.json:")
        );
    }

    #[test]
    fn hyperswarm_only_settings() {
        use clap::Parser as _;
        let args = crate::cli::Args::try_parse_from([
            "xkrd",
            "--p2pool",
            "--no-p2pool-tcp",
            "--p2pool-addpeers",
            "10.0.0.1:37889",
            "--p2pool-p2p",
            "0.0.0.0:37889",
        ])
        .unwrap();
        let s = P2poolSettings::from_args(&args, Network::Mainnet, Path::new("/d")).unwrap();
        assert!(!s.seeds);
        assert!(s.add_peers.is_empty());
        assert_eq!(s.p2p.out_peers, 0);
        assert_eq!(s.p2p.listen, None);
        assert!(s.server_config().seeds.is_empty());
    }

    #[test]
    fn found_block_refusals() {
        // Stale: the height is taken, the transactions left the pool, not on the tip.
        assert!(submit_refusal(&SubmitError::TransactionAbsent(Hash([1; 32]))).stale);
        assert!(!submit_refusal(&SubmitError::Decode("short".into())).stale);
        assert_eq!(outcome_refusal(&AddBlockOutcome::AddedToMain), None);
        assert_eq!(outcome_refusal(&AddBlockOutcome::AddedAndSwitched), None);
        for o in [
            AddBlockOutcome::AlreadyExists,
            AddBlockOutcome::Orphan,
            AddBlockOutcome::AddedToAlternative,
        ] {
            assert!(outcome_refusal(&o).is_some_and(|r| r.stale), "{o:?}");
        }
        let invalid = outcome_refusal(&AddBlockOutcome::Invalid(
            xkr_chain::RejectReason::NotConnected,
        ))
        .unwrap();
        assert!(!invalid.stale);

        // Other refusals: one line per minute with the count since the last line.
        let mut log = RefusalLog::default();
        assert_eq!(
            log.note(1000, "a").as_deref(),
            Some("p2pool: 1 found block refused in the last minute: a")
        );
        assert_eq!(log.note(1001, "b"), None);
        assert_eq!(log.note(1059, "c"), None);
        assert_eq!(
            log.note(1060, "d").as_deref(),
            Some("p2pool: 3 found blocks refused in the last minute: d")
        );
        assert_eq!(log.note(1061, "e"), None);

        // A stale refusal is not counted.
        let shared = Mutex::new(RefusalLog::default());
        let stale = Refusal {
            stale: true,
            reason: "transaction is not in the pool".into(),
        };
        log_refusal(&shared, &stale, 1000);
        assert_eq!(shared.lock().unwrap().count, 0);
        assert_eq!(shared.lock().unwrap().said_at, None);
        let other = Refusal {
            stale: false,
            reason: "invalid".into(),
        };
        log_refusal(&shared, &other, 1000);
        log_refusal(&shared, &other, 1010);
        assert_eq!(shared.lock().unwrap().said_at, Some(1000));
        assert_eq!(shared.lock().unwrap().count, 1);
    }

    #[test]
    fn terminal_lines() {
        let plain = Style::new(false);
        assert_eq!(
            share_found_line(plain, 29_277, 150_800_000),
            "Share found · height 29 277 · diff 150 800 000"
        );
        assert_eq!(
            block_found_line(plain, 2_620_300),
            "Block found by p2pool at height 2 620 300"
        );
        assert_eq!(
            running_line(plain, "127.0.0.1:3333".parse().unwrap()),
            "p2pool running · stratum 127.0.0.1:3333"
        );
        assert_eq!(
            running_line_stratum_off(plain),
            "p2pool running · stratum off"
        );
        assert_eq!(
            stratum_off_line(plain, "127.0.0.1:3333 in use; set --p2pool-stratum"),
            "stratum: off (127.0.0.1:3333 in use; set --p2pool-stratum)"
        );
        for line in [
            share_found_line(plain, 99_999_999, 999_999_999_999),
            block_found_line(plain, 9_999_999),
        ] {
            assert!(line.chars().count() <= 60, "{line}");
        }
    }

    /// A testnet chain in a temporary directory, removed on drop.
    struct TestChain {
        dir: PathBuf,
        chain: Option<Arc<Chain>>,
    }

    impl TestChain {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xkrd-p2pool-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store =
                xkr_storage::Store::open_with_cache(dir.join("chain.redb"), 16 << 20).unwrap();
            let pool = Arc::new(TxPool::new(PoolConfig::default()));
            let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).unwrap();
            Self {
                dir,
                chain: Some(Arc::new(chain)),
            }
        }

        fn chain(&self) -> &Arc<Chain> {
            self.chain.as_ref().unwrap()
        }

        /// Mines one block at the testnet's difficulty 1 (every nonce is a solution).
        fn mine(&self, miner: &MinerAddress) {
            let chain = self.chain();
            let t = chain.block_template(miner, &[]).unwrap();
            let mut block = t.block.clone();
            set_merge_mining_tag(&mut block).unwrap();
            let blob = xkr_serialization::encode_block(&block).unwrap();
            let r = chain
                .add_block(RawBlock {
                    block: blob,
                    transactions: t.transactions,
                })
                .unwrap();
            assert!(
                matches!(r.outcome, AddBlockOutcome::AddedToMain),
                "{:?}",
                r.outcome
            );
        }
    }

    impl Drop for TestChain {
        fn drop(&mut self) {
            drop(self.chain.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A deterministic random source for templates built twice.
    struct Counter(u64);

    impl rand_core::RngCore for Counter {
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            self.0
        }
        fn fill_bytes(&mut self, dst: &mut [u8]) {
            for b in dst {
                *b = self.next_u64() as u8;
            }
        }
    }

    /// The same chain through the node's own storage and through remote nodes on the clients
    /// topic: the same miner data, the same headers in the main-chain cache and the same
    /// block template.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_nodes_give_the_template_of_the_local_chain() {
        use crate::remote::tests as rt;

        let tc = rt::TestChain::new("p2pool-same");
        for _ in 0..6 {
            tc.mine(&rt::miner());
        }
        let (commands, _) = mpsc::unbounded_channel();
        let local: Arc<dyn MainChainSource> = LocalChain::new(tc.chain().clone(), commands);
        let (remote, stop) = rt::start(
            vec![
                ([1; 32], rt::TestNode::real(&tc)),
                ([2; 32], rt::TestNode::real(&tc)),
            ],
            xkr_chain::StateHashes::default(),
        );
        rt::wait_for(&remote, |s| s.nodes.len() == 2 && s.height == Some(6)).await;
        let remote: Arc<dyn MainChainSource> = Arc::new(remote);
        assert_eq!(*local.subscribe_tip().borrow(), tc.chain().tip());
        assert_eq!(
            remote.subscribe_tip().borrow().hash,
            local.subscribe_tip().borrow().hash
        );

        let sidechain = SidechainConfig {
            name: "xkrd-p2pool-same".into(),
            password: String::new(),
            block_time: 10,
            min_diff: 1,
            pplns_window: 60,
            uncle_penalty: 20,
        };
        let params = Network::Testnet.params();
        let build = |source: Arc<dyn MainChainSource>| {
            let sidechain = sidechain.clone();
            async move {
                let main = Arc::new(MainChainState::new(&sidechain));
                let (data, top) = source.refresh(main.clone(), None).await.unwrap();
                let sc = SideChain::new(SidechainOptions::new(
                    sidechain,
                    ForkHeights::from_params(source.params()),
                    u64::from(params.mined_money_unlock_window),
                ));
                let wallet = MinerWallet::parse(fee::DEFAULT_FEE_ADDRESS).unwrap();
                let template = xkr_p2pool::BlockTemplate::new()
                    .update_with_rng(
                        &template_miner_data(&data, source.params()),
                        &sc,
                        &*main.read(),
                        &wallet,
                        1_900_000_000,
                        &mut Counter(7),
                    )
                    .unwrap();
                (data, top, template, main)
            }
        };
        let (local_data, local_top, local_template, local_main) = build(local).await;
        let (remote_data, remote_top, remote_template, remote_main) = build(remote).await;
        assert_eq!(local_data, remote_data);
        assert_eq!((local_top, remote_top), (6, 6));
        for h in 0..=6 {
            assert_eq!(
                local_main.block_at_height(h),
                remote_main.block_at_height(h),
                "{h}"
            );
        }
        assert_eq!(
            MainChainView::tip(&*local_main),
            MainChainView::tip(&*remote_main)
        );
        assert_eq!(local_template.height(), 7);
        assert_eq!(
            local_template.hashing_blob(0).blob,
            remote_template.hashing_blob(0).blob
        );
        assert_eq!(
            local_template.mainchain_blob(1, 2),
            remote_template.mainchain_blob(1, 2)
        );
        stop.cancel();
    }

    /// Waits until the status satisfies `f`.
    async fn wait_status(
        rx: &mut watch::Receiver<Arc<P2poolStatus>>,
        f: impl Fn(&P2poolStatus) -> bool,
    ) -> Arc<P2poolStatus> {
        tokio::time::timeout(WAIT, async {
            loop {
                let s = rx.borrow_and_update().clone();
                if f(&s) {
                    return s;
                }
                rx.changed().await.expect("the p2pool task ended");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("status not reached: {:?}", rx.borrow()))
    }

    /// Hashes nonces from 0 until one meets `target`; returns the nonce and the hash.
    fn mine_share(blob_hex: &str, target: u64) -> (u32, [u8; 32]) {
        let mut blob = hex::decode(blob_hex).unwrap();
        let off = xkr_stratum::blob::nonce_offset(&blob);
        for nonce in 0u32.. {
            blob[off..off + 4].copy_from_slice(&nonce.to_le_bytes());
            let h = xkr_crypto::cn_turtle_lite_slow_hash_v2(&blob).0;
            if u64::from_le_bytes(h[24..].try_into().unwrap()) < target {
                return (nonce, h);
            }
        }
        unreachable!()
    }

    /// The whole service on a synchronized testnet chain, in-process and without peers: it
    /// waits for the sync, builds a template on the chain's tip, a stratum miner gets a job,
    /// its share (difficulty 1 on both chains) lands in the sidechain and, as a main-chain
    /// block, on the chain; then it stops cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_from_a_stratum_miner_lands_in_the_sidechain() {
        let tc = TestChain::new("service");
        let miner = xkr_rpc::parse_miner_address(fee::DEFAULT_FEE_ADDRESS).unwrap();
        // the sidechain's fork table (always the mainnet one) has major version 5 from height 4
        for _ in 0..4 {
            tc.mine(&miner);
        }
        let top = tc.chain().tip().height;
        assert_eq!(top, 4);

        // a sidechain of difficulty 1, so the first share is a sidechain share
        let sidechain = SidechainConfig {
            name: "xkrd-p2pool-test".into(),
            password: String::new(),
            block_time: 10,
            min_diff: 1,
            pplns_window: 60,
            uncle_penalty: 20,
        };
        let mut settings = P2poolSettings::new(sidechain);
        settings.stratum.listen = "127.0.0.1:0".parse().unwrap();
        settings.stratum.auto_diff = false;
        settings.seeds = false;

        let (net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_fee_tx, fee_rx) = watch::channel(Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        });
        let (status_tx, mut status) = watch::channel(Arc::new(P2poolStatus::default()));
        // no P2P manager: found blocks go straight to the chain
        let (commands, _) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(
            settings,
            P2poolNode {
                source: LocalChain::new(tc.chain().clone(), commands),
                net,
                fee: fee_rx,
                status: status_tx,
                hyper: None,
                term: None,
                link: PoolLink::default(),
            },
            stop.clone(),
        ));

        // waits for the node to synchronize
        let s = wait_status(&mut status, |s| s.state == "waiting").await;
        assert_eq!(s.sidechain, "xkrd-p2pool-test");
        assert!(s.payout_donation);
        net_tx.send_replace(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        let s = wait_status(&mut status, |s| s.state == "running").await;
        let addr = s.stratum.clone().expect("stratum address");
        assert_eq!((s.shares, s.shares_found, s.blocks_found), (0, 0, 0));

        // a miner logs in, gets a job on the chain's tip and finds a share
        let client_stop = CancellationToken::new();
        let pool = StratumPool::spawn(
            StratumConfig {
                pools: vec![addr],
                login: "x+1000".into(),
                pass: "x".into(),
                agent: "xkrd-test/1.0".into(),
            },
            client_stop.clone(),
        );
        let mut jobs = pool.jobs();
        let job = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(j) = jobs.borrow_and_update().clone() {
                    return j;
                }
                jobs.changed().await.unwrap();
            }
        })
        .await
        .expect("no job");
        assert_eq!(job.raw["algo"], "cryptonight_turtle");
        assert_eq!(job.raw["height"], top + 1);
        let target = xkr_stratum::target::parse_target(&job.target).unwrap();
        let blob = job.blob.clone();
        let (nonce, hash) = tokio::task::spawn_blocking(move || mine_share(&blob, target))
            .await
            .unwrap();
        let outcome = pool
            .submit(ShareSubmit {
                job_id: job.job_id.clone(),
                nonce: hex::encode(nonce.to_le_bytes()),
                result: hex::encode(hash),
            })
            .await;
        assert_eq!(outcome, SubmitOutcome::Accepted);

        // the share is the sidechain's first (height 0) and a main-chain block
        let s = wait_status(&mut status, |s| s.shares_found == 1 && s.blocks_found == 1).await;
        assert_eq!(s.shares, 1);
        assert_eq!(s.height, 0);
        assert_eq!(s.stratum_miners, 1);
        assert_eq!(tc.chain().tip().height, top + 1);

        client_stop.cancel();
        stop.cancel();
        tokio::time::timeout(WAIT, task)
            .await
            .expect("p2pool stops")
            .unwrap();
        assert_eq!(status.borrow().state, "stopped");
    }

    /// The built-in miner via p2pool (`start_mining p2pool`), in process: it waits while
    /// p2pool waits for the sync, then mines the node's template; its share (difficulty 1 on
    /// both chains) lands in the sidechain paid to the node's payout address, which the next
    /// template's PPLNS window pays, and the share is also a main-chain block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_share_from_the_built_in_miner_lands_in_the_sidechain() {
        let tc = TestChain::new("miner");
        let miner_keys = xkr_rpc::parse_miner_address(fee::DEFAULT_FEE_ADDRESS).unwrap();
        for _ in 0..4 {
            tc.mine(&miner_keys);
        }
        let top = tc.chain().tip().height;

        // the node's own payout address (not the donation)
        let g = xkr_types::PublicKey(
            xkr_crypto::EdwardsPoint::mul_base(&xkr_crypto::Scalar::ONE)
                .compress()
                .to_bytes(),
        );
        let payout = xkr_wallet_core::Address::new(g, g).encode();
        let payout_wallet = MinerWallet::parse(&payout).unwrap();

        let sidechain = SidechainConfig {
            name: "xkrd-p2pool-test-miner".into(),
            password: String::new(),
            block_time: 10,
            min_diff: 1,
            pplns_window: 60,
            uncle_penalty: 20,
        };
        let mut settings = P2poolSettings::new(sidechain);
        settings.stratum.listen = "127.0.0.1:0".parse().unwrap();
        settings.seeds = false;

        let (net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_fee_tx, fee_rx) = watch::channel(Fee {
            address: Some(payout.clone()),
            donation: false,
        });
        let (status_tx, mut status) = watch::channel(Arc::new(P2poolStatus::default()));
        let (commands, _) = mpsc::unbounded_channel();
        let link = PoolLink::default();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(
            settings,
            P2poolNode {
                source: LocalChain::new(tc.chain().clone(), commands.clone()),
                net: net.clone(),
                fee: fee_rx,
                status: status_tx,
                hyper: None,
                term: None,
                link: link.clone(),
            },
            stop.clone(),
        ));
        wait_status(&mut status, |s| s.state == "waiting").await;
        assert!(matches!(link.state(), LinkState::Waiting));

        // the miner starts before p2pool runs and waits for it
        let (mining_tx, mut mining) = watch::channel(Arc::new(xkr_rpc::MiningStatus::default()));
        let miner_stop = CancellationToken::new();
        let thread = crate::miner::spawn(
            crate::miner::MinerConfig {
                mode: crate::miner::MineMode::P2pool(link.clone()),
                threads: 1,
            },
            Some(tc.chain().clone()),
            commands,
            net,
            mining_tx,
            miner_stop.clone(),
        )
        .unwrap();
        net_tx.send_replace(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        wait_status(&mut status, |s| s.state == "running").await;

        // its share is in the sidechain and on the main chain
        let m = tokio::time::timeout(WAIT, async {
            loop {
                let m = mining.borrow_and_update().clone();
                if m.shares_found >= 1 {
                    return m;
                }
                mining.changed().await.expect("the miner ended");
            }
        })
        .await
        .expect("no share from the built-in miner");
        assert!(m.active);
        assert_eq!(m.mode, "p2pool");
        assert_eq!(m.address, payout);
        assert_eq!(m.blocks_found, 0);
        let s = wait_status(&mut status, |s| s.shares_found >= 1 && s.blocks_found >= 1).await;
        assert!(s.shares >= 1);
        assert!(tc.chain().tip().height > top);

        // the next template builds on it and its PPLNS window pays the payout wallet
        let LinkState::Running(pool) = link.state() else {
            panic!("p2pool not running");
        };
        let following = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(t) = pool.current()
                    && t.sidechain_height() >= 1
                {
                    return t;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("no template on the miner's share");
        assert_eq!(following.block.miner_wallet, payout_wallet);
        assert!(!following.shares.is_empty());
        assert!(
            following
                .shares
                .iter()
                .all(|share| share.wallet == payout_wallet && !share.weight.is_zero())
        );

        miner_stop.cancel();
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .unwrap()
            .expect("the miner thread ends");
        assert!(!mining.borrow().active);
        stop.cancel();
        tokio::time::timeout(WAIT, task)
            .await
            .expect("p2pool stops")
            .unwrap();
        assert!(matches!(link.state(), LinkState::Down(_)));
    }

    /// A stratum address in use: the start-up check says so, and the service runs without
    /// the stratum server (sidechain and P2P only) instead of failing; it also stops cleanly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stratum_port_in_use_runs_p2pool_without_stratum() {
        let tc = TestChain::new("stratum-in-use");
        let miner = xkr_rpc::parse_miner_address(fee::DEFAULT_FEE_ADDRESS).unwrap();
        for _ in 0..4 {
            tc.mine(&miner);
        }
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();
        let reason = format!("{addr} in use; set --p2pool-stratum");

        let sidechain = SidechainConfig {
            name: "xkrd-p2pool-test-off".into(),
            password: String::new(),
            block_time: 10,
            min_diff: 1,
            pplns_window: 60,
            uncle_penalty: 20,
        };
        let mut settings = P2poolSettings::new(sidechain);
        settings.stratum.listen = addr;
        settings.seeds = false;

        // the start-up check
        let mut checked = settings.clone();
        assert!(!checked.check_stratum());
        assert_eq!(checked.stratum_off.as_deref(), Some(reason.as_str()));
        let fee = Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        };
        assert_eq!(
            step_detail(&checked, &fee),
            "sidechain xkrd-p2pool-test-off · stratum off · payout nilsjr (donation)"
        );
        let waiting = base_status(&checked, &fee);
        assert_eq!(waiting.stratum, None);
        assert_eq!(waiting.stratum_off.as_deref(), Some(reason.as_str()));

        // the port taken only after the check: the service finds it when it starts
        let (net_tx, net) = watch::channel(Arc::new(NetStatus::default()));
        let (_fee_tx, fee_rx) = watch::channel(fee);
        let (status_tx, mut status) = watch::channel(Arc::new(P2poolStatus::default()));
        let (commands, _) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(
            settings,
            P2poolNode {
                source: LocalChain::new(tc.chain().clone(), commands),
                net,
                fee: fee_rx,
                status: status_tx,
                hyper: None,
                term: None,
                link: PoolLink::default(),
            },
            stop.clone(),
        ));
        wait_status(&mut status, |s| s.state == "waiting").await;
        net_tx.send_replace(Arc::new(NetStatus {
            synced: true,
            ..NetStatus::default()
        }));
        let s = wait_status(&mut status, |s| s.state != "waiting").await;
        assert_eq!(s.state, "running", "{s:?}");
        assert_eq!(s.stratum, None);
        assert_eq!(s.stratum_off.as_deref(), Some(reason.as_str()));
        assert_eq!(s.error, None);
        assert_eq!(s.stratum_miners, 0);

        stop.cancel();
        tokio::time::timeout(WAIT, task)
            .await
            .expect("p2pool stops")
            .unwrap();
        assert_eq!(status.borrow().state, "stopped");
        drop(taken);
    }
}
