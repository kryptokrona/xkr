//! `xkrwallet`: the terminal wallet (`src/zedwallet++/zed_wallet.cpp` `main`).
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use xkr_wallet_cli::args::{self, Command};
use xkr_wallet_cli::{Console, HttpNodes, Session, SessionConfig};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let options = match args::parse(&argv) {
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
    let mut config = SessionConfig::new(options.network);
    config.handle_signals = true;
    let session = Session::new(
        options,
        HttpNodes,
        Console::stdio(),
        runtime.handle().clone(),
        config,
    );
    let code = session.run();
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
