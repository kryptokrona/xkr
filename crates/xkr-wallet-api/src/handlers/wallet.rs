//! Opening, importing, creating and closing (`ApiDispatcher::openWallet`, `keyImportWallet`,
//! `seedImportWallet`, `importViewWallet`, `createWallet`, `closeWallet`,
//! `getDefaultWalletParams`).
//!
//! Each takes `filename` and `password` (required) and `daemonHost` (default `127.0.0.1`) and
//! `daemonPort` (default the network's daemon RPC port); the imports take an optional
//! `scanHeight` (default 0). Success is 200 with an empty body. All parameters are read before
//! the wallet file is touched.

use serde_json::Value;
use xkr_crypto::{Scalar, sc_check};
use xkr_types::SecretKey;
use xkr_wallet::{ErrorCode, Wallet};
use zeroize::Zeroizing;

use super::Fail;
use crate::params::{self, BadParam};
use crate::respond::Reply;
use crate::server::{App, NodeFactory, Route};

/// `getDefaultWalletParams`.
struct WalletParams {
    filename: String,
    password: Zeroizing<String>,
    daemon_host: String,
    daemon_port: u16,
    /// Whether `daemonHost` or `daemonPort` was in the body.
    daemon_given: bool,
}

impl WalletParams {
    fn read(body: &Value, default_port: u16) -> Result<Self, BadParam> {
        let filename = params::string(body, "filename")?;
        let password = Zeroizing::new(params::string(body, "password")?);
        let daemon_host =
            params::opt_string(body, "daemonHost")?.unwrap_or_else(|| "127.0.0.1".to_owned());
        let daemon_port = if params::has(body, "daemonPort") {
            params::u16_param(body, "daemonPort")?
        } else {
            default_port
        };
        Ok(Self {
            filename,
            password,
            daemon_host,
            daemon_port,
            daemon_given: params::has(body, "daemonHost") || params::has(body, "daemonPort"),
        })
    }
}

/// What each closed-wallet route reads besides the wallet parameters.
enum Kind {
    Open,
    Create,
    Keys {
        view: SecretKey,
        spend: SecretKey,
        scan_height: u64,
    },
    Seed {
        mnemonic: Zeroizing<String>,
        scan_height: u64,
    },
    View {
        view: SecretKey,
        address: String,
        scan_height: u64,
    },
}

/// A private key of `/wallet/import/key` as a scalar mod `l`: any 32 bytes are accepted there,
/// and a key at or above the group order is used in its reduced form.
fn reduced(key: SecretKey) -> SecretKey {
    if sc_check(&key.0) {
        key
    } else {
        SecretKey(Scalar::from_bytes_mod_order(key.0).to_bytes())
    }
}

fn scan_height(body: &Value) -> Result<u64, BadParam> {
    Ok(params::opt_u64(body, "scanHeight")?.unwrap_or(0))
}

fn kind(route: &Route, body: &Value) -> Result<Kind, Fail> {
    Ok(match route {
        Route::WalletOpen => Kind::Open,
        Route::WalletCreate => Kind::Create,
        Route::ImportKey => Kind::Keys {
            view: reduced(params::key(body, "privateViewKey")?),
            spend: reduced(params::key(body, "privateSpendKey")?),
            scan_height: scan_height(body)?,
        },
        Route::ImportSeed => Kind::Seed {
            mnemonic: Zeroizing::new(params::string(body, "mnemonicSeed")?),
            scan_height: scan_height(body)?,
        },
        Route::ImportView => Kind::View {
            address: params::string(body, "address")?,
            view: params::key(body, "privateViewKey")?,
            scan_height: scan_height(body)?,
        },
        _ => return Err(Fail::Internal),
    })
}

/// The address errors `validateAddresses` reports; any other address problem on this route
/// answers 500 with an empty body, as the recorded answers do.
fn is_address_error(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::AddressWrongLength
            | ErrorCode::AddressWrongPrefix
            | ErrorCode::AddressNotBase58
            | ErrorCode::AddressNotValid
            | ErrorCode::IntegratedAddressPaymentIdInvalid
            | ErrorCode::AddressIsIntegrated
    )
}

async fn make<N: NodeFactory>(
    app: &App<N>,
    route: &Route,
    body: &Value,
) -> Result<Wallet<N::Transport>, Fail> {
    let mut p = WalletParams::read(body, app.default_daemon_port)?;
    let kind = kind(route, body)?;
    // Without daemonHost/daemonPort: the local node when one answers, else Hyperswarm.
    if !p.daemon_given
        && app.nodes.has_swarm()
        && !xkr_wallet::local_node_answers(p.daemon_port).await
    {
        tracing::info!("no daemon given and no local node: using a node found over Hyperswarm");
        xkr_wallet::SWARM_HOST.clone_into(&mut p.daemon_host);
    }
    let node = app.nodes.connect(&p.daemon_host, p.daemon_port)?;
    let config = app.wallet_config.clone();
    let (file, password) = (p.filename.as_str(), p.password.as_str());
    let wallet = match kind {
        Kind::Open => Wallet::open(file, password, node, config).await?,
        Kind::Create => Wallet::create(file, password, node, config).await?,
        Kind::Keys {
            view,
            spend,
            scan_height,
        } => Wallet::import_keys(file, password, spend, view, scan_height, node, config).await?,
        Kind::Seed {
            mnemonic,
            scan_height,
        } => Wallet::import_seed(file, password, &mnemonic, scan_height, node, config).await?,
        Kind::View {
            view,
            address,
            scan_height,
        } => Wallet::import_view(file, password, view, &address, scan_height, node, config)
            .await
            .map_err(|e| {
                if is_address_error(e.code()) {
                    Fail::Internal
                } else {
                    Fail::Wallet(e)
                }
            })?,
    };
    wallet.set_autosave(app.autosave());
    Ok(wallet)
}

/// POST `/wallet/open`, `/wallet/import/key`, `/wallet/import/seed`, `/wallet/import/view`,
/// `/wallet/create`: 403 if a wallet is open, else 200 or the error.
pub(crate) async fn open<N: NodeFactory>(app: &App<N>, route: &Route, body: &Value) -> Reply {
    let mut slot = app.wallet.write().await;
    if slot.is_some() {
        return Reply::empty(403);
    }
    match Box::pin(make(app, route, body)).await {
        Ok(wallet) => {
            *slot = Some(wallet);
            Reply::empty(200)
        }
        Err(fail) => fail.reply(),
    }
}

/// DELETE `/wallet`: stops syncing and saves under the write lock (C++ drops the
/// `WalletBackend`, whose destructor saves, while it holds `m_mutex`); 200, or 403 if no wallet
/// is open.
pub(crate) async fn close<N: NodeFactory>(app: &App<N>) -> Reply {
    let mut slot = app.wallet.write().await;
    let Some(wallet) = slot.take() else {
        return Reply::empty(403);
    };
    if let Err(e) = wallet.close().await {
        tracing::warn!(error = %e, "saving the wallet failed");
    }
    Reply::empty(200)
}
