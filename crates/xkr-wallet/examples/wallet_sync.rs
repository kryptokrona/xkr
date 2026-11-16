//! Opens a wallet file (format A or B, detected), syncs it against a daemon and prints one
//! JSON object with its addresses, balances, transactions and sync status. No private keys are
//! printed. Used by `testnet/fixtures/check-cpp-wallet-compat.sh`.
//!
//! ```text
//! wallet_sync <file> <password> --daemon host:port [--network testnet] [--timeout-secs N]
//!             [--send <address> <amount>] [--save]
//! ```
//!
//! `--send` pays `amount` atomic units to `address` (`SEKR…` or `Xkr…`) with `send_basic` after
//! the sync and waits until the transaction is in a block (or the timeout). `--save` saves the
//! wallet at the end (format B: the `.xkrstate` sidecar). Exit code 0 on success, 1 on a
//! wallet error or a sync timeout, 2 on bad arguments.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use xkr_consensus::Network;
use xkr_types::Hash;
use xkr_wallet::{FileFormat, Node, SyncConfig, Wallet, WalletConfig};

const USAGE: &str = "usage: wallet_sync <file> <password> --daemon host:port [--network testnet] \
                     [--timeout-secs N] [--send <address> <amount>] [--save]";

struct Args {
    file: String,
    password: String,
    host: String,
    port: u16,
    network: Network,
    timeout: Duration,
    send: Option<(String, u64)>,
    save: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut rest = std::env::args().skip(1);
    let file = rest.next().ok_or("missing <file>")?;
    let password = rest.next().ok_or("missing <password>")?;
    let mut daemon = None;
    let mut network = Network::Mainnet;
    let mut timeout = Duration::from_secs(300);
    let mut send = None;
    let mut save = false;
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--daemon" => daemon = Some(rest.next().ok_or("--daemon needs host:port")?),
            "--network" => {
                network = match rest.next().as_deref() {
                    Some("testnet") => Network::Testnet,
                    Some("mainnet") => Network::Mainnet,
                    _ => return Err("--network is mainnet or testnet".into()),
                };
            }
            "--timeout-secs" => {
                let secs = rest.next().and_then(|s| s.parse().ok());
                timeout = Duration::from_secs(secs.ok_or("--timeout-secs needs a number")?);
            }
            "--send" => {
                let address = rest.next().ok_or("--send needs <address> <amount>")?;
                let amount = rest.next().and_then(|s| s.parse().ok());
                send = Some((
                    address,
                    amount.ok_or("--send needs an amount in atomic units")?,
                ));
            }
            "--save" => save = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let daemon = daemon.ok_or("missing --daemon host:port")?;
    let (host, port) = daemon
        .rsplit_once(':')
        .and_then(|(h, p)| Some((h.to_owned(), p.parse().ok()?)))
        .ok_or("--daemon must be host:port")?;
    Ok(Args {
        file,
        password,
        host,
        port,
        network,
        timeout,
        send,
        save,
    })
}

/// Waits until `done` holds or `deadline` passes; false on timeout.
async fn wait(deadline: Instant, mut done: impl FnMut() -> bool) -> bool {
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    true
}

/// Synced: the wallet has processed the node's top block.
fn synced(wallet: &Wallet) -> bool {
    let s = wallet.status();
    s.daemon_online
        && s.last_block_hash.is_some()
        && s.wallet_block_count >= s.local_daemon_block_count
}

fn report(wallet: &Wallet, sent: Option<(Hash, bool)>) -> Result<Value, xkr_wallet::WalletError> {
    let format = match wallet.format() {
        FileFormat::WalletBackend => "WalletBackend",
        FileFormat::WalletBackendJsString => "WalletBackendJsString",
        FileFormat::WalletGreen => "WalletGreen",
    };
    let balance = wallet.balance(None)?;
    let balances: Vec<Value> = wallet
        .balances()
        .into_iter()
        .map(|b| json!({ "address": b.address, "unlocked": b.unlocked, "locked": b.locked }))
        .collect();
    let transactions: Vec<Value> = wallet
        .transactions(None, None)?
        .iter()
        .map(|t| {
            json!({
                "hash": t.hash.to_hex(),
                "blockHeight": t.block_height,
                "amount": t.total_amount(),
                "fee": t.fee,
                "paymentID": t.payment_id,
            })
        })
        .collect();
    let status = wallet.status();
    let mut out = json!({
        "format": format,
        "primaryAddress": wallet.primary_address(),
        "addresses": wallet.addresses(),
        "balance": { "unlocked": balance.unlocked, "locked": balance.locked },
        "balances": balances,
        "transactionCount": transactions.len(),
        "transactions": transactions,
        "unconfirmedCount": wallet.unconfirmed(None)?.len(),
        "status": {
            "walletBlockCount": status.wallet_block_count,
            "localDaemonBlockCount": status.local_daemon_block_count,
            "networkBlockCount": status.network_block_count,
        },
    });
    if let Some((hash, confirmed)) = sent {
        out["sent"] = json!({ "hash": hash.to_hex(), "confirmed": confirmed });
    }
    Ok(out)
}

async fn run(args: Args) -> Result<Value, String> {
    let node = Node::http(&args.host, args.port).map_err(|e| ToString::to_string(&e))?;
    let config = WalletConfig {
        network: args.network,
        sync: SyncConfig {
            poll_interval: Duration::from_secs(1),
            info_interval: Duration::from_secs(1),
            ..SyncConfig::default()
        },
        ..WalletConfig::default()
    };
    let wallet = Wallet::open(&args.file, &args.password, node, config)
        .await
        .map_err(|e| format!("open: {e} (code {})", e.number()))?;
    let deadline = Instant::now() + args.timeout;
    if !wait(deadline, || synced(&wallet)).await {
        return Err(format!(
            "not synced within the timeout: {:?}",
            wallet.status()
        ));
    }
    let mut sent = None;
    if let Some((address, amount)) = &args.send {
        let hash = wallet
            .send_basic(address, *amount, "")
            .await
            .map_err(|e| format!("send: {e} (code {})", e.number()))?;
        let confirmed = wait(deadline, || {
            wallet
                .unconfirmed(None)
                .is_ok_and(|p| p.iter().all(|t| t.hash != hash))
        })
        .await;
        sent = Some((hash, confirmed));
    }
    let out = report(&wallet, sent).map_err(|e| ToString::to_string(&e))?;
    if args.save {
        wallet.save().await.map_err(|e| format!("save: {e}"))?;
    }
    wallet.stop_sync().await;
    Ok(out)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(out) => {
            println!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wallet_sync: {e}");
            ExitCode::from(1)
        }
    }
}
