//! `xkrwallet`: the terminal wallet (`src/zedwallet++/zed_wallet.cpp` `main`).
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use xkr_daemon_client::ClientSource;
use xkr_hyper_wallet::{LazySwarm, SwarmOptions};
use xkr_wallet_cli::args::{self, Command};
use xkr_wallet_cli::{Console, Nodes, Session, SessionConfig};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut options = match args::parse(&argv) {
        Command::Run(options) => options,
        Command::Print(text) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Command::Fail(text) => {
            println!("{text}");
            return ExitCode::from(1);
        }
    };
    if options.debug {
        debug_log();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            println!("Unexpected error: {e}");
            return ExitCode::from(1);
        }
    };
    let invite = match options.invite.as_deref().map(str::parse) {
        None => None,
        Some(Ok(invite)) => Some(invite),
        Some(Err(e)) => {
            println!("There was an error parsing the invite you specified: {e}");
            return ExitCode::from(1);
        }
    };
    let swarm = LazySwarm::new(SwarmOptions::from_env(
        options.network,
        invite,
        concat!("xkrwallet/", env!("CARGO_PKG_VERSION")),
    ));
    // Without --remote-daemon: the local node when one answers, else Hyperswarm.
    if !options.daemon_given
        && !runtime.block_on(xkr_wallet::local_node_answers(options.daemon_port))
    {
        println!(
            "No local node on port {}: using a node found over Hyperswarm.",
            options.daemon_port
        );
        xkr_wallet::SWARM_HOST.clone_into(&mut options.daemon_host);
    }
    let nodes = Nodes {
        swarm: Some(Arc::new(swarm.clone()) as Arc<dyn ClientSource>),
    };
    let mut config = SessionConfig::new(options.network);
    config.handle_signals = true;
    let session = Session::new(
        options,
        nodes,
        Console::stdio(),
        runtime.handle().clone(),
        config,
    );
    let code = session.run();
    runtime.block_on(swarm.stop());
    runtime.shutdown_timeout(Duration::from_secs(2));
    ExitCode::from(code)
}

/// `--debug` (zedwallet): the wallet's log goes to `xkrwallet.log` in the working directory.
fn debug_log() {
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("xkrwallet.log")
    else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
}
