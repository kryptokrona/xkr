//! The wallet, embedded: `xkr-wallet` runs in the app and syncs from the bundled xkrd's
//! loopback RPC (or a remote node from the settings).
//!
//! One wallet is open at a time. New wallets go to `<app data>/wallets` (`wallets-testnet` on
//! testnet) in the wallet-api format; any file the wallet tools open can be opened from
//! anywhere, including the old C++ files, which `xkr-wallet` converts on first open.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use xkr_consensus::Network;
use xkr_wallet::validate::validate_address;
use xkr_wallet::{AutosaveConfig, Node, Transaction, Wallet, WalletConfig, WalletError};
use zeroize::Zeroizing;

/// Extension of new wallet files (wallet-api, wallet-backend-js and zedwallet use it too).
const EXT: &str = "wallet";
/// How many recently opened files are remembered.
const RECENT: usize = 8;

/// Where the wallet syncs from.
#[derive(Debug, Clone)]
pub struct NodeAddr {
    pub host: String,
    pub port: u16,
}

struct Open {
    wallet: Wallet,
    password: Zeroizing<String>,
}

/// The open wallet, if any.
pub struct WalletManager {
    app_data: PathBuf,
    open: Mutex<Option<Open>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Recent {
    files: Vec<PathBuf>,
}

fn err(e: &WalletError) -> String {
    match e.detail() {
        Some(d) => format!("{e} ({d})"),
        None => e.to_string(),
    }
}

impl WalletManager {
    pub fn new(app_data: &Path) -> Self {
        Self {
            app_data: app_data.to_owned(),
            open: Mutex::new(None),
        }
    }

    fn dir(&self, network: Network) -> PathBuf {
        self.app_data.join(match network {
            Network::Mainnet => "wallets",
            Network::Testnet => "wallets-testnet",
        })
    }

    fn recent_file(&self) -> PathBuf {
        self.app_data.join("recent-wallets.json")
    }

    fn remember(&self, path: &Path) {
        let file = self.recent_file();
        let mut r: Recent = fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        r.files.retain(|p| p != path);
        r.files.insert(0, path.to_owned());
        r.files.truncate(RECENT);
        if let Ok(text) = serde_json::to_string_pretty(&r) {
            let _ = fs::write(file, text);
        }
    }

    /// Wallet files in the app's wallet directory, then recently opened ones elsewhere.
    pub fn list(&self, network: Network) -> Value {
        let dir = self.dir(network);
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == EXT) && p.is_file())
            .collect();
        let recent: Recent = fs::read_to_string(self.recent_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for p in recent.files {
            if p.is_file() && !files.contains(&p) {
                files.push(p);
            }
        }
        let modified = |p: &Path| {
            fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs())
        };
        files.sort_by_key(|p| std::cmp::Reverse(modified(p)));
        let list: Vec<Value> = files
            .iter()
            .map(|p| {
                json!({
                    "name": p.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default(),
                    "path": p,
                    "modified": modified(p),
                })
            })
            .collect();
        json!({ "dir": dir, "wallets": list })
    }

    /// The file for a new wallet called `name`.
    fn new_path(&self, network: Network, name: &str) -> Result<PathBuf, String> {
        let name = name.trim();
        if name.is_empty()
            || name.len() > 64
            || name.starts_with('.')
            || name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0'))
        {
            return Err("use a name without slashes or colons".into());
        }
        let dir = self.dir(network);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{name}.{EXT}"));
        if path.exists() {
            return Err(format!("a wallet called {name} exists already"));
        }
        Ok(path)
    }

    fn config(network: Network) -> WalletConfig {
        WalletConfig {
            network,
            ..WalletConfig::default()
        }
    }

    async fn install(&self, wallet: Wallet, password: &str) {
        wallet.set_autosave(Some(AutosaveConfig::default()));
        self.remember(&wallet.path());
        let previous = self.open.lock().await.replace(Open {
            wallet,
            password: Zeroizing::new(password.to_owned()),
        });
        if let Some(old) = previous
            && let Err(e) = old.wallet.close().await
        {
            eprintln!("cannot save the previous wallet: {e}");
        }
    }

    /// Creates a new wallet; returns its seed words for the backup screen.
    pub async fn create(
        &self,
        network: Network,
        node: NodeAddr,
        name: &str,
        password: &str,
    ) -> Result<Value, String> {
        let path = self.new_path(network, name)?;
        let wallet = Wallet::create(&path, password, http(&node)?, Self::config(network))
            .await
            .map_err(|e| err(&e))?;
        let address = wallet.primary_address();
        let mnemonic = wallet.mnemonic(&address).map_err(|e| err(&e))?;
        self.install(wallet, password).await;
        Ok(json!({ "address": address, "mnemonic": mnemonic }))
    }

    /// Restores a wallet from its 25 seed words.
    pub async fn import_seed(
        &self,
        network: Network,
        node: NodeAddr,
        name: &str,
        password: &str,
        mnemonic: &str,
        scan_height: u64,
    ) -> Result<(), String> {
        let path = self.new_path(network, name)?;
        let words = mnemonic.split_whitespace().collect::<Vec<_>>().join(" ");
        let wallet = Wallet::import_seed(
            &path,
            password,
            &words,
            scan_height,
            http(&node)?,
            Self::config(network),
        )
        .await
        .map_err(|e| err(&e))?;
        self.install(wallet, password).await;
        Ok(())
    }

    /// Opens a wallet file of any format the wallet tools read.
    pub async fn open(
        &self,
        network: Network,
        node: NodeAddr,
        path: &Path,
        password: &str,
    ) -> Result<(), String> {
        let wallet = Wallet::open(path, password, http(&node)?, Self::config(network))
            .await
            .map_err(|e| err(&e))?;
        self.install(wallet, password).await;
        Ok(())
    }

    /// Saves and closes the open wallet.
    pub async fn close(&self) -> Result<(), String> {
        if let Some(open) = self.open.lock().await.take() {
            open.wallet.close().await.map_err(|e| err(&e))?;
        }
        Ok(())
    }

    /// Points the open wallet at another node (after the settings changed).
    pub async fn swap_node(&self, node: NodeAddr) -> Result<(), String> {
        if let Some(open) = self.open.lock().await.as_ref() {
            let current = open.wallet.node();
            if current.host != node.host || current.port != node.port {
                open.wallet.swap_node(http(&node)?).await;
            }
        }
        Ok(())
    }

    /// Address, balance and sync state; `{ "open": false }` without a wallet.
    pub async fn overview(&self) -> Value {
        let guard = self.open.lock().await;
        let Some(open) = guard.as_ref() else {
            return json!({ "open": false });
        };
        let w = &open.wallet;
        let balance = w.balance(None).unwrap_or_default();
        let status = w.status();
        let node = w.node();
        let path = w.path();
        json!({
            "open": true,
            "name": path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default(),
            "path": path,
            "format": format!("{:?}", w.format()),
            "address": w.primary_address(),
            "addresses": w.addresses(),
            "view_only": w.is_view_wallet(),
            "balance": { "unlocked": balance.unlocked, "locked": balance.locked },
            "sync": {
                "wallet_height": status.wallet_block_count,
                "node_height": status.local_daemon_block_count,
                "network_height": status.network_block_count,
                "daemon_online": status.daemon_online,
                "peers": status.peer_count,
                "hashrate": status.hashrate,
            },
            "node": {
                "host": node.host,
                "port": node.port,
                "fee_address": node.fee_address,
                "fee_amount": node.fee_amount,
            },
        })
    }

    /// Transactions, newest first, unconfirmed ones on top; at most `limit`.
    pub async fn transactions(&self, limit: usize) -> Result<Value, String> {
        let guard = self.open.lock().await;
        let open = guard.as_ref().ok_or("no wallet is open")?;
        let w = &open.wallet;
        let mut confirmed = w.transactions(None, None).map_err(|e| err(&e))?;
        confirmed.sort_by_key(|t| std::cmp::Reverse((t.block_height, t.timestamp)));
        let pending = w.unconfirmed(None).map_err(|e| err(&e))?;
        let tx = |t: &Transaction, confirmed: bool| {
            json!({
                "hash": t.hash.to_string(),
                "amount": t.total_amount(),
                "fee": t.fee,
                "height": t.block_height,
                "timestamp": t.timestamp,
                "payment_id": t.payment_id,
                "coinbase": t.is_coinbase,
                "confirmed": confirmed,
            })
        };
        let list: Vec<Value> = pending
            .iter()
            .map(|t| tx(t, false))
            .chain(confirmed.iter().map(|t| tx(t, true)))
            .take(limit)
            .collect();
        Ok(json!({ "total": confirmed.len() + pending.len(), "transactions": list }))
    }

    /// Sends `amount` atomic units; returns the transaction hash.
    pub async fn send(
        &self,
        address: &str,
        amount: u64,
        payment_id: &str,
    ) -> Result<String, String> {
        let guard = self.open.lock().await;
        let open = guard.as_ref().ok_or("no wallet is open")?;
        let hash = open
            .wallet
            .send_basic(address.trim(), amount, payment_id.trim())
            .await
            .map_err(|e| err(&e))?;
        Ok(hash.to_string())
    }

    /// The seed words of the primary address, after the password is given again.
    pub async fn mnemonic(&self, password: &str) -> Result<String, String> {
        let guard = self.open.lock().await;
        let open = guard.as_ref().ok_or("no wallet is open")?;
        if open.password.as_str() != password {
            return Err("wrong password".into());
        }
        let address = open.wallet.primary_address();
        open.wallet.mnemonic(&address).map_err(|e| err(&e))
    }

    /// Scans again from `height`.
    pub async fn rescan(&self, height: u64) -> Result<(), String> {
        let guard = self.open.lock().await;
        let open = guard.as_ref().ok_or("no wallet is open")?;
        open.wallet.reset(height).await.map_err(|e| err(&e))
    }
}

/// A node over HTTP.
fn http(node: &NodeAddr) -> Result<Node, String> {
    Node::http(&node.host, node.port).map_err(|e| err(&e))
}

/// Whether `address` is a valid XKR address (integrated addresses allowed).
pub fn check_address(address: &str) -> Result<(), String> {
    validate_address(address.trim(), true)
        .map(|_| ())
        .map_err(|e| err(&e))
}
