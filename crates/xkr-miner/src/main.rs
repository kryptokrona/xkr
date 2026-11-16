//! `xkr-miner`: solo mining against a daemon with the C++ `miner`'s flags
//! (`src/miner/mining_config.cpp` `MiningConfig::parse`), or pool mining over stratum.
//!
//! ```text
//! xkr-miner --daemon-address 127.0.0.1:11898 --address SEKR... [--threads N] [--limit N]
//! xkr-miner --pool stratum+tcp://pool:3333 [--pool stratum+ssl://backup:443] --pool-user SEKR...
//! ```
//!
//! The C++ options keep their names, defaults and checks (`--daemon-address`, `--daemon-host`,
//! `--daemon-rpc-port`, `--scan-time`, `--address`, `--threads`, `--limit`,
//! `--first-block-timestamp`, `--block-timestamp-interval`), so the binary can replace `miner`
//! as is. Added: `--pool` (several for fail-over), `--pool-user`, `--pool-pass`, `--rig-id`,
//! `--log-json` (one JSON object per line on stdout for a host app), `--network` (default
//! ports) and `--software-aes`.
//!
//! On a colour terminal (not `--log-json`, `NO_COLOR` or `TERM=dumb`) it looks like xkrd: the
//! "kryptokrona miner" header, the start-up steps, a live panel ([`panel`]) and the stop steps
//! (`xkr_term`). Otherwise it prints the C++ miner's lines (or JSON lines) as before.
#![forbid(unsafe_code)]

mod panel;

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use xkr_consensus::Network;
use xkr_miner::{
    AesImpl, Event, MinerOptions, Observer, SoloConfig, SoloSource, StratumSource, SubmitResult,
    Timestamps, format_speed, run,
};
use xkr_stratum::{LoginExtras, StratumConfig};
use xkr_term::steps::{
    Kind, Mode as StepMode, Reporter, SignalAction, Step, Subject, signal_action, step_on,
};
use xkr_term::term::{self, Color, Term, Word};
use xkr_wallet_core::Address;

use crate::panel::{Flash, PanelState};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum NetworkArg {
    /// Kryptokrona mainnet (daemon RPC port 11898).
    Mainnet,
    /// The C++ `USE_TESTNET` network (daemon RPC port 11899).
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

#[derive(Debug, Parser)]
#[command(
    name = "xkr-miner",
    version,
    about = "Kryptokrona CPU miner: solo against a daemon, or in a stratum pool"
)]
struct Cli {
    /// The daemon [host:port] combination to use for node operations. This option overrides
    /// --daemon-host and --daemon-rpc-port. Also accepts http:// and https:// URLs.
    #[arg(long, value_name = "host:port", help_heading = "Daemon")]
    daemon_address: Option<String>,

    /// The daemon host to use for node operations.
    #[arg(
        long,
        value_name = "host",
        default_value = "127.0.0.1",
        help_heading = "Daemon"
    )]
    daemon_host: String,

    /// The daemon RPC port to use for node operations [default: 11898, testnet 11899].
    #[arg(long, value_name = "#", help_heading = "Daemon")]
    daemon_rpc_port: Option<u16>,

    /// Blockchain polling interval (seconds). How often miner will check the Blockchain for
    /// updates.
    #[arg(long, value_name = "#", default_value_t = 1, help_heading = "Daemon")]
    scan_time: u64,

    /// The valid CryptoNote miner's address.
    #[arg(long, value_name = "address", help_heading = "Mining")]
    address: Option<String>,

    /// Timestamp incremental step for each subsequent block. May be set only if
    /// --first-block-timestamp has been set.
    #[arg(
        long,
        value_name = "#",
        default_value_t = 0,
        allow_negative_numbers = true,
        help_heading = "Mining"
    )]
    block_timestamp_interval: i64,

    /// Set timestamp to the first mined block. 0 means leave timestamp unchanged.
    #[arg(long, value_name = "#", default_value_t = 0, help_heading = "Mining")]
    first_block_timestamp: u64,

    /// Mine this exact quantity of blocks and then stop. 0 means no limit. In a pool: accepted
    /// shares.
    #[arg(long, value_name = "#", default_value_t = 0, help_heading = "Mining")]
    limit: u64,

    /// The mining threads count. Must not exceed hardware capabilities [default: all cores].
    #[arg(long, value_name = "#", help_heading = "Mining")]
    threads: Option<usize>,

    /// Mine in a pool: stratum+tcp://host:port or stratum+ssl://host:port (host:port is plain
    /// TCP). Repeat for fail-over pools, tried in order.
    #[arg(long = "pool", value_name = "URL", help_heading = "Pool")]
    pools: Vec<String>,

    /// Pool login: the payout address, optionally with a pool-specific suffix such as .rigid
    /// [default: --address].
    #[arg(long, value_name = "user", help_heading = "Pool")]
    pool_user: Option<String>,

    /// Pool password.
    #[arg(long, value_name = "pass", default_value = "x", help_heading = "Pool")]
    pool_pass: String,

    /// Rig name sent to the pool (rigid).
    #[arg(long, value_name = "name", help_heading = "Pool")]
    rig_id: Option<String>,

    /// Status as JSON lines on stdout (started, job, hashrate every 10 s, accepted, rejected,
    /// stopped); logs go to stderr.
    #[arg(long)]
    log_json: bool,

    /// Network of the daemon (default RPC port).
    #[arg(long, value_enum, default_value_t = NetworkArg::Mainnet)]
    network: NetworkArg,

    /// Use the software AES rounds even if the CPU has AES instructions.
    #[arg(long)]
    software_aes: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let term = Term::stdout();
    // The terminal look needs a colour terminal; files, pipes, NO_COLOR, TERM=dumb and
    // --log-json keep the line output (testnet scenario 65 reads it).
    let fancy = !cli.log_json && term.tty() && term.color();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if fancy {
        // log lines above the panel
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(true)
            .with_writer(term.clone())
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }
    let ui = fancy.then(|| Ui::new(term, cli.network));
    match start(&cli, ui) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// How the miner gets its work.
#[derive(Debug)]
enum Mode {
    Solo {
        daemon: String,
        cfg: SoloConfig,
    },
    Pool {
        cfg: StratumConfig,
        extras: LoginExtras,
        /// Algorithm by height when a job does not name one.
        network: Network,
    },
}

/// C++ `MiningConfig::parse` checks, then the miner.
fn start(cli: &Cli, ui: Option<Ui>) -> Result<(), String> {
    let concurrency = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let threads = cli.threads.unwrap_or(concurrency);
    if threads == 0 || threads > concurrency {
        return Err(format!("--threads option must be 1..{concurrency}"));
    }
    if cli.scan_time == 0 {
        return Err("--scan-time must not be zero".into());
    }
    if cli.first_block_timestamp == 0 && cli.block_timestamp_interval != 0 {
        return Err(
            "If you specify --block-timestamp-interval you must also specify \
                    --first-block-timestamp"
                .into(),
        );
    }
    let aes = if cli.software_aes {
        AesImpl::Software
    } else {
        AesImpl::detect()
    };
    let mode = if cli.pools.is_empty() {
        let network: Network = cli.network.into();
        let daemon = daemon_url(cli, network.params().rpc_port)?;
        let address = mining_address(cli.address.as_deref())?;
        Mode::Solo {
            daemon,
            cfg: SoloConfig {
                address,
                scan_time: Duration::from_secs(cli.scan_time),
                timestamps: Timestamps {
                    first: cli.first_block_timestamp,
                    interval: cli.block_timestamp_interval,
                },
            },
        }
    } else {
        let login = cli
            .pool_user
            .clone()
            .or_else(|| cli.address.clone())
            .ok_or("--pool needs --pool-user (or --address)")?;
        Mode::Pool {
            cfg: StratumConfig {
                pools: cli.pools.clone(),
                login,
                pass: cli.pool_pass.clone(),
                agent: format!("xkr-miner/{VERSION}"),
            },
            extras: LoginExtras {
                rig_id: cli.rig_id.clone(),
            },
            network: cli.network.into(),
        }
    };
    let opts = MinerOptions {
        threads,
        aes,
        report_interval: if ui.is_some() {
            PANEL_REPORT
        } else if cli.log_json {
            Duration::from_secs(10)
        } else {
            Duration::from_mins(1)
        },
        limit: cli.limit,
        thread_name: "xkr-mine".into(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    runtime.block_on(mine(mode, &opts, cli.log_json, ui))
}

/// C++ `parseDaemonAddressFromString` for `--daemon-address` (`host` or `host:port`), else
/// `--daemon-host` and `--daemon-rpc-port`; `http://` / `https://` URLs pass through.
fn daemon_url(cli: &Cli, default_port: u16) -> Result<String, String> {
    let Some(address) = cli.daemon_address.as_deref().map(str::trim) else {
        let port = cli.daemon_rpc_port.unwrap_or(default_port);
        return Ok(host_port(&cli.daemon_host, port));
    };
    if address.contains("://") {
        return Ok(address.to_owned());
    }
    let mut parts = address.split(':');
    let host = parts.next().unwrap_or_default();
    let port = match parts.next() {
        Some(p) => p
            .trim()
            .parse::<u16>()
            .map_err(|_| "Could not parse --daemon-address option".to_owned())?,
        None => default_port,
    };
    if host.is_empty() {
        return Err("Could not parse --daemon-address option".into());
    }
    Ok(host_port(host, port))
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A standard address (C++ `validateAddresses` with integrated addresses not allowed).
fn check_address(text: &str) -> Result<Address, String> {
    let address = Address::parse(text.trim()).map_err(|e| e.to_string())?;
    if address.payment_id.is_some() {
        return Err("integrated addresses are not allowed for mining".into());
    }
    Ok(address)
}

/// `--address`, or (as C++ does) asked for on the terminal until it is valid.
fn mining_address(given: Option<&str>) -> Result<Address, String> {
    let mut candidate = given.map(str::to_owned);
    loop {
        if let Some(text) = &candidate {
            match check_address(text) {
                Ok(a) => return Ok(a),
                Err(e) => {
                    if !std::io::stdin().is_terminal() {
                        return Err(format!("Address is not valid: {e}"));
                    }
                    println!("Address is not valid: {e}");
                }
            }
        } else if !std::io::stdin().is_terminal() {
            return Err("--address (or --pool with --pool-user) is required".into());
        }
        print!("What address do you want to mine to?: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let read = std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("no address given".into());
        }
        candidate = Some(line.trim().to_owned());
    }
}

/// Runs the miner until Ctrl-C / SIGTERM / SIGHUP or `--limit`.
async fn mine(mode: Mode, opts: &MinerOptions, json: bool, ui: Option<Ui>) -> Result<(), String> {
    let shutdown = CancellationToken::new();
    tokio::spawn(stop_on_signals(shutdown.clone(), ui.clone()));
    let solo = matches!(mode, Mode::Solo { .. });
    let mut out = match &ui {
        Some(ui) => {
            ui.live().panel.mode = match &mode {
                Mode::Solo { .. } => "solo".to_owned(),
                Mode::Pool { cfg, .. } => cfg
                    .pools
                    .first()
                    .map_or_else(|| "pool".to_owned(), |url| panel::pool_mode(url)),
            };
            ui.term.print(&term::header(ui.term.style(), Word::Miner));
            ui.start.begin();
            tokio::spawn(draw_panel(ui.clone(), shutdown.clone()));
            Out::Fancy(Box::new(Fancy::new(ui.clone(), solo)))
        }
        None => Out::Plain(Output { json, solo }),
    };
    let start_ui = ui.as_ref().map(|u| &u.start);
    let stop_ui = ui.as_ref().map(|u| &u.stop);
    let result = match mode {
        Mode::Solo { daemon, cfg } => {
            let step = step_on(start_ui, "Daemon");
            let mut source = match SoloSource::connect(&daemon, cfg) {
                Ok(source) => source,
                Err(e) => {
                    step.fail(e.to_string());
                    if let Some(ui) = &ui {
                        ui.start.abort();
                    }
                    return Err(e.to_string());
                }
            };
            step.done_with(daemon.clone());
            out.threads_step();
            let result = run(&mut source, opts, &mut out, &shutdown).await;
            let step = step_on(stop_ui, "daemon connection");
            drop(source);
            step.done_with(format!("{daemon} closed"));
            result
        }
        Mode::Pool {
            cfg,
            extras,
            network,
        } => {
            let step = step_on(start_ui, "Pool");
            let pools = cfg.pools.join(", ");
            let mut source =
                StratumSource::spawn(cfg, extras, shutdown.clone()).on_network(network);
            step.done_with(pools.clone());
            out.threads_step();
            let result = run(&mut source, opts, &mut out, &shutdown).await;
            let step = step_on(stop_ui, "pool connection");
            shutdown.cancel();
            drop(source);
            step.done_with(format!("{pools} closed"));
            result
        }
    };
    shutdown.cancel();
    match result {
        Ok(stats) => {
            if let Some(ui) = &ui {
                ui.stop
                    .end_with(&totals_text(&stats, ui.started.elapsed().as_secs()));
            }
            Ok(())
        }
        Err(e) => {
            if let Out::Fancy(f) = &mut out
                && let Some(step) = f.threads_step.take()
            {
                step.fail(e.to_string());
            }
            if let Some(ui) = &ui {
                ui.start.abort();
                ui.stop.end_with(&format!(
                    "· uptime {}",
                    term::duration_words(ui.started.elapsed().as_secs())
                ));
            }
            Err(format!("cannot start the mining threads: {e}"))
        }
    }
}

/// `· 12 accepted · 0 rejected · 1 234 567 hashes · uptime 3 h 12 min` (the final line).
fn totals_text(stats: &xkr_miner::Stats, uptime_secs: u64) -> String {
    format!(
        "· {} accepted · {} rejected · {} hashes · uptime {}",
        stats.accepted,
        stats.rejected,
        term::group(stats.total_hashes),
        term::duration_words(uptime_secs)
    )
}

/// Ctrl-C, SIGTERM or SIGHUP (the terminal went away) stops the miner cleanly; later ones do
/// not cut the stop short (`xkr_term::steps::signal_action`): the second shows a note once.
async fn stop_on_signals(shutdown: CancellationToken, ui: Option<Ui>) {
    let mut count = 0_usize;
    loop {
        wait_for_signal().await;
        count += 1;
        match signal_action(count) {
            SignalAction::Stop => {
                if let Some(ui) = &ui {
                    ui.begin_stop();
                }
                shutdown.cancel();
            }
            // never a forced quit: the threads and the connection always close
            SignalAction::Note => match &ui {
                Some(ui) => ui.stop.say(STOPPING_NOTE),
                None => eprintln!("{STOPPING_NOTE}"),
            },
            SignalAction::Ignore => {}
        }
    }
}

/// Shown once when another signal arrives while the miner stops (xkrd says the same about
/// its database).
const STOPPING_NOTE: &str = "stopping cleanly — please wait, the threads are being stopped";

/// SIGINT (Ctrl-C) everywhere, SIGTERM and SIGHUP on Unix.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hangup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            Some(()) = next_signal(term.as_mut()) => {}
            Some(()) = next_signal(hangup.as_mut()) => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// The next delivery of a Unix signal stream (never, without one).
#[cfg(unix)]
async fn next_signal(stream: Option<&mut tokio::signal::unix::Signal>) -> Option<()> {
    match stream {
        Some(s) => s.recv().await,
        None => std::future::pending().await,
    }
}

/// How often the hashrate is measured for the panel.
const PANEL_REPORT: Duration = Duration::from_secs(2);
/// Panel redraw interval (the spinner's frame rate, 8 fps).
const PANEL_TICK: Duration = Duration::from_millis(125);

/// The terminal look: the shared terminal, the panel's state, the start and stop steps.
#[derive(Debug, Clone)]
struct Ui {
    term: Term,
    live: Arc<Mutex<Live>>,
    start: Arc<Reporter>,
    stop: Arc<Reporter>,
    started: Instant,
}

/// What the panel task draws, and the running stop step of the hashing threads.
#[derive(Debug, Default)]
struct Live {
    panel: PanelState,
    /// Start-up is done: the panel is drawn.
    panel_on: bool,
    /// The miner is stopping: no panel, the stop steps run.
    stopping: bool,
    /// The stop step of the hashing threads, finished by `Event::Stopped`.
    threads_step: Option<Step>,
}

impl Ui {
    fn new(term: Term, network: NetworkArg) -> Self {
        let style = term.style();
        let sink: Arc<dyn xkr_term::steps::Sink> = Arc::new(term.clone());
        let start = Reporter::with_subject(
            Kind::Start,
            Subject::MINER,
            StepMode::Fancy,
            style,
            sink.clone(),
        );
        start.set_subtitle(match network {
            NetworkArg::Mainnet => "Mainnet",
            NetworkArg::Testnet => "Testnet",
        });
        let stop = Reporter::with_subject(Kind::Stop, Subject::MINER, StepMode::Fancy, style, sink);
        Self {
            term,
            live: Arc::new(Mutex::new(Live::default())),
            start,
            stop,
            started: Instant::now(),
        }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stopping starts (a signal, the limit): the panel goes, the stop title and the step of
    /// the hashing threads appear. Once only.
    fn begin_stop(&self) {
        let mut live = self.live();
        if live.stopping {
            return;
        }
        live.stopping = true;
        live.panel_on = false;
        self.term.clear_progress();
        self.start.abort();
        self.stop.begin();
        live.threads_step = Some(self.stop.step("hashing threads"));
    }
}

/// Redraws the panel 8 times a second until `shutdown`.
async fn draw_panel(ui: Ui, shutdown: CancellationToken) {
    let style = ui.term.style();
    let width = ui.term.columns();
    let mut tick = tokio::time::interval(PANEL_TICK);
    let mut frame = 0_usize;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }
        let live = ui.live();
        if live.panel_on && !live.stopping {
            ui.term.panel(panel::render(
                &live.panel,
                Instant::now(),
                frame,
                width,
                style,
            ));
        }
        frame = frame.wrapping_add(1);
    }
}

/// The observer on a colour terminal: the start-up steps until the first job, then the panel.
#[derive(Debug)]
struct Fancy {
    ui: Ui,
    solo: bool,
    threads_step: Option<Step>,
    first_job: Option<Step>,
}

impl Fancy {
    fn new(ui: Ui, solo: bool) -> Self {
        Self {
            ui,
            solo,
            threads_step: None,
            first_job: None,
        }
    }

    fn event(&mut self, event: &Event<'_>) {
        match event {
            Event::Started { threads, aes } => {
                {
                    let mut live = self.ui.live();
                    live.panel.threads = *threads;
                }
                let plural = if *threads == 1 { "thread" } else { "threads" };
                if let Some(step) = self.threads_step.take() {
                    step.done_with(format!("{threads} {plural}, {} AES", aes.name()));
                }
                self.first_job = Some(self.ui.start.step("First job"));
            }
            Event::Job(job) => {
                let difficulty = job.target.difficulty();
                {
                    let mut live = self.ui.live();
                    live.panel.height = job.height;
                    live.panel.difficulty = difficulty;
                    live.panel.paused = None;
                }
                if let Some(step) = self.first_job.take() {
                    let at = if job.height == 0 {
                        String::new()
                    } else {
                        format!("height {} · ", term::group(job.height))
                    };
                    step.done_with(format!("{at}diff {}", term::group(difficulty)));
                    self.ui.start.ready(self.ui.started.elapsed());
                    self.ui.live().panel_on = true;
                }
            }
            Event::Paused(reason) => {
                if let Some(step) = &self.first_job {
                    step.handle().status(reason);
                }
                self.ui.live().panel.paused = Some((*reason).to_owned());
            }
            Event::Report(stats) => {
                let mut live = self.ui.live();
                live.panel.hashrate = stats.hashrate;
                live.panel.accepted = stats.accepted;
                live.panel.rejected = stats.rejected;
            }
            Event::Submitted {
                found,
                result,
                stats,
            } => {
                let flash = match result {
                    SubmitResult::Accepted { .. } if self.solo => Flash::Block(found.job.height),
                    SubmitResult::Accepted { .. } => Flash::Share,
                    SubmitResult::Stale(r) => Flash::Refused(format!("stale: {r}")),
                    SubmitResult::Rejected(r) | SubmitResult::Error(r) => Flash::Refused(r.clone()),
                };
                let mut live = self.ui.live();
                live.panel.accepted = stats.accepted;
                live.panel.rejected = stats.rejected;
                live.panel.flash = Some((flash, Instant::now() + panel::FLASH));
            }
            Event::LimitReached(limit) => {
                let what = match (self.solo, *limit) {
                    (true, 1) => "1 block".to_owned(),
                    (true, n) => format!("{n} blocks"),
                    (false, 1) => "1 share".to_owned(),
                    (false, n) => format!("{n} shares"),
                };
                self.ui.term.print(&format!(
                    "  {} {}",
                    self.ui.term.style().paint(Color::Green, "✔ limit reached"),
                    self.ui.term.style().paint(Color::Dim, &format!("· {what}"))
                ));
                self.ui.begin_stop();
            }
            Event::Stopped(stats) => {
                self.ui.begin_stop();
                let step = self.ui.live().threads_step.take();
                if let Some(step) = step {
                    let plural = if stats.threads == 1 {
                        "thread"
                    } else {
                        "threads"
                    };
                    step.done_with(format!(
                        "{} {plural} stopped · {} hashes",
                        stats.threads,
                        term::group(stats.total_hashes)
                    ));
                }
            }
        }
    }
}

/// The observer: the terminal look or the line output.
#[derive(Debug)]
enum Out {
    Plain(Output),
    Fancy(Box<Fancy>),
}

impl Out {
    /// The start-up step of the hashing threads (finished by `Event::Started`).
    fn threads_step(&mut self) {
        if let Self::Fancy(f) = self {
            f.threads_step = Some(f.ui.start.step("Threads"));
        }
    }
}

impl Observer for Out {
    fn event(&mut self, event: Event<'_>) {
        match self {
            Self::Plain(o) => o.event(event),
            Self::Fancy(f) => f.event(&event),
        }
    }
}

/// Prints events: C++-style text, or JSON lines (`--log-json`).
#[derive(Debug)]
struct Output {
    json: bool,
    solo: bool,
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The status fields every JSON line about progress carries.
fn stats_json(s: &xkr_miner::Stats) -> Value {
    json!({
        "hashrate": (s.hashrate * 100.0).round() / 100.0,
        "total_hashes": s.total_hashes,
        "accepted": s.accepted,
        "rejected": s.rejected,
        "stale": s.stale,
        "errors": s.errors,
        "mining": s.mining,
        "height": s.height,
        "difficulty": s.difficulty,
        "job_id": s.job_id,
        "threads": s.threads,
    })
}

impl Output {
    fn mode(&self) -> &'static str {
        if self.solo { "solo" } else { "pool" }
    }

    fn line(event: &str, mut fields: Value) {
        if let Some(obj) = fields.as_object_mut() {
            obj.insert("event".into(), json!(event));
            obj.insert("time".into(), json!(unix_time()));
        }
        println!("{fields}");
    }

    fn json_event(&self, event: &Event<'_>) {
        match event {
            Event::Started { threads, aes } => Self::line(
                "started",
                json!({
                    "mode": self.mode(),
                    "threads": threads,
                    "aes": aes.name(),
                    "version": VERSION,
                }),
            ),
            Event::Job(job) => Self::line(
                "job",
                json!({
                    "job_id": job.id,
                    "height": job.height,
                    "difficulty": job.target.difficulty(),
                    "major": job.major,
                    "algo": job.algo().name(),
                }),
            ),
            Event::Paused(reason) => Self::line("paused", json!({ "reason": reason })),
            Event::Report(stats) => Self::line("hashrate", stats_json(stats)),
            Event::Submitted {
                found,
                result,
                stats,
            } => {
                let mut fields = stats_json(stats);
                let (name, reason, block_hash) = match result {
                    SubmitResult::Accepted { block_hash } => (
                        "accepted",
                        None,
                        block_hash.as_ref().map(ToString::to_string),
                    ),
                    SubmitResult::Rejected(r) => ("rejected", Some(r.as_str()), None),
                    SubmitResult::Stale(r) => ("stale", Some(r.as_str()), None),
                    SubmitResult::Error(r) => ("error", Some(r.as_str()), None),
                };
                if let Some(obj) = fields.as_object_mut() {
                    obj.insert("job_id".into(), json!(found.job.id));
                    obj.insert("height".into(), json!(found.job.height));
                    obj.insert("nonce".into(), json!(found.nonce_hex()));
                    obj.insert("hash".into(), json!(found.hash_hex()));
                    obj.insert("block".into(), json!(found.job.target.is_block()));
                    if let Some(r) = reason {
                        obj.insert("reason".into(), json!(r));
                    }
                    if let Some(h) = block_hash {
                        obj.insert("block_hash".into(), json!(h));
                    }
                }
                Self::line(name, fields);
            }
            Event::LimitReached(limit) => Self::line("limit", json!({ "limit": limit })),
            Event::Stopped(stats) => Self::line("stopped", stats_json(stats)),
        }
    }

    fn text_event(&self, event: &Event<'_>) {
        match event {
            Event::Started { threads, aes } => {
                println!(
                    "xkr-miner {VERSION}: {threads} thread(s), {} AES",
                    aes.name()
                );
            }
            Event::Job(job) => {
                if self.solo {
                    println!(
                        "Started mining for difficulty of {}. Good luck! ;)",
                        job.target.difficulty()
                    );
                } else {
                    println!(
                        "New job {} from the pool: height {}, difficulty {}",
                        job.id,
                        job.height,
                        job.target.difficulty()
                    );
                }
            }
            Event::Paused(reason) => println!("Mining paused: {reason}"),
            Event::Report(stats) => {
                println!(
                    "\nMining at {} ({} accepted, {} rejected)\n",
                    format_speed(stats.hashrate),
                    stats.accepted,
                    stats.rejected
                );
            }
            Event::Submitted { result, stats, .. } => match (result, self.solo) {
                (SubmitResult::Accepted { block_hash }, true) => {
                    let hash = block_hash.as_ref().map_or_default(ToString::to_string);
                    println!("\nBlock found! Hash: {hash}\n");
                }
                (SubmitResult::Accepted { .. }, false) => {
                    println!(
                        "Share accepted ({}/{})",
                        stats.accepted,
                        stats.accepted + stats.rejected
                    );
                }
                (SubmitResult::Rejected(r) | SubmitResult::Error(r), true) => {
                    println!("Failed to submit block, possibly daemon offline or syncing? ({r})");
                }
                (SubmitResult::Rejected(r) | SubmitResult::Error(r), false) => {
                    println!("Share rejected: {r}");
                }
                (SubmitResult::Stale(r), _) => println!("Stale solution: {r}"),
            },
            Event::LimitReached(limit) => {
                if self.solo {
                    println!("Mined requested amount of blocks ({limit}). Quitting.");
                } else {
                    println!("Accepted the requested amount of shares ({limit}). Quitting.");
                }
            }
            Event::Stopped(stats) => {
                println!(
                    "Stopped: {} accepted, {} rejected, {} hashes",
                    stats.accepted, stats.rejected, stats.total_hashes
                );
            }
        }
    }
}

impl Observer for Output {
    fn event(&mut self, event: Event<'_>) {
        if self.json {
            self.json_event(&event);
        } else {
            self.text_event(&event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("xkr-miner").chain(args.iter().copied())).unwrap()
    }

    /// The C++ miner's option spellings parse unchanged.
    #[test]
    fn cpp_flags() {
        let c = cli(&[
            "--daemon-address=127.0.0.1:17899",
            "--address=SEKRx",
            "--threads=1",
            "--scan-time=1",
            "--limit=10",
            "--first-block-timestamp",
            "1700000000",
            "--block-timestamp-interval=-5",
        ]);
        assert_eq!(c.limit, 10);
        assert_eq!(c.threads, Some(1));
        assert_eq!(c.block_timestamp_interval, -5);
        assert_eq!(daemon_url(&c, 11898).unwrap(), "127.0.0.1:17899");
        let c = cli(&[
            "--daemon-host",
            "node.example",
            "--daemon-rpc-port",
            "12000",
        ]);
        assert_eq!(daemon_url(&c, 11898).unwrap(), "node.example:12000");
        let c = cli(&["--daemon-address", "node.example"]);
        assert_eq!(daemon_url(&c, 11899).unwrap(), "node.example:11899");
        let c = cli(&["--daemon-address", "https://node.example/rpc"]);
        assert_eq!(daemon_url(&c, 11898).unwrap(), "https://node.example/rpc");
        let c = cli(&["--daemon-address", "node:x"]);
        assert!(daemon_url(&c, 11898).is_err());
        let c = cli(&[]);
        assert_eq!(daemon_url(&c, 11898).unwrap(), "127.0.0.1:11898");
        assert_eq!(c.scan_time, 1);
        assert_eq!(c.pool_pass, "x");
    }

    /// One signal stops, the 2nd and 3rd never force a quit (the shared policy); the miner's
    /// note speaks of its threads.
    #[test]
    fn later_signals_never_force() {
        let actions: Vec<SignalAction> = (1..=3).map(signal_action).collect();
        assert_eq!(
            actions,
            [SignalAction::Stop, SignalAction::Note, SignalAction::Ignore]
        );
        assert!(STOPPING_NOTE.starts_with("stopping cleanly — please wait"));
        assert!(!STOPPING_NOTE.contains("database"));
    }

    #[test]
    fn final_line_totals() {
        let stats = xkr_miner::Stats {
            accepted: 12,
            rejected: 1,
            total_hashes: 1_234_567,
            ..xkr_miner::Stats::default()
        };
        assert_eq!(
            totals_text(&stats, 3 * 3_600 + 12 * 60),
            "· 12 accepted · 1 rejected · 1 234 567 hashes · uptime 3 h 12 min"
        );
    }

    #[test]
    fn pool_flags() {
        let c = cli(&[
            "--pool",
            "stratum+tcp://a:3333",
            "--pool",
            "stratum+ssl://b:443",
            "--pool-user",
            "SEKRuser.rig",
            "--rig-id",
            "rig1",
            "--log-json",
        ]);
        assert_eq!(c.pools.len(), 2);
        assert_eq!(c.rig_id.as_deref(), Some("rig1"));
        assert!(c.log_json);
    }
}
