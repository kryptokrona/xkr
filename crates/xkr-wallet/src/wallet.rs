//! The wallet handle (`src/wallet_backend/wallet_backend.cpp` `WalletBackend`).
//!
//! A [`Wallet`] owns the state, the file it is stored in, the daemon connection and the sync
//! tasks. All methods take `&self`; the state sits behind one lock that sync and the API calls
//! share (C++ pauses the synchronizer for changes, `pauseSynchronizerToRunFunction`). Sends,
//! fusions and delayed transactions are serialised among themselves so two of them never pick
//! the same inputs. Saving encrypts on the blocking thread pool.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use xkr_consensus::Network;
use xkr_crypto::{generate_keys, os_rng};
use xkr_daemon_client::{
    AnyTransport, ClientSource, DaemonClient, DaemonError, DaemonUrl, HttpConfig, HttpTransport,
    SwarmTransport, Transport,
};
use xkr_serialization::decode_transaction;
use xkr_types::{Hash, PublicKey, SecretKey};
use xkr_wallet_core::select::FusionEstimate;
use xkr_wallet_core::{
    WalletKeys, is_deterministic, mnemonic_from_secret, secret_from_mnemonic, view_key_from_spend,
};
use zeroize::Zeroizing;

use crate::autosave::{self, AutosaveConfig, AutosaveTask, SaveMark};
use crate::error::{ErrorCode, WalletError, walletd_codes};
use crate::events::{EVENT_CHANNEL_CAPACITY, WalletEvent};
use crate::persist::green::GreenHistory;
use crate::persist::{self, FileFormat, WalletFile};
use crate::send::delayed::DelayedStore;
use crate::send::fusion::{self, FusionRequest};
use crate::send::relay::{self, daemon_offline};
use crate::send::{self, Prepared, SendRequest, our_key};
use crate::state::{
    AddressBalance, Balance, Transaction, TransactionInfo, TxState, WalletState,
    timestamp_to_scan_height, unix_now,
};
use crate::sync::{self, DaemonStatus, SyncConfig, SyncTasks};
use crate::validate::validate_address;

/// The host a wallet reports for a daemon reached over Hyperswarm ([`Node::swarm`]); given as
/// the daemon host (`--remote-daemon hyperswarm`, wallet-api `daemonHost`) it picks Hyperswarm.
pub const SWARM_HOST: &str = "hyperswarm";

/// A daemon to talk to, with the host and port wallet-api reports (`GET /node`).
#[derive(Debug)]
pub struct Node<T = AnyTransport> {
    /// The client.
    pub client: DaemonClient<T>,
    /// Host as given.
    pub host: String,
    /// Port as given.
    pub port: u16,
}

impl Node<AnyTransport> {
    /// A daemon at `host:port` over HTTP (`Nigel(daemonHost, daemonPort)`). A `host` with a
    /// scheme (`https://node.example/prefix`) is used as the whole URL.
    pub fn http(host: &str, port: u16) -> Result<Self, WalletError> {
        let url = if host.contains("://") {
            DaemonUrl::parse(host)
        } else {
            DaemonUrl::from_host_port(host, port)
        }
        .map_err(|e| WalletError::new(ErrorCode::DaemonOffline).with_detail(e.to_string()))?;
        let transport = HttpTransport::with_url(url, &HttpConfig::default())
            .map_err(|e| WalletError::new(ErrorCode::DaemonOffline).with_detail(e.to_string()))?;
        Ok(Self {
            client: DaemonClient::new(AnyTransport::Http(transport)),
            host: host.to_owned(),
            port,
        })
    }

    /// A node found over Hyperswarm through `source` (reported as host [`SWARM_HOST`], port 0).
    #[must_use]
    pub fn swarm(source: Arc<dyn ClientSource>) -> Self {
        Self {
            client: DaemonClient::new(AnyTransport::Swarm(SwarmTransport::new(source))),
            host: SWARM_HOST.to_owned(),
            port: 0,
        }
    }

    /// The daemon of `host:port`: Hyperswarm through `swarm` when `host` is [`SWARM_HOST`],
    /// else HTTP.
    ///
    /// # Errors
    /// [`SWARM_HOST`] without a `swarm`, or an address that does not parse.
    pub fn connect(
        host: &str,
        port: u16,
        swarm: Option<&Arc<dyn ClientSource>>,
    ) -> Result<Self, WalletError> {
        if host == SWARM_HOST {
            return swarm.map(|s| Self::swarm(s.clone())).ok_or_else(|| {
                WalletError::new(ErrorCode::DaemonOffline)
                    .with_detail("Hyperswarm is not available in this build".to_owned())
            });
        }
        Self::http(host, port)
    }
}

/// How long [`local_node_answers`] waits.
pub const LOCAL_PROBE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a node answers `/info` on `127.0.0.1:port` within [`LOCAL_PROBE`]: the wallet tools
/// use a local node when one runs and Hyperswarm otherwise, unless a daemon address is given.
pub async fn local_node_answers(port: u16) -> bool {
    let Ok(transport) = HttpTransport::with_url(
        match DaemonUrl::from_host_port("127.0.0.1", port) {
            Ok(u) => u,
            Err(_) => return false,
        },
        &HttpConfig {
            request_timeout: LOCAL_PROBE,
            connect_timeout: LOCAL_PROBE,
            ..HttpConfig::default()
        },
    ) else {
        return false;
    };
    DaemonClient::new(transport).info().await.is_ok()
}

impl<T: Transport> Node<T> {
    /// A daemon behind any transport (tests use an in-process router).
    #[must_use]
    pub fn new(client: DaemonClient<T>, host: impl Into<String>, port: u16) -> Self {
        Self {
            client,
            host: host.into(),
            port,
        }
    }
}

/// Settings of a wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletConfig {
    /// The network (ring-size rule for version-2 transactions).
    pub network: Network,
    /// The format new wallets are written in (opening detects the format).
    pub format: FileFormat,
    /// Sync timing.
    pub sync: SyncConfig,
    /// Whether to start syncing when the wallet is opened (C++ `WalletBackend::init` always
    /// does).
    pub start_sync: bool,
}

impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            network: Network::Mainnet,
            format: FileFormat::WalletBackend,
            sync: SyncConfig::default(),
            start_sync: true,
        }
    }
}

/// wallet-api `/status` and walletd `getStatus` (`WalletBackend::getStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletStatus {
    /// Index of the last block the wallet processed.
    pub wallet_block_count: u64,
    /// The node's top block index.
    pub local_daemon_block_count: u64,
    /// The network's top block index as the node sees it.
    pub network_block_count: u64,
    /// The node's peers.
    pub peer_count: u64,
    /// Network hashrate estimate (difficulty / 90).
    pub hashrate: u64,
    /// Whether this is a view wallet.
    pub is_view_wallet: bool,
    /// Number of subwallets.
    pub subwallet_count: usize,
    /// Hash of the last block the wallet processed.
    pub last_block_hash: Option<Hash>,
    /// Whether the node answered the last request.
    pub daemon_online: bool,
}

/// wallet-api `GET /node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// Host as given.
    pub host: String,
    /// Port as given.
    pub port: u16,
    /// The node fee address, empty if the node takes no fee.
    pub fee_address: String,
    /// The node fee per transaction.
    pub fee_amount: u64,
}

/// What the wallet and its tasks share.
pub(crate) struct Shared<T> {
    pub state: Mutex<WalletState>,
    pub node: RwLock<Arc<Node<T>>>,
    pub daemon: RwLock<DaemonStatus>,
    pub events: broadcast::Sender<WalletEvent>,
    pub file: Mutex<WalletFile>,
    pub send_lock: tokio::sync::Mutex<()>,
    pub save_lock: tokio::sync::Mutex<()>,
    pub block_hashes: Mutex<BTreeMap<u64, Hash>>,
    pub config: WalletConfig,
    /// walletd puts the node fee before the destinations, `WalletBackend` after them.
    pub node_fee_first: AtomicBool,
    /// Bumped on every change of the state that a save should write.
    pub changes: AtomicU64,
    /// Bumped when a transaction was recorded (received, sent, cancelled).
    pub tx_changes: AtomicU64,
    /// Wakes the autosave task after a change.
    pub changed: tokio::sync::Notify,
    /// What the last save wrote.
    pub saved: Mutex<SaveMark>,
}

impl<T> Shared<T> {
    /// The current daemon.
    pub fn node(&self) -> Arc<Node<T>> {
        Arc::clone(&self.node.read())
    }

    /// Sends an event to the subscribers (none is fine).
    pub fn emit(&self, event: WalletEvent) {
        let _ = self.events.send(event);
    }

    /// Notes a change of the state for the autosave; `transaction` when a transaction was
    /// recorded.
    pub fn mark_changed(&self, transaction: bool) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        if transaction {
            self.tx_changes.fetch_add(1, Ordering::SeqCst);
        }
        self.changed.notify_one();
    }
}

/// An open wallet.
pub struct Wallet<T = AnyTransport> {
    shared: Arc<Shared<T>>,
    tasks: Mutex<Option<SyncTasks>>,
    autosave: Mutex<Option<AutosaveTask>>,
}

impl<T> std::fmt::Debug for Wallet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wallet")
            .field("file", &*self.shared.file.lock())
            .finish_non_exhaustive()
    }
}

/// Runs blocking file work on the blocking pool.
async fn blocking<R, F>(work: F) -> Result<R, WalletError>
where
    R: Send + 'static,
    F: FnOnce() -> Result<R, WalletError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        WalletError::new(ErrorCode::InvalidWalletFilename).with_detail(e.to_string())
    })?
}

/// Writes a new wallet's file(s): format A once; format B the container and the sidecar.
fn write_new(file: &WalletFile, state: &WalletState) -> Result<(), WalletError> {
    if file.format == FileFormat::WalletGreen {
        let container = persist::green::green_from_state(state, file.view_timestamp, unix_now());
        persist::write_container(&file.path, &file.password, &container)?;
    }
    persist::save(file, &persist::backend::state_to_json(state))
}

impl<T: Transport + 'static> Wallet<T> {
    // -----------------------------------------------------------------------------------------
    // opening and closing

    async fn launch(
        state: WalletState,
        file: WalletFile,
        node: Node<T>,
        config: WalletConfig,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let saved = SaveMark {
            changes: 0,
            tx_changes: 0,
            height: state.sync_status().height(),
            at: Instant::now(),
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            node: RwLock::new(Arc::new(node)),
            daemon: RwLock::new(DaemonStatus::default()),
            events,
            file: Mutex::new(file),
            send_lock: tokio::sync::Mutex::new(()),
            save_lock: tokio::sync::Mutex::new(()),
            block_hashes: Mutex::new(BTreeMap::new()),
            config,
            node_fee_first: AtomicBool::new(false),
            changes: AtomicU64::new(0),
            tx_changes: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
            saved: Mutex::new(saved),
        });
        // `Nigel::init`: a node that does not answer is not an error; the wallet opens offline.
        let _ = sync::refresh_info(&shared).await;
        sync::refresh_fee(&shared).await;
        let tasks = shared.config.start_sync.then(|| SyncTasks::start(&shared));
        Self {
            shared,
            tasks: Mutex::new(tasks),
            autosave: Mutex::new(None),
        }
    }

    async fn create_with_state(
        path: PathBuf,
        password: &str,
        state: WalletState,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let file = WalletFile {
            path,
            password: Zeroizing::new(password.to_owned()),
            format: config.format,
            view_timestamp: unix_now(),
        };
        let (file_copy, state_copy) = (file.clone(), state.clone());
        blocking(move || write_new(&file_copy, &state_copy)).await?;
        Ok(Self::launch(state, file, node, config).await)
    }

    /// `WalletBackend::createWallet`: new deterministic keys, scanning from now. Fails with
    /// `WALLET_FILE_ALREADY_EXISTS` or `INVALID_WALLET_FILENAME` as C++
    /// (`checkNewWalletFilename`).
    pub async fn create(
        path: impl AsRef<Path>,
        password: &str,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let path = path.as_ref().to_path_buf();
        persist::check_new_file(&path)?;
        let keys = WalletKeys::generate(&mut os_rng());
        let state =
            WalletState::new_spend(keys.spend_secret, keys.view_secret, 0, true, unix_now())?;
        Self::create_with_state(path, password, state, node, config).await
    }

    /// `WalletBackend::importWalletFromKeys`.
    pub async fn import_keys(
        path: impl AsRef<Path>,
        password: &str,
        spend_secret: SecretKey,
        view_secret: SecretKey,
        scan_height: u64,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let path = path.as_ref().to_path_buf();
        persist::check_new_file(&path)?;
        let state =
            WalletState::new_spend(spend_secret, view_secret, scan_height, false, unix_now())?;
        Self::create_with_state(path, password, state, node, config).await
    }

    /// `WalletBackend::importWalletFromSeed`: the mnemonic gives the spend key, the view key is
    /// derived from it.
    pub async fn import_seed(
        path: impl AsRef<Path>,
        password: &str,
        mnemonic: &str,
        scan_height: u64,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let path = path.as_ref().to_path_buf();
        persist::check_new_file(&path)?;
        let spend_secret = secret_from_mnemonic(mnemonic)?;
        let (view_secret, _) = view_key_from_spend(&spend_secret);
        let state =
            WalletState::new_spend(spend_secret, view_secret, scan_height, false, unix_now())?;
        Self::create_with_state(path, password, state, node, config).await
    }

    /// `WalletBackend::importViewWallet`: the private view key and the address.
    pub async fn import_view(
        path: impl AsRef<Path>,
        password: &str,
        view_secret: SecretKey,
        address: &str,
        scan_height: u64,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let path = path.as_ref().to_path_buf();
        persist::check_new_file(&path)?;
        let address = validate_address(address, false)?;
        let state = WalletState::new_view(view_secret, &address, scan_height, unix_now())?;
        Self::create_with_state(path, password, state, node, config).await
    }

    /// `WalletBackend::openWallet`, for any of the wallet formats (detected from the file).
    /// A walletd container's state is read from `<container>.xkrstate` when it exists and
    /// belongs to the container; otherwise the wallet starts from the container's keys and
    /// cached history and rescans.
    pub async fn open(
        path: impl AsRef<Path>,
        password: &str,
        node: Node<T>,
        config: WalletConfig,
    ) -> Result<Self, WalletError> {
        let path = path.as_ref().to_path_buf();
        let password_copy = Zeroizing::new(password.to_owned());
        let loaded = blocking(move || persist::load(&path, &password_copy, unix_now())).await?;
        let mut config = config;
        config.format = loaded.file.format;
        Ok(Self::launch(loaded.state, loaded.file, node, config).await)
    }

    /// `WalletBackend::save`: writes the state (format B: the sidecar). It waits for a save
    /// that is running and for a send, fusion or delayed transaction operation that is
    /// running; every file is replaced in one step (see [`crate::autosave`]).
    pub async fn save(&self) -> Result<(), WalletError> {
        autosave::save(&self.shared, false, true).await.map(|_| ())
    }

    /// Saves by itself while open, as `config` says ([`AutosaveConfig`]); `None` stops it.
    /// Off until this is called. Call it within the Tokio runtime the wallet runs on.
    pub fn set_autosave(&self, config: Option<AutosaveConfig>) {
        let task = config.map(|c| AutosaveTask::start(&self.shared, c));
        // the old task finishes a save it is writing, then ends
        *self.autosave.lock() = task;
    }

    /// Stops the autosave and syncing, then saves (`~WalletBackend`, wallet-api
    /// `DELETE /wallet`).
    pub async fn close(self) -> Result<(), WalletError> {
        let autosave = self.autosave.lock().take();
        if let Some(task) = autosave {
            task.stop().await;
        }
        self.stop_sync().await;
        self.save().await
    }

    /// Starts the sync tasks if they are not running.
    pub fn start_sync(&self) {
        let mut tasks = self.shared_tasks();
        if tasks.is_none() {
            *tasks = Some(SyncTasks::start(&self.shared));
        }
    }

    /// Stops the sync tasks and waits for them.
    pub async fn stop_sync(&self) {
        let tasks = self.shared_tasks().take();
        if let Some(tasks) = tasks {
            tasks.stop().await;
        }
    }

    fn shared_tasks(&self) -> parking_lot::MutexGuard<'_, Option<SyncTasks>> {
        self.tasks.lock()
    }

    /// `WalletBackend::reset`: forgets transactions, outputs and progress and scans again from
    /// `scan_height`; then saves.
    pub async fn reset(&self, scan_height: u64) -> Result<(), WalletError> {
        self.shared.state.lock().reset(scan_height);
        self.after_reset(scan_height);
        self.save().await
    }

    /// `WalletBackend::reset(scanHeight, timestamp)`: a non-zero `timestamp` is turned into a
    /// scan height (`utilities::timestampToScanHeight`), which then replaces `scan_height`; the
    /// wallet needs a fixed height to cut its transactions at. Then as [`Self::reset`].
    pub async fn reset_from(&self, scan_height: u64, timestamp: u64) -> Result<(), WalletError> {
        let height = if timestamp == 0 {
            scan_height
        } else {
            timestamp_to_scan_height(timestamp)
        };
        self.reset(height).await
    }

    fn after_reset(&self, scan_height: u64) {
        self.shared.block_hashes.lock().clear();
        self.shared.emit(WalletEvent::Reset {
            height: scan_height,
        });
    }

    /// `WalletBackend::swapNode`: talks to `node` from now on (sync continues from the stored
    /// progress).
    pub async fn swap_node(&self, node: Node<T>) {
        *self.shared.node.write() = Arc::new(node);
        *self.shared.daemon.write() = DaemonStatus::default();
        self.shared.state.lock().generation += 1;
        let _ = sync::refresh_info(&self.shared).await;
        sync::refresh_fee(&self.shared).await;
    }

    /// wallet-api `GET /node`: the daemon address and its fee (`getNodeAddress`,
    /// `getNodeFee`).
    #[must_use]
    pub fn node(&self) -> NodeInfo {
        let node = self.shared.node();
        let fee = self.shared.daemon.read().payable_fee();
        NodeInfo {
            host: node.host.clone(),
            port: node.port,
            fee_address: fee.map_or_default(|(a, _)| a.to_string()),
            fee_amount: fee.map_or(0, |(_, amount)| amount),
        }
    }

    /// The cached daemon status.
    #[must_use]
    pub fn daemon_status(&self) -> DaemonStatus {
        self.shared.daemon.read().clone()
    }

    /// The wallet file.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.shared.file.lock().path.clone()
    }

    /// The wallet file format.
    #[must_use]
    pub fn format(&self) -> FileFormat {
        self.shared.file.lock().format
    }

    /// Events: transactions, sends, cancellations, rollbacks, sync and daemon state.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<WalletEvent> {
        self.shared.events.subscribe()
    }

    /// `WalletBackend::changePassword`: saves with the new password (format B: the container
    /// too).
    pub async fn change_password(&self, password: &str) -> Result<(), WalletError> {
        autosave::change_password(&self.shared, password).await
    }

    /// walletd `export`: writes the keys to a new container at `path` with the wallet's
    /// password (keys only; C++ walletd rescans it).
    pub async fn export_container(&self, path: impl AsRef<Path>) -> Result<(), WalletError> {
        self.export_container_with_history(path, None).await
    }

    /// walletd `export` (`WalletGreen::exportWallet`): writes the keys to a new container at
    /// `path` with the wallet's password, and `history` (walletd's transactions and
    /// transfers, see `xkr_wallet_files::green::cache::encode_cache`) as its cache. C++
    /// walletd shows that history and rescans the outputs.
    pub async fn export_container_with_history(
        &self,
        path: impl AsRef<Path>,
        history: Option<GreenHistory>,
    ) -> Result<(), WalletError> {
        let path = path.as_ref().to_path_buf();
        let file = self.shared.file.lock().clone();
        let mut container = persist::green::green_from_state(
            &self.shared.state.lock(),
            file.view_timestamp,
            unix_now(),
        );
        container.history = history;
        blocking(move || persist::write_container_with_cache(&path, &file.password, &container))
            .await
    }

    // -----------------------------------------------------------------------------------------
    // subwallets

    /// `WalletBackend::addSubWallet`: a new address with a random spend key (not deterministic,
    /// as in C++); returns the address and its private spend key. Format B rewrites the
    /// container and saves.
    pub async fn add_subwallet(&self) -> Result<(String, SecretKey), WalletError> {
        let (_, secret) = generate_keys(&mut os_rng());
        let address = self.shared.state.lock().add_subwallet(secret, unix_now())?;
        self.subwallets_changed().await?;
        Ok((address, secret))
    }

    /// `WalletBackend::importSubWallet`: if the wallet already scanned past `scan_height`, the
    /// whole wallet scans again from there (as C++ does).
    pub async fn import_subwallet(
        &self,
        spend_secret: SecretKey,
        scan_height: u64,
    ) -> Result<String, WalletError> {
        let mut addresses = self
            .import_subwallets(std::slice::from_ref(&spend_secret), scan_height)
            .await?;
        Ok(addresses.remove(0))
    }

    /// walletd `createAddressList` (`WalletGreen::createAddressList`): imports every key with
    /// `scan_height`, with one rescan for the whole list when the wallet already scanned past
    /// `scan_height`, and one save. Every key is checked first; on an error nothing is added.
    /// Returns the addresses in the order of the keys.
    pub async fn import_subwallets(
        &self,
        spend_secrets: &[SecretKey],
        scan_height: u64,
    ) -> Result<Vec<String>, WalletError> {
        let (addresses, reset) = {
            let mut s = self.shared.state.lock();
            let addresses = s.import_subwallets(spend_secrets, scan_height)?;
            let reset = !addresses.is_empty() && s.sync.height() >= scan_height;
            if reset {
                s.reset(scan_height);
            }
            (addresses, reset)
        };
        if reset {
            self.after_reset(scan_height);
        }
        if !addresses.is_empty() {
            self.subwallets_changed().await?;
        }
        Ok(addresses)
    }

    /// `WalletBackend::importViewSubWallet` (view wallets only).
    pub async fn import_view_subwallet(
        &self,
        spend_public: PublicKey,
        scan_height: u64,
    ) -> Result<String, WalletError> {
        let (address, reset) = {
            let mut s = self.shared.state.lock();
            let address = s.import_view_subwallet(spend_public, scan_height)?;
            let reset = s.sync.height() >= scan_height;
            if reset {
                s.reset(scan_height);
            }
            (address, reset)
        };
        if reset {
            self.after_reset(scan_height);
        }
        self.subwallets_changed().await?;
        Ok(address)
    }

    /// `WalletBackend::deleteSubWallet` (not the primary address).
    pub async fn delete_subwallet(&self, address: &str) -> Result<(), WalletError> {
        {
            let mut s = self.shared.state.lock();
            let key = our_key(&s, address)?;
            s.delete_subwallet(&key)?;
        }
        self.subwallets_changed().await
    }

    /// walletd `deleteAddress` (`WalletGreen::deleteAddress`): removes any address, also the
    /// primary or the only one. The next address becomes the primary one; a wallet left
    /// without addresses stops syncing until one is added (and then scans for it from its
    /// start).
    pub async fn delete_address(&self, address: &str) -> Result<(), WalletError> {
        {
            let mut s = self.shared.state.lock();
            let key = our_key(&s, address)?;
            s.delete_address(&key)?;
        }
        self.shared.block_hashes.lock().clear();
        self.subwallets_changed().await
    }

    /// Format B keeps the container's keys and the sidecar in step with the subwallets
    /// (container first, then the state); format A saves later.
    async fn subwallets_changed(&self) -> Result<(), WalletError> {
        if self.format() != FileFormat::WalletGreen {
            self.shared.mark_changed(false);
            return Ok(());
        }
        autosave::save(&self.shared, true, true).await.map(|_| ())
    }

    /// `WalletBackend::getPrimaryAddress`; empty for a walletd container without addresses.
    #[must_use]
    pub fn primary_address(&self) -> String {
        self.shared
            .state
            .lock()
            .try_primary()
            .map_or_default(|s| s.address.clone())
    }

    /// `WalletBackend::getAddresses`, in creation order.
    #[must_use]
    pub fn addresses(&self) -> Vec<String> {
        self.shared
            .state
            .lock()
            .subwallets()
            .iter()
            .map(|s| s.address.clone())
            .collect()
    }

    /// `WalletBackend::getWalletCount`.
    #[must_use]
    pub fn subwallet_count(&self) -> usize {
        self.shared.state.lock().subwallets().len()
    }

    /// `WalletBackend::isViewWallet`.
    #[must_use]
    pub fn is_view_wallet(&self) -> bool {
        self.shared.state.lock().is_view_wallet()
    }

    // -----------------------------------------------------------------------------------------
    // balances and transactions

    /// The height the C++ wallets use for unlock checks: `Nigel::networkBlockCount`.
    fn network_height(&self) -> u64 {
        self.shared.daemon.read().network_block_count()
    }

    /// `WalletBackend::getTotalBalance` (`None`) or `getBalance(address)`.
    pub fn balance(&self, address: Option<&str>) -> Result<Balance, WalletError> {
        let height = self.network_height();
        let now = unix_now();
        let s = self.shared.state.lock();
        match address {
            None => Ok(s.balance(None, height, now)),
            Some(address) => {
                let key = our_key(&s, address)?;
                Ok(s.balance(Some(&[key]), height, now))
            }
        }
    }

    /// `WalletBackend::getBalances`: per address, in creation order.
    #[must_use]
    pub fn balances(&self) -> Vec<AddressBalance> {
        let height = self.network_height();
        self.shared.state.lock().balances(height, unix_now())
    }

    /// `WalletBackend::getTransactions` / `getTransactionsRange`: the confirmed transactions
    /// with `range.start <= blockHeight < range.end` (all for `None`), and only those touching
    /// `address` if given.
    pub fn transactions(
        &self,
        range: Option<Range<u64>>,
        address: Option<&str>,
    ) -> Result<Vec<Transaction>, WalletError> {
        let (start, end) = range.map_or((0, u64::MAX), |r| (r.start, r.end));
        let s = self.shared.state.lock();
        let key = address.map(|a| our_key(&s, a)).transpose()?;
        Ok(s.transactions()
            .iter()
            .filter(|t| start <= t.block_height && t.block_height < end)
            .filter(|t| key.is_none_or(|k| t.involves(&k)))
            .cloned()
            .collect())
    }

    /// `WalletBackend::getUnconfirmedTransactions`: sent transactions not in a block yet
    /// (incoming pool transactions are not tracked), only those touching `address` if given.
    pub fn unconfirmed(&self, address: Option<&str>) -> Result<Vec<Transaction>, WalletError> {
        let s = self.shared.state.lock();
        let key = address.map(|a| our_key(&s, a)).transpose()?;
        Ok(s.locked_transactions()
            .iter()
            .filter(|t| key.is_none_or(|k| t.involves(&k)))
            .cloned()
            .collect())
    }

    /// Sent transactions the node forgot (walletd state `CANCELLED`), only those touching
    /// `address` if given.
    pub fn cancelled(&self, address: Option<&str>) -> Result<Vec<Transaction>, WalletError> {
        let s = self.shared.state.lock();
        let key = address.map(|a| our_key(&s, a)).transpose()?;
        Ok(s.cancelled_transactions()
            .iter()
            .filter(|t| key.is_none_or(|k| t.involves(&k)))
            .cloned()
            .collect())
    }

    /// A transaction by hash with its walletd state: confirmed or pending (`SUCCEEDED`),
    /// cancelled (`CANCELLED`) or delayed (`CREATED`).
    #[must_use]
    pub fn transaction(&self, hash: &Hash) -> Option<TransactionInfo> {
        let s = self.shared.state.lock();
        if let Some(info) = s.find_transaction(hash) {
            return Some(info);
        }
        s.delayed.record(hash).map(|transaction| TransactionInfo {
            transaction,
            state: TxState::Created,
        })
    }

    /// What each subwallet received in each of `hashes`: the sum of the wallet's outputs (unspent,
    /// locked or spent) that the transaction created, per public spend key. A transaction with
    /// no own output (or whose spent outputs were pruned) is missing.
    #[must_use]
    pub fn received_in(
        &self,
        hashes: &[Hash],
    ) -> std::collections::HashMap<Hash, Vec<(PublicKey, u64)>> {
        let wanted: std::collections::HashSet<&Hash> = hashes.iter().collect();
        let mut out: std::collections::HashMap<Hash, Vec<(PublicKey, u64)>> =
            std::collections::HashMap::new();
        let s = self.shared.state.lock();
        for sub in s.subwallets() {
            for input in sub.unspent.iter().chain(&sub.locked).chain(&sub.spent) {
                if !wanted.contains(&input.parent_transaction_hash) {
                    continue;
                }
                let list = out.entry(input.parent_transaction_hash).or_default();
                match list.iter_mut().find(|(k, _)| *k == sub.public_spend_key) {
                    Some(entry) => entry.1 = entry.1.saturating_add(input.amount),
                    None => list.push((sub.public_spend_key, input.amount)),
                }
            }
        }
        out
    }

    /// `WalletBackend::getTxPrivateKey`.
    pub fn tx_private_key(&self, hash: &Hash) -> Result<SecretKey, WalletError> {
        self.shared
            .state
            .lock()
            .tx_private_key(hash)
            .ok_or_else(|| ErrorCode::TxPrivateKeyNotFound.into())
    }

    // -----------------------------------------------------------------------------------------
    // keys

    /// `WalletBackend::getPrivateViewKey` (wallet-api `/keys`).
    #[must_use]
    pub fn keys(&self) -> SecretKey {
        self.shared.state.lock().view_secret()
    }

    /// The shared public view key.
    #[must_use]
    pub fn view_public_key(&self) -> PublicKey {
        self.shared.state.lock().view_public()
    }

    /// `WalletBackend::getSpendKeys(address)`: public and private spend key.
    pub fn spend_keys(&self, address: &str) -> Result<(PublicKey, SecretKey), WalletError> {
        let s = self.shared.state.lock();
        if s.is_view_wallet() {
            return Err(ErrorCode::IllegalViewWalletOperation.into());
        }
        let key = our_key(&s, address)?;
        let secret = s
            .subwallet(&key)
            .and_then(|sub| sub.private_spend_key)
            .ok_or_else(|| WalletError::new(ErrorCode::IllegalViewWalletOperation))?;
        Ok((key, secret))
    }

    /// `WalletBackend::getMnemonicSeedForAddress`: only for an address whose spend key gives
    /// the wallet's view key, else `KEYS_NOT_DETERMINISTIC`.
    pub fn mnemonic(&self, address: &str) -> Result<String, WalletError> {
        let (_, secret) = self.spend_keys(address)?;
        if !is_deterministic(&secret, &self.keys()) {
            return Err(ErrorCode::KeysNotDeterministic.into());
        }
        Ok(mnemonic_from_secret(&secret))
    }

    /// `WalletBackend::getStatus`.
    #[must_use]
    pub fn status(&self) -> WalletStatus {
        let d = self.shared.daemon.read().clone();
        let s = self.shared.state.lock();
        WalletStatus {
            wallet_block_count: s.sync_status().height(),
            local_daemon_block_count: d.local_block_count(),
            network_block_count: d.network_block_count(),
            peer_count: d.peer_count(),
            hashrate: d.hashrate(),
            is_view_wallet: s.is_view_wallet(),
            subwallet_count: s.subwallets().len(),
            last_block_hash: s.sync_status().last_hash(),
            daemon_online: d.online,
        }
    }

    // -----------------------------------------------------------------------------------------
    // sending

    /// Relays a prepared transaction and records it.
    async fn relay_and_store(&self, prepared: &Prepared) -> Result<Hash, WalletError> {
        let node = self.shared.node();
        let hash = relay::relay(&node.client, &prepared.built.blob, prepared.built.hash).await?;
        let tx = send::store_sent(&mut self.shared.state.lock(), prepared);
        self.shared.mark_changed(true);
        self.shared.emit(WalletEvent::Sent(tx));
        Ok(hash)
    }

    /// `WalletBackend::sendTransactionBasic`: one destination (an integrated address carries
    /// its payment id), default fee and mixin, change to the primary address.
    pub async fn send_basic(
        &self,
        destination: &str,
        amount: u64,
        payment_id: &str,
    ) -> Result<Hash, WalletError> {
        self.send_advanced(SendRequest::basic(destination, amount, payment_id))
            .await
    }

    /// Where the node fee goes among the destinations of a transfer: first, as walletd
    /// (`convertWalletRpcOrdersToWalletOrders`) puts it, or last (the default), as
    /// `WalletBackend` (`sendTransactionAdvanced`) appends it.
    pub fn set_node_fee_first(&self, first: bool) {
        self.shared.node_fee_first.store(first, Ordering::Relaxed);
    }

    /// How many blocks an output waits after the block holding it before it is offered to
    /// sends and counted as unlocked: 0 (the default) as `WalletBackend` (wallet-api,
    /// zedwallet++), which only applies the unlock time; walletd's `WalletGreen` uses 1
    /// (`transactionSoftLockTime`): an output of the last processed block counts as locked.
    pub fn set_spendable_age(&self, blocks: u64) {
        self.shared.state.lock().spendable_age = blocks;
    }

    /// `WalletBackend::sendTransactionAdvanced`.
    pub async fn send_advanced(&self, request: SendRequest) -> Result<Hash, WalletError> {
        let _guard = self.shared.send_lock.lock().await;
        let prepared = send::prepare_transfer(&self.shared, &request).await?;
        self.relay_and_store(&prepared).await
    }

    /// `WalletBackend::sendFusionTransactionBasic`: default mixin, all subwallets, to the
    /// primary address.
    pub async fn fusion_basic(&self) -> Result<Hash, WalletError> {
        self.fusion_advanced(FusionRequest::default()).await
    }

    /// `WalletBackend::sendFusionTransactionAdvanced`, or walletd `sendFusionTransaction` when
    /// [`FusionRequest::threshold`] is set.
    pub async fn fusion_advanced(&self, request: FusionRequest) -> Result<Hash, WalletError> {
        let _guard = self.shared.send_lock.lock().await;
        let prepared = fusion::prepare_fusion(&self.shared, &request).await?;
        self.relay_and_store(&prepared).await
    }

    /// walletd `estimateFusion`.
    pub fn estimate_fusion(
        &self,
        threshold: u64,
        addresses: &[String],
    ) -> Result<FusionEstimate, WalletError> {
        let height = self.network_height();
        fusion::estimate(
            &self.shared.state.lock(),
            threshold,
            addresses,
            height,
            unix_now(),
        )
    }

    // -----------------------------------------------------------------------------------------
    // delayed transactions (walletd)

    /// walletd `createDelayedTransaction`: builds and signs, does not relay; the inputs are set
    /// aside until the transaction is sent or deleted.
    pub async fn delayed_create(&self, request: SendRequest) -> Result<Hash, WalletError> {
        let _guard = self.shared.send_lock.lock().await;
        let prepared = send::prepare_transfer(&self.shared, &request).await?;
        let hash = prepared.built.hash;
        {
            let mut state = self.shared.state.lock();
            if !DelayedStore::inputs_unspent(&state, &prepared) {
                return Err(WalletError::new(ErrorCode::NotEnoughBalance)
                    .with_detail("an input was spent while the transaction was built"));
            }
            state.delayed_insert(prepared);
        }
        self.shared.mark_changed(false);
        Ok(hash)
    }

    /// walletd `getDelayedTransactionHashes`.
    #[must_use]
    pub fn delayed_list(&self) -> Vec<Hash> {
        self.shared.state.lock().delayed.hashes()
    }

    /// walletd `sendDelayedTransaction`. A transaction the node refuses stays in the list.
    pub async fn delayed_send(&self, hash: &Hash) -> Result<Hash, WalletError> {
        let _guard = self.shared.send_lock.lock().await;
        let prepared = {
            let mut state = self.shared.state.lock();
            let prepared = state.delayed.take(hash).ok_or_else(delayed_not_found)?;
            if !DelayedStore::inputs_unspent(&state, &prepared) {
                for (_, input) in &prepared.inputs {
                    state.reserved.remove(&input.key_image);
                }
                return Err(WalletError::new(ErrorCode::NotEnoughBalance)
                    .with_detail("an input of the delayed transaction is spent"));
            }
            prepared
        };
        match self.relay_and_store(&prepared).await {
            Ok(hash) => Ok(hash),
            Err(e) => {
                self.shared.state.lock().delayed.restore(prepared);
                Err(e)
            }
        }
    }

    /// walletd `deleteDelayedTransaction`.
    pub fn delayed_delete(&self, hash: &Hash) -> Result<(), WalletError> {
        if self.shared.state.lock().delayed_delete(hash) {
            self.shared.mark_changed(false);
            Ok(())
        } else {
            Err(delayed_not_found())
        }
    }

    // -----------------------------------------------------------------------------------------
    // blocks and extra (walletd)

    /// walletd `getBlockHashes`: hashes of the blocks `start..start + count` that the wallet
    /// has processed (fewer at the wallet's height), from the sync progress, a cache, or the
    /// daemon's headers.
    pub async fn block_hashes(&self, start: u64, count: u64) -> Result<Vec<Hash>, WalletError> {
        let (top, empty) = {
            let s = self.shared.state.lock();
            (s.sync_status().height(), s.sync_status().is_empty())
        };
        if empty {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(count).min(top + 1);
        if start >= end {
            return Ok(Vec::new());
        }
        let missing: Vec<u64> = {
            let s = self.shared.state.lock();
            let cache = self.shared.block_hashes.lock();
            (start..end)
                .filter(|h| s.sync_status().hash_at(*h).is_none() && !cache.contains_key(h))
                .collect()
        };
        let node = self.shared.node();
        for chunk in contiguous_chunks(&missing, 1000) {
            let headers = node
                .client
                .block_headers_range(chunk.start, chunk.end - 1)
                .await
                .map_err(|e| daemon_offline(&e))?;
            let mut cache = self.shared.block_hashes.lock();
            for header in headers {
                cache.insert(header.height, header.hash);
            }
        }
        let s = self.shared.state.lock();
        let cache = self.shared.block_hashes.lock();
        (start..end)
            .map(|h| {
                s.sync_status()
                    .hash_at(h)
                    .or_else(|| cache.get(&h).copied())
                    .ok_or_else(|| {
                        WalletError::new(ErrorCode::DaemonError)
                            .with_detail(format!("no hash for block {h}"))
                    })
            })
            .collect()
    }

    /// The index of the block with `hash` on the daemon's main chain (walletd
    /// `getTransactions` by `blockHash`); `None` if the node does not know it.
    pub async fn block_height_of(&self, hash: &Hash) -> Result<Option<u64>, WalletError> {
        if let Some(height) = self
            .shared
            .block_hashes
            .lock()
            .iter()
            .find(|(_, h)| *h == hash)
            .map(|(height, _)| *height)
        {
            return Ok(Some(height));
        }
        let node = self.shared.node();
        match node.client.block_header_by_hash(hash).await {
            Ok(header) if !header.orphan_status => {
                self.shared
                    .block_hashes
                    .lock()
                    .insert(header.height, header.hash);
                Ok(Some(header.height))
            }
            Ok(_) | Err(DaemonError::Rpc { .. }) => Ok(None),
            Err(e) => Err(daemon_offline(&e)),
        }
    }

    /// The full `extra` of a wallet transaction (walletd `extra`), fetched with
    /// `/gettransactions` the first time and kept with the transaction.
    pub async fn transaction_extra(&self, hash: &Hash) -> Result<Vec<u8>, WalletError> {
        let known = {
            let s = self.shared.state.lock();
            match s.find_transaction(hash) {
                Some(info) => info.transaction.extra,
                None => match s.delayed.get(hash) {
                    Some(p) => Some(p.built.tx.prefix.extra.clone()),
                    None => return Err(transaction_not_found()),
                },
            }
        };
        if let Some(extra) = known {
            return Ok(extra);
        }
        let node = self.shared.node();
        let answer = node
            .client
            .transactions(std::slice::from_ref(hash))
            .await
            .map_err(|e| daemon_offline(&e))?;
        let blob = answer.blobs.first().ok_or_else(transaction_not_found)?;
        let tx = decode_transaction(blob)
            .map_err(|e| WalletError::new(ErrorCode::DaemonError).with_detail(e.to_string()))?;
        let extra = tx.prefix.extra;
        if let Some(record) = self.shared.state.lock().find_transaction_mut(hash) {
            record.extra = Some(extra.clone());
            self.shared.mark_changed(false);
        }
        Ok(extra)
    }
}

/// A delayed transaction that is not in the list (walletd `OBJECT_NOT_FOUND`).
fn delayed_not_found() -> WalletError {
    WalletError::with_message(
        ErrorCode::HashInvalid,
        "No delayed transaction with this hash.",
    )
    .with_walletd_code(walletd_codes::OBJECT_NOT_FOUND)
}

/// A transaction the wallet does not have (walletd `OBJECT_NOT_FOUND`).
fn transaction_not_found() -> WalletError {
    WalletError::with_message(ErrorCode::HashInvalid, "No transaction with this hash.")
        .with_walletd_code(walletd_codes::OBJECT_NOT_FOUND)
}

/// Splits sorted heights into ranges of consecutive heights, at most `max` long.
fn contiguous_chunks(heights: &[u64], max: u64) -> Vec<Range<u64>> {
    let mut out: Vec<Range<u64>> = Vec::new();
    for &h in heights {
        match out.last_mut() {
            Some(r) if r.end == h && r.end - r.start < max => r.end += 1,
            _ => out.push(Range {
                start: h,
                end: h.saturating_add(1),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks() {
        assert_eq!(
            contiguous_chunks(&[1, 2, 3, 7, 8, 10], 2),
            vec![1..3, 3..4, 7..9, 10..11]
        );
        assert_eq!(contiguous_chunks(&[], 5).len(), 0);
    }
}
