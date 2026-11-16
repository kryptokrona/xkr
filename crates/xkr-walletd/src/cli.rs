//! The program (`src/wallet_service/main.cpp` `main`, `payment_gate_service.cpp`
//! `PaymentGateService::init`, `run`, `runRpcProxy`, `runWalletService`).
//!
//! Exit codes and messages follow C++: settings that cannot be used print `Fatal error: …` and
//! end with 1; a container that does not open is logged (`Failed to init walletService reason:
//! …`) and ends with 0, as does a refused `--generate-container`.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use xkr_daemon_client::{DaemonClient, DaemonUrl, HttpConfig, HttpTransport};
use xkr_wallet::{AutosaveConfig, FileFormat, Node, SyncConfig, Wallet, WalletConfig};

use crate::config::{self, Cli, Config, Fatal, LOG_INFO, Startup};
use crate::container::{self, Generated};
use crate::errors::AppError;
use crate::rpc;
use crate::service::{RpcSettings, Service};

/// `cryptonote::getProjectCLIHeader`.
#[must_use]
pub fn header() -> String {
    format!(
        "\n xkr-walletd v{} (kryptokrona-service)\n This software is distributed under the \
         General Public License v3.0\n\n Copyright 2019, The Kryptokrona Developers\n\n \
         Additional Copyright(s) may apply, please see the included LICENSE file for more \
         information.\n If you did not receive a copy of the LICENSE, please visit:\n \
         https://github.com/kryptokrona/kryptokrona/blob/master/LICENSE\n\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// `Fatal error: <message>` on standard error, exit code 1.
fn fatal(message: &str) -> ExitCode {
    eprintln!("Fatal error: {message}");
    ExitCode::from(1)
}

/// A C++ `logging::Level` as a tracing filter (`FATAL` and `ERROR` are errors).
fn level_filter(level: i32) -> LevelFilter {
    match level {
        ..=1 => LevelFilter::ERROR,
        2 => LevelFilter::WARN,
        3 => LevelFilter::INFO,
        4 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}

/// The console logger (at most `INFO`, as C++ `consoleLogger`) and the log file (everything up
/// to `--log-level`).
fn init_logging(level: i32, file: std::fs::File) {
    let console = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(level_filter(level.min(LOG_INFO)));
    let file = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(file))
        .with_filter(level_filter(level));
    let _ = tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init();
}

/// Waits for Ctrl-C or SIGTERM (`tools::SignalHandler`).
async fn stop_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("Stop signal caught");
}

/// The engine settings of the service's wallet.
fn wallet_config(config: &Config, start_sync: bool) -> WalletConfig {
    WalletConfig {
        network: config.network,
        format: FileFormat::WalletGreen,
        sync: SyncConfig::default(),
        start_sync,
    }
}

/// `PaymentGateService::runRpcProxy` + `runWalletService`.
async fn serve(config: &Config) -> ExitCode {
    if config.generate {
        return match container::generate(config).await {
            Ok(Generated::Created(_) | Generated::Refused(_)) => ExitCode::SUCCESS,
            Err(e) => fatal(&e),
        };
    }

    tracing::info!(
        "Starting Payment Gate with remote node, timeout: {}",
        config.init_timeout
    );
    let port = config.daemon_port as u16;
    let url = if config.daemon_address.contains("://") {
        DaemonUrl::parse(&config.daemon_address)
    } else {
        DaemonUrl::from_host_port(&config.daemon_address, port)
    };
    let transport = match url.and_then(|u| HttpTransport::with_url(u, &HttpConfig::default())) {
        Ok(t) => t,
        Err(e) => return fatal(&e.to_string()),
    };
    let client = DaemonClient::new(transport.clone());
    // `NodeRpcProxy::init`: the first status poll must finish within the timeout
    let timeout = Duration::from_secs(u64::try_from(config.init_timeout).unwrap_or(0));
    if tokio::time::timeout(timeout, client.info()).await.is_err() {
        return fatal("Operation timed out");
    }
    let node = Node::new(
        DaemonClient::new(transport),
        config.daemon_address.clone(),
        port,
    );

    tracing::info!("Loading wallet");
    let wallet = match Wallet::open(
        &config.container_file,
        &config.container_password,
        node,
        wallet_config(config, !config.print_addresses),
    )
    .await
    {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(
                "Failed to init walletService reason: {}",
                AppError::from(&e).message
            );
            return ExitCode::SUCCESS;
        }
    };
    tracing::info!("Wallet loading is finished.");
    let node_info = wallet.node();
    if !node_info.fee_address.is_empty() && node_info.fee_amount != 0 {
        tracing::info!(
            "You have connected to a node that charges a fee to send transactions: {} per \
             transaction to {}",
            node_info.fee_amount,
            node_info.fee_address
        );
    }

    if config.print_addresses {
        for address in wallet.addresses() {
            println!("Address: {address}");
        }
        wallet.stop_sync().await;
        return ExitCode::SUCCESS;
    }

    let settings = if config.legacy_security {
        RpcSettings::legacy(&config.cors)
    } else {
        RpcSettings::with_password(&config.rpc_password, &config.cors)
    };
    // the container saves by itself too: every few minutes while something changed, after
    // each transaction and after a long sync, besides `save` and the shutdown
    wallet.set_autosave(Some(AutosaveConfig::default()));
    let wallet = Arc::new(wallet);
    let service = Arc::new(Service::new(
        Arc::clone(&wallet),
        client,
        config.network,
        settings,
    ));
    let bind = (config.bind_address.as_str(), config.bind_port as u16);
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => return fatal(&e.to_string()),
    };
    tracing::info!(
        "JSON-RPC server listening on {}:{}",
        config.bind_address,
        config.bind_port
    );
    if let Err(e) = axum::serve(listener, rpc::router(Arc::clone(&service)))
        .with_graceful_shutdown(stop_signal())
        .await
    {
        tracing::error!("JSON-RPC server: {e}");
    }
    tracing::info!("JSON-RPC server stopped, stopping wallet service...");
    let _ready = service.serialize().await;
    wallet.set_autosave(None);
    wallet.stop_sync().await;
    match wallet.save().await {
        Ok(()) => tracing::info!("Wallet is saved"),
        Err(e) => tracing::warn!("Couldn't save container: {e}"),
    }
    ExitCode::SUCCESS
}

/// `main`.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            println!(
                "Error: Unable to parse command line argument options: {e}\n\n{}",
                Cli::help_text()
            );
            return ExitCode::from(1);
        }
    };
    let header = header();
    let config = match config::load(&cli, &header) {
        Ok(Startup::Exit { stdout, code }) => {
            print!("{stdout}");
            return ExitCode::from(code);
        }
        Ok(Startup::Run(config)) => *config,
        Err(Fatal(message)) => return fatal(&message),
    };

    // `PaymentGateService::init`
    if !config.server_root.is_empty()
        && let Err(e) = std::env::set_current_dir(&config.server_root)
    {
        return fatal(&format!(
            "Couldn't change directory to '{}': {e}",
            config.server_root
        ));
    }
    let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log_file)
    else {
        return fatal("Couldn't open log file");
    };
    init_logging(config.log_level, log_file);
    if !config.server_root.is_empty() {
        tracing::info!("Current working directory now is {}", config.server_root);
    }
    print!("{header}");

    if config.register_service || config.unregister_service || config.daemonize {
        return fatal(
            "--daemonize, --register-service and --unregister-service are Windows service \
             options of the C++ build; run this program under the system's service manager \
             instead",
        );
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => return fatal(&e.to_string()),
    };
    runtime.block_on(serve(&config))
}
