//! `xkrd` — the Kryptokrona node.
//!
//! Threads and tasks (see `docs/node-architecture.md`):
//!
//! * a tokio multi-thread runtime (`xkr-io-*`) for connections, the P2P manager and the RPC;
//! * the global rayon pool (`xkr-verify-*`, all cores) for proof of work and ring signatures;
//! * one chain thread (`xkr-chain`) that applies verified batches and relayed blocks in order;
//! * in a terminal, a console thread (`xkr-console`) reading commands from stdin
//!   (`crate::console`) and a task printing the block and sync lines or the live panel
//!   (`crate::display`);
//! * with p2pool on (`cli::Components`), the built-in p2pool's tasks on the runtime
//!   (`crate::p2pool`; its CPU work on blocking threads), started once the node is synchronized;
//! * while stopping, the shutdown steps (`crate::stopping`).

mod bridge_check;
mod chain_worker;
mod cli;
mod clients;
mod console;
mod db_tool;
mod display;
mod downloader;
mod fee;
mod hyper;
mod miner;
mod p2p;
mod p2pool;
mod peerstore;
mod rng;
mod stopping;
mod term;

use std::cell::Cell;
use std::error::Error;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt, reload};
use xkr_chain::{Chain, ChainConfig, ChainError, unix_now};
use xkr_consensus::Network;
use xkr_rpc::{ClientStats, NetStatus, NodeInfo, RpcConfig, RpcState};
use xkr_storage::{OpenEvent, Store};
use xkr_txpool::{PoolConfig, TxPool};

use crate::chain_worker::{ChainEvent, ChainJob};
use crate::cli::{Args, default_data_dir};
use crate::console::{Console, ExitRequest, LogControl, NodeState};
use crate::p2p::{P2pConfig, P2pManager};
use crate::term::Term;

/// Version reported by `/getinfo` and `/status`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let mut args = Args::parse();
    let term = Term::stdout();
    match args.command.take() {
        Some(cli::Command::Db(cmd)) => {
            init_logging("info", false, None, &term);
            return match db_tool::run(&cmd) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("xkrd db: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(cli::Command::BridgeCheck { hyper_bridge_dir }) => {
            // Errors only: closing the bridge on purpose logs a warning.
            init_logging("error", false, None, &term);
            return match bridge_check::run(hyper_bridge_dir) {
                Ok(dir) => {
                    println!("bridge ok: {}", dir.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("xkrd bridge-check: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        None => {}
    }
    let log_file = match &args.log_file {
        Some(path) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("xkrd: cannot open the log file {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };
    let log = init_logging(&args.log_filter(), args.log_json, log_file, &term);
    let ui = Ui::new(&args, term, log);
    let started = ui.started;
    let result = run(args, ui);
    // Every exit shows the stop: an error before start-up still shows the stop title and line.
    if let Some(r) = stopping::reporter() {
        r.ensure_ended(started.elapsed().as_secs());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "xkrd failed");
            ExitCode::FAILURE
        }
    }
}

/// Logging (C++ `buildLoggerConfiguration` in `src/daemon/daemon.cpp`: a console logger and a
/// file logger at one global level): `RUST_LOG`, else `filter`; text through `term` or JSON on
/// stdout; also plain text to `file`. The filter can be switched later (`set_log`).
fn init_logging(
    filter: &str,
    json: bool,
    file: Option<std::fs::File>,
    term: &Term,
) -> Arc<LogControl> {
    let from_env = EnvFilter::try_from_default_env().ok();
    let (filter_layer, handle) =
        reload::Layer::new(from_env.unwrap_or_else(|| EnvFilter::new(filter)));
    let json_layer = json.then(|| fmt::layer().json().with_current_span(false));
    let text_layer = (!json).then(|| {
        fmt::layer()
            .with_target(false)
            .with_ansi(term.color())
            .with_writer(term.clone())
    });
    let file_layer = file.map(|f| {
        fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(Mutex::new(f))
    });
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(json_layer)
        .with(text_layer)
        .with(file_layer)
        .try_init();
    Arc::new(LogControl::new(move |f: &str| {
        let new = EnvFilter::builder().parse(f).map_err(|e| e.to_string())?;
        handle.reload(new).map_err(|e| e.to_string())
    }))
}

/// Whether the console runs: not `--no-console` and stdin is a terminal (C++ reads stdin
/// unless `--no-console`; without a terminal there is nobody to type).
fn console_wanted(args: &Args) -> bool {
    !args.no_console && std::io::stdin().is_terminal()
}

/// Terminal output, log control, the console and the start-up / stop control.
#[derive(Debug, Clone)]
struct Ui {
    term: Term,
    log: Arc<LogControl>,
    /// Whether the console runs.
    console: bool,
    /// Cancelled by Ctrl-C, SIGTERM and `exit`, from the header on (also during start-up).
    shutdown: CancellationToken,
    exit: ExitRequest,
    /// What the console answers from (the node's state once it is up).
    shared: Arc<console::Shared>,
    /// The start-up steps.
    startup: Arc<stopping::Reporter>,
    /// When xkrd started (start-up time, uptime).
    started: Instant,
}

impl Ui {
    /// Also installs the global shutdown reporter (same output mode as the start-up one).
    fn new(args: &Args, term: Term, log: Arc<LogControl>) -> Self {
        let mode = if args.log_json {
            stopping::Mode::Json
        } else if display::panel_mode(&term) {
            stopping::Mode::Fancy
        } else {
            stopping::Mode::Plain
        };
        let sink: Arc<dyn stopping::Sink> = Arc::new(term.clone());
        stopping::install(stopping::Reporter::new(mode, term.style(), sink.clone()));
        let startup =
            stopping::Reporter::with_kind(stopping::Kind::Start, mode, term.style(), sink);
        startup.set_subtitle(match args.network {
            cli::NetworkArg::Mainnet => "Mainnet",
            cli::NetworkArg::Testnet => "Testnet",
        });
        stopping::install_startup(startup.clone());
        let shutdown = CancellationToken::new();
        Self {
            console: console_wanted(args),
            exit: ExitRequest::new(shutdown.clone()),
            shutdown,
            shared: Arc::new(console::Shared::new()),
            startup,
            started: Instant::now(),
            term,
            log,
        }
    }

    /// Starts a start-up step (also what the console answers until the node is up).
    fn step(&self, name: &str) -> stopping::Step {
        self.shared.set_phase(name);
        self.startup.step(name)
    }
}

/// The database file in a data directory.
fn db_file(data_dir: &Path) -> PathBuf {
    data_dir.join("chain.redb")
}

/// `  Type help for the console commands, exit to stop the node.` under the header.
fn console_hint(style: term::Style) -> String {
    format!(
        "  Type {} for the console commands, {} to stop the node.",
        style.paint(term::Color::Bold, "help"),
        style.paint(term::Color::Bold, "exit"),
    )
}

fn run(args: Args, ui: Ui) -> Result<(), Box<dyn Error>> {
    let network: Network = args.network.into();
    if !args.log_json {
        ui.term.print(&term::header(ui.term.style()));
    }
    if args.no_hyper {
        tracing::info!("--no-hyper: Hyperswarm transport disabled, legacy Levin/TCP only");
    }
    if args.hugin_relay {
        tracing::debug!(
            "--hugin-relay: the Hugin relay is on without mode flags (--no-hugin turns it off)"
        );
    }

    let data_dir = args
        .data_dir
        .clone()
        .unwrap_or_else(|| default_data_dir(network));
    std::fs::create_dir_all(&data_dir)?;
    let db_path = db_file(&data_dir);
    if !args.log_json && ui.console {
        ui.term.print(&console_hint(ui.term.style()));
    }

    // From here on the console answers and Ctrl-C / SIGTERM / `exit` stop the node, also while
    // the database and the chain are still being opened.
    if ui.console {
        let console = Console::new(
            ui.shared.clone(),
            Some(ui.log.clone()),
            ui.exit.clone(),
            ui.term.style(),
            VERSION,
        );
        if let Err(e) = console::spawn(console, ui.term.clone()) {
            tracing::warn!(error = %e, "cannot start the console");
        }
    }
    let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
    let verify_threads = args.verify_thread_count();
    let io_threads = args.io_threads.unwrap_or(cores.min(4)).max(1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(io_threads)
        .thread_name("xkr-io")
        .enable_all()
        .build()?;
    runtime.spawn(stop_on_signals(ui.exit.clone()));
    ui.startup.begin();

    // What runs (`--node`, `--hugin`, `--p2pool`; none: all three). Flags that do not go
    // together, and modes without the node (they need a remote node), stop the start here.
    let step = ui.step("Mode");
    let checked = args
        .checked_components()
        .and_then(|c| c.not_available().map_or(Ok(c), Err));
    let components = match checked {
        Ok(c) => c,
        Err(e) => {
            step.fail(e.clone());
            return fail_start(&ui, Opened::Nothing, e.into());
        }
    };
    step.done_with(components.label());

    let step = ui.step("Verification");
    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(verify_threads)
        .thread_name(|i| format!("xkr-verify-{i}"))
        .build_global()
    {
        step.fail(e.to_string());
        return fail_start(&ui, Opened::Nothing, e.into());
    }
    step.done_with(format!("{verify_threads} threads"));
    tracing::info!(
        version = VERSION,
        ?network,
        data_dir = %data_dir.display(),
        verify_threads,
        io_threads,
        fast_sync = args.fast_sync,
        mode = %components.label(),
        "starting xkrd"
    );
    if ui.shutdown.is_cancelled() {
        return stop_during_startup(&ui, Opened::Nothing);
    }

    let step = ui.step("Storage");
    // redb checks the whole file after an unclean stop: explained once, shown live in the
    // step, and stopped (it runs again at the next start) when the node is asked to stop.
    // redb reports progress only at its phase boundaries, so the status shows how much of the
    // file this process has read (`stopping::check_status`), refreshed every second.
    let file_size = std::fs::metadata(&db_path).map_or(0, |m| m.len());
    let check_started = Rc::new(Cell::new(None::<Instant>));
    let check_done = Arc::new(AtomicBool::new(false));
    let observer = {
        let handle = step.handle();
        let check_started = check_started.clone();
        let check_done = check_done.clone();
        let shutdown = ui.shutdown.clone();
        let read_base = Rc::new(Cell::new(None::<u64>));
        move |event: &OpenEvent| {
            match event {
                OpenEvent::Repair { .. } => {
                    if check_started.get().is_none() {
                        check_started.set(Some(Instant::now()));
                        read_base.set(stopping::process_read_bytes());
                        for line in repair_notes(file_size) {
                            handle.note(&line);
                        }
                        spawn_check_sampler(
                            handle.clone(),
                            read_base.get(),
                            file_size,
                            check_done.clone(),
                        );
                    }
                    let read = read_since(read_base.get());
                    handle.status(&stopping::check_status(read, file_size));
                }
                OpenEvent::ResumingRepack { table } => {
                    handle.status(&format!("finishing an interrupted repack of {table}"));
                }
            }
            !shutdown.is_cancelled()
        }
    };
    let opened = open_storage(&args, network, &db_path, observer);
    check_done.store(true, Ordering::Relaxed);
    let (store, prune_state) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            if ui.shutdown.is_cancelled() && check_started.get().is_some() {
                step.fail("check stopped, it runs again at the next start");
                return stop_during_startup(&ui, Opened::Nothing);
            }
            step.fail(e.to_string());
            return fail_start(&ui, Opened::Nothing, e);
        }
    };
    let check_time = check_started.get().map(|t| t.elapsed());
    step.done_with(storage_detail(&db_path, &prune_state, check_time));
    if ui.shutdown.is_cancelled() {
        return stop_during_startup(&ui, Opened::Store(store));
    }
    if args.explorer && (prune_state.seed.is_stripe() || prune_state.is_pruned()) {
        return fail_start(
            &ui,
            Opened::Store(store),
            "--explorer needs the whole chain and this database is pruned: start without \
             --explorer, or sync a full database in another --data-dir"
                .into(),
        );
    }

    let step = ui.step("Transaction pool");
    let pool = Arc::new(TxPool::new(PoolConfig::default()));
    step.done_with("in memory");
    let mut chain_cfg = ChainConfig::new(network);
    chain_cfg.fast_sync = args.fast_sync;
    chain_cfg.durable_interval = Duration::from_secs(args.db_durable_secs.max(1));
    chain_cfg.max_reorg_depth = max_reorg_depth(
        args.max_reorg_depth,
        prune_state.seed.is_stripe() || prune_state.is_pruned(),
    );
    if let Some(h) = args.fork13_height {
        if network != Network::Testnet {
            return fail_start(
                &ui,
                Opened::Store(store),
                "--fork13-height is only accepted with --network testnet \
                 (the mainnet fork height is a source constant, docs/forks.md)"
                    .into(),
            );
        }
        chain_cfg.fork13_height = (h != 0).then_some(h);
    }
    if let Some(h) = args.fork14_height {
        if network != Network::Testnet {
            return fail_start(
                &ui,
                Opened::Store(store),
                "--fork14-height is only accepted with --network testnet \
                 (the mainnet fork height is a source constant, docs/forks.md)"
                    .into(),
            );
        }
        chain_cfg.fork14_height = (h != 0).then_some(h);
    }
    if let Some(grace) = args.fork14_grace {
        if network != Network::Testnet {
            return fail_start(
                &ui,
                Opened::Store(store),
                "--fork14-grace is only accepted with --network testnet \
                 (the mainnet grace period is a source constant, docs/tx-v2.md)"
                    .into(),
            );
        }
        chain_cfg.fork14.grace_blocks = grace;
    }
    let step = ui.step("Chain");
    let chain = match Chain::open(store, chain_cfg, pool) {
        Ok(chain) => Arc::new(chain),
        Err(e) => {
            step.fail(e.to_string());
            // `Chain::open` closed the database it was given.
            return fail_start(&ui, Opened::Closed, e.into());
        }
    };
    step.done_with(chain_detail(&chain.tip()));
    if ui.shutdown.is_cancelled() {
        return stop_during_startup(&ui, Opened::Chain(&chain));
    }

    let (job_tx, job_rx) = mpsc::unbounded_channel::<ChainJob>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<ChainEvent>();
    let worker = match chain_worker::spawn(chain.clone(), job_rx, event_tx) {
        Ok(worker) => worker,
        Err(e) => return fail_start(&ui, Opened::Chain(&chain), e.into()),
    };

    let started = ui.started;
    let startup = ui.startup.clone();
    let result = runtime.block_on(async_main(
        args,
        network,
        data_dir,
        chain.clone(),
        job_tx,
        event_rx,
        ui,
    ));
    // An error in `async_main` can end start-up before it was ready; the stop steps follow
    // either way (their title comes with the first one).
    startup.abort();
    let step = stopping::step("remaining tasks");
    let at = Instant::now();
    runtime.shutdown_timeout(RUNTIME_STOP);
    if at.elapsed() >= RUNTIME_STOP {
        step.timed_out(RUNTIME_STOP);
    } else {
        step.done();
    }
    // bare-kit's process-wide state, once the Bare workers are gone (nothing to do if the
    // Hyperswarm transport never started a worker).
    if !xkr_p2p_hyper::bare::teardown(BARE_TEARDOWN) {
        tracing::warn!("bare-kit did not finish tearing down in time");
    }
    let step = stopping::step("chain thread");
    if worker.join().is_err() {
        tracing::error!("the chain thread panicked");
        step.fail("panicked");
    } else {
        step.done_with("sync stopped, last batch stored");
    }
    // Closed here, not when the last handle goes: tasks or threads that outlive the stop
    // still hold the chain, and a process that exits with the file open makes the next start
    // check the whole file. Late callers get `Closed` errors from here on.
    let step = stopping::step("database");
    let height = chain.tip().height;
    let closed = chain.close();
    database_step_result(step, height, &closed);
    if let Some(r) = stopping::reporter() {
        r.end(Some(height), started.elapsed().as_secs());
    }
    tracing::info!(height, "xkrd stopped");
    closed?;
    result
}

/// The "database" stop step after [`Chain::close`].
fn database_step_result(step: stopping::Step, height: u64, closed: &Result<(), ChainError>) {
    match closed {
        Ok(()) => step.done_with(format!("closed at height {}", term::group(height))),
        Err(e) => step.fail(format!("close failed: {e}")),
    }
}

/// Opens the database, installs the shipped dictionary on a fresh one and sets up pruning.
/// `observer` sees redb's check after an unclean stop and a resumed repack
/// (`Store::open_observed`; returning `false` stops the check).
fn open_storage(
    args: &Args,
    network: Network,
    db_path: &Path,
    observer: impl Fn(&OpenEvent) -> bool + 'static,
) -> Result<(Store, xkr_storage::PruneState), Box<dyn Error>> {
    let store = Store::open_observed(db_path, args.db_cache_mb.max(16) * 1024 * 1024, observer)?;
    // Fresh databases start with the dictionary shipped in the binary (identical chunks on
    // every node, `crates/xkr-storage/dicts/README.md`).
    let net_name = match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    };
    if let Some(dict) = xkr_storage::builtin_dictionary(net_name)
        && store.install_dictionary_if_fresh(dict)?
    {
        tracing::info!(bytes = dict.len(), "installed the shipped zstd dictionary");
    }
    setup_pruning(&store, args.prune, args.prune_stripes)?;
    let prune_state = store.reader()?.prune_state();
    Ok((store, prune_state))
}

/// Bytes read by this process since `base` (`stopping::process_read_bytes`).
fn read_since(base: Option<u64>) -> Option<u64> {
    Some(stopping::process_read_bytes()?.saturating_sub(base?))
}

/// Refreshes the Storage step's status once a second while redb checks the file (until
/// `done`).
fn spawn_check_sampler(
    handle: stopping::StepHandle,
    base: Option<u64>,
    file_size: u64,
    done: Arc<AtomicBool>,
) {
    let _ = std::thread::Builder::new()
        .name("xkr-check".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                if done.load(Ordering::Relaxed) {
                    break;
                }
                handle.status(&stopping::check_status(read_since(base), file_size));
            }
        });
}

/// Why the Storage step takes long after an unclean stop (shown once, above its spinner).
fn repair_notes(file_size: u64) -> [String; 2] {
    [
        "The node did not stop cleanly last time, so the database is being checked.".to_owned(),
        format!(
            "This reads the whole {} file and can take a few minutes; it does not need the \
             network.",
            term::bytes(file_size)
        ),
    ]
}

/// `opened 9.30 GB (full)` / `(pruned, stripe 2/4 (0x0402))` / `(pruned, recent blocks only)`,
/// plus `, checked after an unclean stop · 3 min 12 s` when redb checked the file.
fn storage_detail(
    db_path: &Path,
    state: &xkr_storage::PruneState,
    check_time: Option<Duration>,
) -> String {
    let size = std::fs::metadata(db_path).map_or(0, |m| m.len());
    let kind = if state.seed.is_stripe() {
        format!("pruned, {}", state.seed)
    } else if state.pruned_below > 0 {
        "pruned, recent blocks only".to_owned()
    } else {
        "full".to_owned()
    };
    let checked = check_time.map_or_else(String::new, |t| {
        format!(
            ", checked after an unclean stop · {}",
            term::duration_min_sec(t.as_secs())
        )
    });
    format!("opened {} ({kind}{checked})", term::bytes(size))
}

/// `height 2 620 300 · tip 3f1c9a0b2e4d…`.
fn chain_detail(tip: &xkr_chain::TipInfo) -> String {
    let hash = tip.hash.to_string();
    format!(
        "height {} · tip {}…",
        term::group(tip.height),
        &hash[..hash.len().min(12)]
    )
}

/// What start-up had opened when it ended early.
enum Opened<'a> {
    /// Nothing that needs closing.
    Nothing,
    /// The database was opened and has been closed again (`Chain::open` failed).
    Closed,
    /// The database is open.
    Store(Store),
    /// The chain is open.
    Chain(&'a Chain),
}

/// Start-up ended early (asked to stop, or an error): the start-up steps stop and the
/// "Stopping the node" steps list what was opened, and only that, then the final line.
fn stop_during_startup(ui: &Ui, opened: Opened<'_>) -> Result<(), Box<dyn Error>> {
    ui.startup.abort();
    close_opened(stopping::reporter(), opened, ui.started.elapsed().as_secs()).map(drop)
}

/// A start-up error: what was opened is closed and listed ([`stop_during_startup`]), then
/// the error is returned.
fn fail_start(ui: &Ui, opened: Opened<'_>, error: Box<dyn Error>) -> Result<(), Box<dyn Error>> {
    if let Err(e) = stop_during_startup(ui, opened) {
        tracing::error!(error = %e, "closing after the failed start");
    }
    Err(error)
}

/// The stop steps of [`Opened`] on `reporter` (title, one step per open part, final line);
/// the top block index when the chain was open.
fn close_opened(
    reporter: Option<&Arc<stopping::Reporter>>,
    opened: Opened<'_>,
    uptime_secs: u64,
) -> Result<Option<u64>, Box<dyn Error>> {
    if let Some(r) = reporter {
        r.begin();
    }
    let (height, result) = match opened {
        Opened::Nothing => (None, Ok(())),
        Opened::Closed => {
            stopping::step_on(reporter, "database").done_with("closed");
            (None, Ok(()))
        }
        Opened::Store(store) => {
            let step = stopping::step_on(reporter, "database");
            let closed = store.close();
            match &closed {
                Ok(()) => step.done_with("closed"),
                Err(e) => step.fail(format!("close failed: {e}")),
            }
            (None, closed.map_err(ChainError::from))
        }
        Opened::Chain(chain) => {
            let step = stopping::step_on(reporter, "database");
            let height = chain.tip().height;
            let closed = chain.close();
            database_step_result(step, height, &closed);
            (Some(height), closed)
        }
    };
    if let Some(r) = reporter {
        r.end(height, uptime_secs);
    }
    if let Some(height) = height {
        tracing::info!(height, "xkrd stopped");
    } else {
        tracing::info!("xkrd stopped");
    }
    result?;
    Ok(height)
}

/// Ctrl-C / SIGTERM / SIGHUP: the first stops the node cleanly (also during start-up; the
/// console's `exit` counts as the first too). Later ones never cut the stop short, so the
/// database is always closed: the next one shows `stopping::STOPPING_NOTE` once, the rest do
/// nothing (only SIGKILL ends the process before the stop is done).
async fn stop_on_signals(exit: ExitRequest) {
    let mut count = 0_usize;
    loop {
        let signal = wait_for_signal().await;
        count = next_signal_count(count, exit.requested());
        match stopping::signal_action(count) {
            stopping::SignalAction::Stop => {
                exit.request();
                tracing::info!(signal = signal.name(), "shutdown requested");
            }
            stopping::SignalAction::Note => {
                if let Some(r) = stopping::reporter() {
                    r.say(stopping::STOPPING_NOTE);
                }
            }
            stopping::SignalAction::Ignore => {}
        }
    }
}

/// The number of a stop signal: one more than before, and at least the second when the node
/// already stops (the console's `exit` started it).
fn next_signal_count(before: usize, stopping: bool) -> usize {
    if stopping {
        before.max(1) + 1
    } else {
        before + 1
    }
}

/// How long the runtime's remaining tasks get after `async_main` (`shutdown_timeout`).
const RUNTIME_STOP: Duration = Duration::from_secs(5);
/// How long bare-kit gets to finish tearing down its worklets at exit (`bare::teardown`).
const BARE_TEARDOWN: Duration = Duration::from_secs(2);
/// How long the Hyperswarm transport gets to unannounce and close.
const HYPER_STOP: Duration = Duration::from_secs(10);
/// How long the RPC server gets to finish its requests and close.
const RPC_STOP: Duration = Duration::from_secs(10);
/// How long the built-in miner's threads get to stop.
const MINER_STOP: Duration = Duration::from_secs(10);
/// How long the built-in p2pool gets to stop (its stratum, P2P and sidechain steps).
const P2POOL_STOP: Duration = Duration::from_secs(15);

/// Stripe pruning at start (`docs/storage.md` "Stripe pruning"). A database with a seed keeps
/// it (and the chain thread keeps pruning with it, flag or not); `--prune` on a database without
/// one picks a random stripe of `--prune-stripes` (default 4) and stores it, and the chain thread
/// prunes the existing chain down to the bound before its first job.
fn setup_pruning(store: &Store, prune: bool, stripes: Option<u8>) -> Result<(), Box<dyn Error>> {
    let state = store.reader()?.prune_state();
    if state.seed.is_stripe() {
        tracing::info!(
            seed = %state.seed,
            pruned_below = state.pruned_below,
            "stripe-pruned database: keeping its stored pruning seed"
        );
        if stripes.is_some_and(|n| n != state.seed.stripes()) {
            tracing::warn!(
                "--prune-stripes is ignored: the database already has a pruning seed (resync to change it)"
            );
        }
        return Ok(());
    }
    if state.pruned_below > 0 {
        if prune {
            tracing::warn!(
                pruned_below = state.pruned_below,
                "--prune is ignored: the database was pruned completely (`xkrd db prune --all`) \
                 and cannot keep a stripe any more (resync to switch to stripe pruning)"
            );
        } else {
            tracing::info!(
                pruned_below = state.pruned_below,
                "completely pruned database: no old signatures to serve below that height \
                 (announcing the recent-blocks-only pruning seed)"
            );
        }
        return Ok(());
    }
    if !prune {
        return Ok(());
    }
    let n = stripes.unwrap_or(xkr_storage::DEFAULT_STRIPES);
    let seed = db_tool::pick_seed(n).ok_or("--prune-stripes must be 2..=8")?;
    store.set_pruning_seed(seed)?;
    tracing::info!(
        %seed,
        "stripe pruning enabled: keeping the signatures of one stripe below \
         min(last checkpoint, tip - 8192)"
    );
    Ok(())
}

/// Deepest reorg a pruning node can still carry out: the newest `2 * STRIPE_SPAN` blocks stay
/// whole (`Checkpoints::prune_bound`), less 1 024 blocks of slack for a tip that moved on since
/// the last prune step.
const MAX_PRUNED_REORG_DEPTH: u64 = 2 * xkr_storage::STRIPE_SPAN - 1_024;

/// `--max-reorg-depth`, clamped to [`MAX_PRUNED_REORG_DEPTH`] (with a warning) on a database
/// that prunes or was pruned: a deeper reorg would pop pruned blocks and be refused anyway.
fn max_reorg_depth(requested: u64, pruning: bool) -> u64 {
    if pruning && requested > MAX_PRUNED_REORG_DEPTH {
        tracing::warn!(
            requested,
            max = MAX_PRUNED_REORG_DEPTH,
            "--max-reorg-depth is above what a pruned database can reorganise; clamped"
        );
        return MAX_PRUNED_REORG_DEPTH;
    }
    requested
}

async fn async_main(
    args: Args,
    network: Network,
    data_dir: PathBuf,
    chain: Arc<Chain>,
    jobs: mpsc::UnboundedSender<ChainJob>,
    chain_events: mpsc::UnboundedReceiver<ChainEvent>,
    ui: Ui,
) -> Result<(), Box<dyn Error>> {
    // Same value `run` used to size the rayon pool; the P2P semaphores are derived from it.
    let verify_threads = rayon::current_num_threads().max(1);
    let params = network.params();
    let db_path = db_file(&data_dir);
    let start_time = unix_now();
    // SIGINT/SIGTERM and the console's `exit` cancel it (`run`, `stop_on_signals`).
    let shutdown = ui.shutdown.clone();

    let (status_tx, status_rx) = watch::channel(Arc::new(NetStatus::default()));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // Built-in miner (`--mine`, `--mine-p2pool`, console `start_mining`): solo blocks go the
    // `submitblock` way, p2pool shares through the built-in p2pool.
    let (mining_tx, mining_rx) = watch::channel(Arc::new(xkr_rpc::MiningStatus::default()));
    let solo_mode = match &args.mine {
        Some(address) => {
            let miner = xkr_rpc::parse_miner_address(address)
                .ok_or("--mine: not a valid SEKR/Xkr address")?;
            Some(miner::MineMode::Solo {
                address: address.clone(),
                miner,
            })
        }
        None => None,
    };
    let mine_threads = args
        .mine_threads
        .unwrap_or_else(miner::default_threads)
        .max(1);

    // Hyperswarm transport settings, built before the RPC so `/status` can show the Hugin
    // relay counters (`hugin`, null with the relay off). The relay hands out jobs only while
    // the node is synchronized (`net`), and submits its shares under the fee address.
    let mut hyper_cfg = (!args.no_hyper).then(|| hyper::HyperConfig {
        net: Some(status_rx.clone()),
        ..hyper::HyperConfig::from_args(&args, network, &data_dir)
    });
    // `--private`: the secret file (created on the first start); a file that cannot be read
    // stops the start.
    let private_node = match hyper_cfg.as_mut().filter(|_| args.private) {
        Some(cfg) => {
            let node = hyper::PrivateNode::load(&data_dir)?;
            cfg.private = Some(node.clone());
            Some(node)
        }
        None => None,
    };
    // The built-in p2pool (`Args::p2pool_enabled`): its settings now (a wrong flag stops the
    // start), the service itself once the node is synchronized (`crate::p2pool`).
    let p2pool_settings = if args.p2pool_enabled() {
        let mut s = p2pool::P2poolSettings::from_args(&args, network, &data_dir)?;
        // A stratum address in use does not stop p2pool (it runs without its stratum server,
        // shown under the p2pool step); the Hugin relay then uses the public p2pool nodes.
        if !s.check_stratum()
            && args.hugin_p2pool.is_empty()
            && let Some(h) = hyper_cfg.as_mut().and_then(|c| c.hugin.as_mut())
        {
            h.p2pool_nodes = p2pool::public_relay_nodes();
        }
        Some(s)
    } else {
        None
    };
    // The built-in miner reaches the running p2pool through this link.
    let pool_link = p2pool_settings
        .as_ref()
        .map(|_| p2pool::PoolLink::default());
    let start_mode = match (solo_mode, &pool_link) {
        (Some(mode), _) => Some(mode),
        (None, Some(link)) if args.mine_p2pool => Some(miner::MineMode::P2pool(link.clone())),
        _ => None,
    };
    let miner_control = Arc::new(miner::MinerControl::new(
        chain.clone(),
        cmd_tx.clone(),
        status_rx.clone(),
        mining_tx,
        shutdown.clone(),
        pool_link.clone(),
    ));
    // The fee address, shared by the Hugin relay (its fee) and the built-in p2pool (its payout).
    let mut hugin_failed: Option<String> = None;
    let mut fee_failed: Option<String> = None;
    let mut fee_handle: Option<Arc<fee::FeeAddress>> = None;
    let hugin_wanted = hyper_cfg.as_ref().is_some_and(|c| c.hugin.is_some());
    if hugin_wanted || p2pool_settings.is_some() {
        match fee::FeeAddress::load(
            &fee::identity_file(&data_dir),
            network,
            args.hugin_payout.as_deref(),
        ) {
            Ok(f) => {
                if let Some(h) = hyper_cfg.as_mut().and_then(|c| c.hugin.as_mut()) {
                    h.fee = Some(f.clone());
                }
                fee_handle = Some(f);
            }
            // `--hugin-payout <address>` that is not one: the operator has to fix the command
            // line.
            Err(e) if e.starts_with("--hugin-payout") => return Err(e.into()),
            Err(e) => {
                if let Some(cfg) = hyper_cfg.as_mut().filter(|c| c.hugin.is_some()) {
                    tracing::error!(error = %e, "Hugin relay disabled");
                    cfg.hugin = None;
                    hugin_failed = Some(e.clone());
                }
                fee_failed = Some(e);
            }
        }
    }
    // The p2pool sidechain on the Hyperswarm network: its topic for the transport, the
    // connections and the "announce now" switch for p2pool.
    let mut p2pool_hyper = None;
    if let (Some(s), Some(cfg)) = (&p2pool_settings, hyper_cfg.as_mut()) {
        let (conns_tx, conns_rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = watch::channel(false);
        cfg.p2pool = Some(hyper::P2poolTopic {
            topic: hyper::p2pool_topic(network, &s.consensus_id),
            conns: conns_tx,
            ready: ready_rx,
        });
        p2pool_hyper = Some(p2pool::HyperLink {
            conns: conns_rx,
            ready: ready_tx,
        });
    }
    let (p2pool_status_tx, p2pool_status) = p2pool_settings
        .as_ref()
        .map(|_| watch::channel(Arc::new(xkr_rpc::P2poolStatus::default())))
        .unzip();
    let hugin_stats = hyper_cfg
        .as_ref()
        .filter(|c| c.hugin.is_some())
        .map(|c| c.hugin_stats.clone());
    let hugin_on = hugin_stats.is_some();
    // The p2pool node the relay tries first, for the start-up step.
    let hugin_p2pool = hyper_cfg
        .as_ref()
        .and_then(|c| c.hugin.as_ref())
        .and_then(|h| h.p2pool_nodes.first().cloned());

    // The clients topic (crate::clients): served with the Hyperswarm transport, unless the node
    // only dials (`--hyper-client-only` without `--private`).
    let client_stats = hyper_cfg
        .as_ref()
        .filter(|_| !args.hyper_client_only || args.private)
        .map(|_| Arc::new(ClientStats::default()));

    let (status_rx_display, mining_rx_display) = (status_rx.clone(), mining_rx.clone());
    let status_rx_p2pool = status_rx.clone();
    // The console answers from the node from now on.
    ui.shared.ready(NodeState {
        chain: chain.clone(),
        net: status_rx.clone(),
        mining: mining_rx.clone(),
        hugin: hugin_stats.clone(),
        fee: fee_handle.clone(),
        p2pool: p2pool_status.clone(),
        clients: client_stats.clone(),
        miner: Some(miner_control.clone()),
        hyper: !args.no_hyper,
        private: private_node.clone(),
        version: VERSION.to_owned(),
        start_time,
        db_path: db_path.clone(),
    });
    if shutdown.is_cancelled() {
        // Stopped during start-up: `run` stops the chain thread and flushes the database.
        ui.startup.abort();
        return Ok(());
    }

    // Blocks found by p2pool go the `submitblock` way (P2P manager -> chain thread -> relay).
    let p2pool_commands = p2pool_settings.as_ref().map(|_| cmd_tx.clone());
    let rpc_port = args.rpc_bind_port.unwrap_or(params.rpc_port);
    let rpc_bind = SocketAddr::new(args.rpc_bind_ip, rpc_port);
    // The node's RPC state: the HTTP server's and the clients topic's.
    let state = Arc::new(RpcState {
        chain: chain.clone(),
        net: status_rx,
        commands: cmd_tx,
        mining: mining_rx,
        hugin: hugin_stats,
        p2pool: p2pool_status,
        clients: client_stats.clone(),
        explorer: args.explorer,
        info: NodeInfo {
            version: VERSION.to_owned(),
            start_time,
            // `/status` names the data directory on a loopback RPC only.
            data_dir: if args.rpc_bind_ip.is_loopback() {
                data_dir.clone()
            } else {
                PathBuf::new()
            },
            db_path,
            fee_address: args.fee_address.clone(),
            fee_amount: args.fee_amount,
        },
    });
    if let (Some(stats), Some(cfg)) = (&client_stats, hyper_cfg.as_mut()) {
        cfg.clients = Some(clients::ClientsServer::new(
            state.clone(),
            stats.clone(),
            network,
            args.max_clients.max(1),
            cfg.p2pool.as_ref().map(|p| p.ready.clone()),
        ));
    }
    let step = ui.step("RPC server");
    let rpc_task = if rpc_port == 0 {
        tracing::info!("RPC server disabled");
        drop(state);
        step.done_with("off");
        None
    } else {
        if args.rpc_bind_ip.is_unspecified() {
            tracing::warn!(
                bind = %args.rpc_bind_ip,
                port = rpc_port,
                "the RPC listens on every interface and has no authentication: anyone who can \
                 reach this port can submit transactions and blocks and read the node; bind \
                 127.0.0.1 or firewall the port unless this is intended"
            );
        }
        let cfg = rpc_config(&args, rpc_port);
        let token = shutdown.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = xkr_rpc::serve(cfg, state, token.clone().cancelled_owned()).await {
                tracing::error!(error = %e, "RPC server failed");
                token.cancel();
            }
        });
        step.done_with(if args.explorer {
            format!("{rpc_bind}, block explorer")
        } else {
            rpc_bind.to_string()
        });
        Some(task)
    };

    let p2p_port = args.p2p_bind_port.unwrap_or(params.p2p_port);
    let listen = (p2p_port != 0).then(|| SocketAddr::new(args.p2p_bind_ip, p2p_port));
    let my_port = if args.hide_my_port || p2p_port == 0 {
        0
    } else {
        u32::from(args.p2p_external_port.unwrap_or(p2p_port))
    };
    let seeds = if args.seed_node.is_empty() {
        params.seed_nodes.iter().map(|s| (*s).to_owned()).collect()
    } else {
        args.seed_node.clone()
    };
    let p2p_cfg = P2pConfig {
        network,
        listen,
        my_port,
        out_peers: args.out_peers,
        in_peers: args.in_peers,
        add_peers: args.add_peer.clone(),
        exclusive: args.add_exclusive_node.clone(),
        seeds,
        allow_local_ip: args.allow_local_ip || network == Network::Testnet,
        max_conns_per_ip: args.max_conns_per_ip.max(1),
        ban_time: Duration::from_secs(args.ban_time),
        state_dir: Some(data_dir.clone()),
        auto_maintenance: !args.no_db_maintenance,
        verify_threads,
    };
    if let Some(mode) = start_mode {
        let step = ui.step("Miner");
        let detail = format!("{mine_threads} threads · {}", mode.name());
        if matches!(mode, miner::MineMode::P2pool(_))
            && let Some(fee) = &fee_handle
        {
            let lines = miner::p2pool_payout_lines(&fee.get());
            if args.log_json {
                tracing::info!("{}", lines.join(" "));
            } else {
                for line in &lines {
                    step.handle().note(line);
                }
            }
        }
        let cfg = miner::MinerConfig {
            mode,
            threads: mine_threads,
        };
        match miner_control.start(cfg) {
            Ok(()) => step.done_with(detail),
            Err(e) => {
                step.fail(e.clone());
                return Err(e.into());
            }
        }
    }
    let step = ui.step("Legacy P2P");
    let manager = P2pManager::new(p2p_cfg, chain.clone(), jobs, status_tx, shutdown.clone());
    step.done_with(listen.map_or_else(|| "not listening".to_owned(), |a| format!("listening {a}")));
    // Hyperswarm transport (crate::hyper): its connections go through `P2pHandle::add_stream`.
    let hyper_task = hyper_cfg.map(|cfg| {
        let step = ui.step("Hyperswarm");
        let task = tokio::spawn(hyper::run(cfg, manager.handle(), shutdown.clone()));
        step.done_with("Bare workers started");
        task
    });
    if private_node.is_some() {
        ui.step("Private node")
            .done_with("peers only by invite (console: invite)");
    }
    if args.no_hyper && !args.no_hugin && !args.mode_given() {
        ui.step("Hugin relay")
            .done_with("off: it needs the Hyperswarm transport (--no-hyper)");
    } else if let Some(e) = hugin_failed {
        ui.step("Hugin relay").fail(e);
    } else if let (true, Some(fee)) = (hugin_on, &fee_handle) {
        ui.step("Hugin relay").done_with(hugin_step_detail(
            args.hugin_max_clients,
            &fee.get(),
            hugin_p2pool.as_deref(),
        ));
    }
    // The built-in p2pool: shown now, started by its task once the node is synchronized and
    // stopped in the stopping steps (`p2pool_stop`).
    let p2pool_stop = CancellationToken::new();
    let mut p2pool_task = None;
    if let Some(settings) = p2pool_settings {
        let step = ui.step("p2pool");
        if let (Some(fee), Some(status), Some(commands)) =
            (&fee_handle, p2pool_status_tx, p2pool_commands)
        {
            step.handle()
                .note("p2pool starts once the node is synchronized");
            if let Some(reason) = &settings.stratum_off {
                if args.log_json {
                    tracing::warn!(reason, "p2pool: stratum off, running without it");
                } else {
                    step.handle()
                        .note(&p2pool::stratum_off_line(ui.term.style(), reason));
                }
            }
            step.done_with(p2pool::step_detail(&settings, &fee.get()));
            let node = p2pool::P2poolNode {
                chain: chain.clone(),
                commands,
                net: status_rx_p2pool,
                fee: fee.subscribe(),
                status,
                hyper: p2pool_hyper,
                term: (!args.log_json).then(|| ui.term.clone()),
                link: pool_link.clone().unwrap_or_default(),
            };
            p2pool_task = Some(tokio::spawn(p2pool::run(
                settings,
                node,
                p2pool_stop.clone(),
            )));
        } else {
            let reason = fee_failed.unwrap_or_else(|| "no fee address".to_owned());
            if let Some(link) = &pool_link {
                link.fail(format!("p2pool not started: {reason}"));
            }
            step.fail(reason);
        }
    }
    // The donation: asked once (for the relay and p2pool together), until the operator answers;
    // the notice shows on every start while donating.
    let interactive = ui.console && !args.log_json;
    let pays = (hugin_on && fee_handle.is_some(), p2pool_task.is_some());
    let mut ask = false;
    let mut notice = false;
    if let Some(fee) = &fee_handle
        && (pays.0 || pays.1)
        && fee.get().donation
    {
        if interactive {
            notice = true;
            ask = fee.should_ask();
        } else {
            tracing::warn!(
                "{}. {}",
                fee::donation_notice(pays.0, pays.1),
                fee::HOW_TO_CHANGE
            );
        }
    }
    ui.startup.ready(ui.started.elapsed());
    // The donation notice and the question together, under "Node running".
    if notice {
        let style = ui.term.style();
        ui.term.print("");
        ui.term
            .print(&format!("  {}.", fee::donation_notice(pays.0, pays.1)));
        if let (true, Some(fee)) = (ask, &fee_handle) {
            for line in fee::QUESTION {
                ui.term
                    .print(&format!("  {}", style.paint(term::Color::Bold, line)));
            }
            fee.ask();
        } else {
            ui.term.print(&format!(
                "  {}",
                style.paint(term::Color::Dim, fee::HOW_TO_CHANGE)
            ));
        }
    }
    // The block / sync lines or the live panel (not with `--log-json`).
    let display_task = (!args.log_json).then(|| {
        tokio::spawn(display::run(
            ui.term.clone(),
            chain,
            status_rx_display,
            mining_rx_display,
            shutdown.clone(),
        ))
    });
    manager.run(chain_events, cmd_rx).await;

    shutdown.cancel();
    if let Some(task) = display_task {
        let _ = task.await;
    }
    // p2pool first: its stratum, P2P (with the Hyperswarm connections it holds) and sidechain
    // steps come from its task.
    if let Some(task) = p2pool_task {
        p2pool_stop.cancel();
        if tokio::time::timeout(P2POOL_STOP, task).await.is_err() {
            stopping::step("p2pool").timed_out(P2POOL_STOP);
        }
    }
    if let Some(task) = hyper_task {
        // Unannounce, close the connections, let the Bare workers leave the DHT (bounded inside).
        let step = stopping::step("Hyperswarm transport");
        match tokio::time::timeout(HYPER_STOP, task).await {
            Ok(_) => step.done(),
            Err(_) => step.timed_out(HYPER_STOP),
        }
    }
    // Every step is bounded, so the stop always reaches the chain thread and the database
    // flush in `run`, which come last.
    if let Some(task) = rpc_task {
        let step = stopping::step("RPC server");
        match tokio::time::timeout(RPC_STOP, task).await {
            Ok(Ok(())) => step.done_with(format!("{rpc_bind} closed")),
            Ok(Err(e)) => step.fail(e.to_string()),
            Err(_) => step.timed_out(RPC_STOP),
        }
    }
    if let Some(thread) = miner_control.take_thread() {
        let step = stopping::step("miner");
        let join = tokio::task::spawn_blocking(move || thread.join());
        match tokio::time::timeout(MINER_STOP, join).await {
            Ok(Ok(Ok(()))) => step.done_with("threads joined"),
            Ok(Ok(Err(_))) => step.fail("a mining thread panicked"),
            Ok(Err(e)) => step.fail(e.to_string()),
            Err(_) => step.timed_out(MINER_STOP),
        }
    }
    Ok(())
}

/// The detail of the "Hugin relay" start-up step: the app limit, the fee and the p2pool node
/// tried first (`p2pool`, shortened to [`P2POOL_DETAIL_MAX`] characters):
/// `max 200 apps · fee to nilsjr · p2pool 127.0.0.1:3333`,
/// `max 200 apps · fee SEKReSw2cP…kcBJZGc · p2pool 127.0.0.1:3333`,
/// `max 200 apps · no fee address · no p2pool`.
fn hugin_step_detail(max_apps: usize, fee: &fee::Fee, p2pool: Option<&str>) -> String {
    let fee = match &fee.address {
        None => "no fee address".to_owned(),
        Some(_) if fee.donation => format!("fee to {}", fee::DEFAULT_FEE_NAME),
        Some(a) => format!("fee {}", fee::short_address(a)),
    };
    let pool = p2pool.map_or_else(
        || "no p2pool".to_owned(),
        |p| {
            let p = p.strip_prefix("stratum+tcp://").unwrap_or(p);
            if p.chars().count() > P2POOL_DETAIL_MAX {
                let head: String = p.chars().take(P2POOL_DETAIL_MAX - 1).collect();
                format!("p2pool {head}…")
            } else {
                format!("p2pool {p}")
            }
        },
    );
    format!("max {max_apps} apps · {fee} · {pool}")
}

/// Longest p2pool node name in the "Hugin relay" start-up step.
const P2POOL_DETAIL_MAX: usize = 24;

/// The signals that stop the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum StopSignal {
    /// SIGINT (Ctrl-C), everywhere.
    Interrupt,
    /// SIGTERM.
    Terminate,
    /// SIGHUP: the terminal went away (its window was closed); stops cleanly like SIGTERM.
    Hangup,
}

impl StopSignal {
    const fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
            Self::Hangup => "SIGHUP",
        }
    }
}

/// The Unix signals besides SIGINT that stop the node (POSIX numbers).
#[cfg(unix)]
const UNIX_STOP_SIGNALS: [(i32, StopSignal); 2] =
    [(15, StopSignal::Terminate), (1, StopSignal::Hangup)];

/// SIGINT (Ctrl-C) everywhere, SIGTERM and SIGHUP on Unix.
async fn wait_for_signal() -> StopSignal {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let [mut first, mut second] =
            UNIX_STOP_SIGNALS.map(|(raw, stop)| (signal(SignalKind::from_raw(raw)).ok(), stop));
        tokio::select! {
            _ = tokio::signal::ctrl_c() => StopSignal::Interrupt,
            Some(stop) = next_signal(&mut first) => stop,
            Some(stop) = next_signal(&mut second) => stop,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        StopSignal::Interrupt
    }
}

/// The next delivery of a Unix signal stream (never, without one).
#[cfg(unix)]
async fn next_signal(
    stream: &mut (Option<tokio::signal::unix::Signal>, StopSignal),
) -> Option<StopSignal> {
    let stop = stream.1;
    match &mut stream.0 {
        Some(s) => s.recv().await.map(|()| stop),
        None => std::future::pending().await,
    }
}

/// The RPC server settings of the command line.
fn rpc_config(args: &Args, port: u16) -> RpcConfig {
    RpcConfig {
        bind: SocketAddr::new(args.rpc_bind_ip, port),
        cors: args.enable_cors.clone(),
        allowed_hosts: args.rpc_allowed_hosts.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_allowed_hosts_reach_the_rpc_config() {
        let plain = Args::try_parse_from(["xkrd"]).unwrap();
        let cfg = rpc_config(&plain, 11898);
        assert_eq!(cfg.allowed_hosts.len(), 0);
        assert_eq!(cfg.bind, "127.0.0.1:11898".parse().unwrap());

        let named = Args::try_parse_from([
            "xkrd",
            "--rpc-bind-ip",
            "0.0.0.0",
            "--rpc-allowed-host",
            "node.example",
            "--rpc-allowed-host",
            "Wallet.LAN",
        ])
        .unwrap();
        let cfg = rpc_config(&named, 11898);
        assert_eq!(cfg.allowed_hosts, ["node.example", "Wallet.LAN"]);
        let policy = xkr_rpc::AccessPolicy::from_config(&cfg);
        assert!(policy.host_allowed("node.example:11898"));
        assert!(policy.host_allowed("wallet.lan"));
        assert!(!policy.host_allowed("other.example"));
    }

    #[test]
    fn stop_signals() {
        #[cfg(unix)]
        {
            use tokio::signal::unix::SignalKind;
            assert_eq!(
                UNIX_STOP_SIGNALS,
                [
                    (
                        SignalKind::terminate().as_raw_value(),
                        StopSignal::Terminate
                    ),
                    (SignalKind::hangup().as_raw_value(), StopSignal::Hangup),
                ]
            );
        }
        assert_eq!(StopSignal::Hangup.name(), "SIGHUP");
        // one signal stops cleanly; the 2nd and 3rd (of any kind) never force a quit
        let mut n = 0;
        let mut actions = Vec::new();
        for stopping in [false, true, true] {
            n = next_signal_count(n, stopping);
            actions.push(stopping::signal_action(n));
        }
        assert_eq!(
            actions,
            [
                stopping::SignalAction::Stop,
                stopping::SignalAction::Note,
                stopping::SignalAction::Ignore
            ]
        );
        // after the console's `exit`, the first signal is already the second
        assert_eq!(
            stopping::signal_action(next_signal_count(0, true)),
            stopping::SignalAction::Note
        );
    }

    #[test]
    fn storage_detail_names_the_check() {
        let state = xkr_storage::PruneState::default();
        let missing = Path::new("/nonexistent/xkrd-test/chain.redb");
        assert_eq!(storage_detail(missing, &state, None), "opened 0 B (full)");
        assert_eq!(
            storage_detail(missing, &state, Some(Duration::from_secs(192))),
            "opened 0 B (full, checked after an unclean stop · 3 min 12 s)"
        );
        assert_eq!(
            repair_notes(9_300_000_000),
            [
                "The node did not stop cleanly last time, so the database is being checked."
                    .to_owned(),
                "This reads the whole 9.30 GB file and can take a few minutes; it does not \
                 need the network."
                    .to_owned(),
            ]
        );
    }

    /// Records what a reporter writes.
    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl stopping::Sink for Recorder {
        fn line(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }

        fn region(&self, _: Vec<String>) {}
    }

    fn plain_reporter() -> (Arc<stopping::Reporter>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let r =
            stopping::Reporter::new(stopping::Mode::Plain, term::Style::new(false), rec.clone());
        (r, rec)
    }

    /// `stopping database … done 3 ms (closed)` → `stopping database … done (closed)`.
    fn untimed(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match (l.find("… done "), l.find(" (")) {
                (Some(a), Some(b)) if b > a => format!("{}… done{}", &l[..a], &l[b..]),
                _ => l.clone(),
            })
            .collect()
    }

    /// A stop during or after a failed start-up lists exactly what was opened.
    #[test]
    fn early_stops_list_what_was_opened() {
        // nothing opened yet (stop or error before the database)
        let (r, rec) = plain_reporter();
        assert_eq!(close_opened(Some(&r), Opened::Nothing, 3).unwrap(), None);
        assert_eq!(
            untimed(&rec.0.lock().unwrap()),
            ["stopping xkrd", "Node stopped · uptime 3 s"]
        );

        // `Chain::open` failed: it closed the database itself
        let (r, rec) = plain_reporter();
        assert_eq!(close_opened(Some(&r), Opened::Closed, 3).unwrap(), None);
        assert_eq!(
            untimed(&rec.0.lock().unwrap()),
            [
                "stopping xkrd",
                "stopping database … done (closed)",
                "Node stopped · uptime 3 s"
            ]
        );

        let dir = std::env::temp_dir().join(format!("xkrd-early-stop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // the database open (stop after Storage, or a bad fork flag)
        let store = Store::open_with_cache(dir.join("chain.redb"), 16 << 20).unwrap();
        let (r, rec) = plain_reporter();
        assert_eq!(
            close_opened(Some(&r), Opened::Store(store), 3).unwrap(),
            None
        );
        assert_eq!(
            untimed(&rec.0.lock().unwrap()),
            [
                "stopping xkrd",
                "stopping database … done (closed)",
                "Node stopped · uptime 3 s"
            ]
        );

        // the chain open (stop after Chain, or the chain thread could not start)
        let store = Store::open_with_cache(dir.join("chain.redb"), 16 << 20).unwrap();
        let pool = Arc::new(TxPool::new(PoolConfig::default()));
        let chain = Chain::open(store, ChainConfig::new(Network::Testnet), pool).unwrap();
        let (r, rec) = plain_reporter();
        assert_eq!(
            close_opened(Some(&r), Opened::Chain(&chain), 3).unwrap(),
            Some(0)
        );
        assert_eq!(
            untimed(&rec.0.lock().unwrap()),
            [
                "stopping xkrd",
                "stopping database … done (closed at height 0)",
                "Node stopped at height 0 · uptime 3 s"
            ]
        );
        drop(chain);
        let _ = std::fs::remove_dir_all(&dir);

        // without a reporter (tests, `xkrd db`) the database is still closed, without output
        assert_eq!(close_opened(None, Opened::Nothing, 0).unwrap(), None);
    }

    #[test]
    fn startup_details() {
        assert_eq!(
            console_hint(term::Style::new(false)),
            "  Type help for the console commands, exit to stop the node."
        );

        let tip = xkr_chain::TipInfo {
            height: 2_620_300,
            hash: xkr_types::Hash([0xab; 32]),
            ..xkr_chain::TipInfo::default()
        };
        assert_eq!(chain_detail(&tip), "height 2 620 300 · tip abababababab…");
    }

    #[test]
    fn hugin_step_details() {
        let donation = fee::Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: true,
        };
        assert_eq!(
            hugin_step_detail(200, &donation, Some("127.0.0.1:3333")),
            "max 200 apps · fee to nilsjr · p2pool 127.0.0.1:3333"
        );
        let own = fee::Fee {
            address: Some(fee::DEFAULT_FEE_ADDRESS.into()),
            donation: false,
        };
        assert_eq!(
            hugin_step_detail(200, &own, Some("stratum+ssl://p2pool.example.org:3333")),
            "max 200 apps · fee SEKReSw2cP…kcBJZGc · p2pool stratum+ssl://p2pool.ex…"
        );
        let none = fee::Fee {
            address: None,
            donation: false,
        };
        assert_eq!(
            hugin_step_detail(5, &none, None),
            "max 5 apps · no fee address · no p2pool"
        );
    }

    #[test]
    fn reorg_depth_is_clamped_only_when_pruning() {
        assert_eq!(MAX_PRUNED_REORG_DEPTH, 7_168);
        assert_eq!(max_reorg_depth(1_000, true), 1_000);
        assert_eq!(max_reorg_depth(50_000, true), MAX_PRUNED_REORG_DEPTH);
        assert_eq!(max_reorg_depth(50_000, false), 50_000);
    }
}
