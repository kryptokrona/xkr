//! `xkr-wallet-api` (also shipped as `wallet-api`): the wallet-api REST service
//! (`src/wallet_api/wallet_api.cpp` `main`).
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use xkr_daemon_client::ClientSource;
use xkr_hyper_wallet::{LazySwarm, SwarmOptions};
use xkr_wallet::{AutosaveConfig, WalletConfig};
use xkr_wallet_api::cli::{self, Command, Options};
use xkr_wallet_api::console::{self, HINT};
use xkr_wallet_api::{ApiConfig, App, Nodes, router};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let options = match cli::parse(&args) {
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
    println!("{}", cli::header());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("Unexpected error: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(run(options))
}

async fn run(options: Options) -> ExitCode {
    let config = ApiConfig {
        rpc_password: options.rpc_password,
        cors: options.enable_cors,
        default_daemon_port: options.network.params().rpc_port,
        wallet: WalletConfig {
            network: options.network,
            ..WalletConfig::default()
        },
    };
    let swarm = LazySwarm::new(SwarmOptions::from_env(
        options.network,
        None,
        concat!("xkr-wallet-api/", env!("CARGO_PKG_VERSION")),
    ));
    let nodes = Nodes {
        swarm: Some(Arc::new(swarm.clone()) as Arc<dyn ClientSource>),
    };
    let app = App::new(config, nodes);
    app.set_autosave(Some(AutosaveConfig::default()));
    let Ok(listener) = TcpListener::bind((options.rpc_bind_ip.as_str(), options.port)).await else {
        println!(
            "Could not bind service to {}:{}\nIs another service using this address and port?",
            options.rpc_bind_ip, options.port
        );
        return ExitCode::from(1);
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serve =
        axum::serve(listener, router(Arc::clone(&app))).with_graceful_shutdown(async move {
            let _ = stop_rx.await;
        });
    let mut server = tokio::spawn(serve.into_future());
    println!(
        "Want documentation on how to use the wallet-api?\nSee https://turtlecoin.github.io/wallet-api-docs/\n"
    );
    println!(
        "The api has been launched on http://{}:{}.\n{HINT}",
        options.rpc_bind_ip, options.port
    );
    let console = console::spawn();
    let mut code = ExitCode::SUCCESS;
    tokio::select! {
        _ = console => {}
        () = wait_for_signal() => {}
        result = &mut server => {
            println!("Unexpected error: {}", match result {
                Ok(Ok(())) => "the server stopped".to_owned(),
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
            });
            code = ExitCode::from(1);
        }
    }
    println!("\nSaving and shutting down...");
    let _ = stop_tx.send(());
    if !server.is_finished() {
        let _ = server.await;
    }
    app.close_wallet().await;
    swarm.stop().await;
    code
}

/// SIGINT (Ctrl-C) everywhere, SIGTERM on Unix (`tools::SignalHandler`).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
