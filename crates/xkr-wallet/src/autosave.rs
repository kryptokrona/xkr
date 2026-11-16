//! Saving: what a save writes and in which order, and the wallet saving by itself while it is
//! open (autosave).
//!
//! The C++ tools save on request and when they close (wallet-api `PUT /save`, the
//! `WalletBackend` destructor; walletd `save` and its shutdown). An open wallet here can also
//! save by itself ([`AutosaveConfig`], turned on with [`crate::Wallet::set_autosave`]):
//! * when something changed and [`AutosaveConfig::interval`] passed since the last save;
//! * right after a transaction was recorded (received, sent or cancelled);
//! * after sync processed [`AutosaveConfig::after_blocks`] blocks since the last save;
//!
//! but never sooner than [`AutosaveConfig::min_gap`] after the last save. The saving runs in
//! its own task, not in the sync tasks, and starts only between sends, fusions and delayed
//! transaction operations: it takes the state while no such operation runs, then writes
//! without holding them up.
//!
//! # What a save writes, and crash safety
//!
//! Every file is replaced in one step (`xkr_wallet_files::atomic_write`: temporary file in the
//! same directory, `fsync`, rename, `fsync` of the directory), so a save stopped at any point
//! leaves the old or the new file. Saves never run at the same time (`save_lock`).
//!
//! Format B keeps two files, the container (keys) and `<container>.xkrstate` (the state).
//! When the keys changed the container is written first, then the state; otherwise only the
//! state. A state file is used only when it belongs to the container (same view key, the same
//! addresses with the same spend keys); otherwise the wallet starts from the container and
//! scans again. So every stopping point opens consistently:
//! * before the container is replaced: the old pair;
//! * after the container, before the state: the new keys, the old state does not belong to
//!   them and is rebuilt by a scan (no address is lost; the keys come from the container);
//! * after both: the new pair.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use xkr_daemon_client::Transport;
use xkr_wallet_files::backend::WalletBackendJson;
use xkr_wallet_files::green::GreenWallet;
use zeroize::Zeroizing;

use crate::error::{ErrorCode, WalletError};
use crate::persist::{self, FileFormat, WalletFile};
use crate::state::unix_now;
use crate::wallet::Shared;

/// When an open wallet saves by itself ([`crate::Wallet::set_autosave`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutosaveConfig {
    /// Save when something changed and this much time passed since the last save (5 minutes).
    pub interval: Duration,
    /// Save right after a transaction was recorded: received, sent or cancelled (on).
    pub on_transaction: bool,
    /// Save after sync processed this many blocks since the last save (1 000).
    pub after_blocks: u64,
    /// Never save sooner than this after the last save (10 seconds).
    pub min_gap: Duration,
}

impl Default for AutosaveConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_mins(5),
            on_transaction: true,
            after_blocks: 1_000,
            min_gap: Duration::from_secs(10),
        }
    }
}

/// What the last save wrote, to tell whether there is something new.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SaveMark {
    /// [`Shared::changes`] when the state was taken.
    pub changes: u64,
    /// [`Shared::tx_changes`] when the state was taken.
    pub tx_changes: u64,
    /// Sync height when the state was taken.
    pub height: u64,
    /// When the save finished (or the wallet was opened).
    pub at: Instant,
}

/// What one save writes, taken from the state in one step.
struct Snapshot {
    file: WalletFile,
    json: WalletBackendJson,
    container: Option<GreenWallet>,
    mark: SaveMark,
}

/// Takes the state to write (and, for format B with `container`, the container).
fn snapshot<T>(shared: &Shared<T>, container: bool) -> Snapshot {
    let file = shared.file.lock().clone();
    let state = shared.state.lock();
    let json = persist::backend::state_to_json(&state);
    let container = (container && file.format == FileFormat::WalletGreen)
        .then(|| persist::green::green_from_state(&state, file.view_timestamp, unix_now()));
    let mark = SaveMark {
        changes: shared.changes.load(Ordering::SeqCst),
        tx_changes: shared.tx_changes.load(Ordering::SeqCst),
        height: state.sync_status().height(),
        at: Instant::now(),
    };
    Snapshot {
        file,
        json,
        container,
        mark,
    }
}

/// Writes a snapshot: the container first (if any), then the state.
async fn write(snapshot: Snapshot) -> Result<SaveMark, WalletError> {
    let Snapshot {
        file,
        json,
        container,
        mut mark,
    } = snapshot;
    tokio::task::spawn_blocking(move || {
        if let Some(container) = &container {
            persist::write_container(&file.path, &file.password, container)?;
        }
        persist::save(&file, &json)
    })
    .await
    .map_err(|e| WalletError::new(ErrorCode::InvalidWalletFilename).with_detail(e.to_string()))??;
    mark.at = Instant::now();
    Ok(mark)
}

/// One save (`WalletBackend::save`): waits for other saves, takes the state between sends
/// (waiting for a running one when `wait_for_sends`, else giving up: `Ok(false)`), writes it
/// (with the container when `container`) and records the save.
pub(crate) async fn save<T>(
    shared: &Shared<T>,
    container: bool,
    wait_for_sends: bool,
) -> Result<bool, WalletError> {
    let _save = shared.save_lock.lock().await;
    save_locked(shared, container, wait_for_sends).await
}

/// `WalletBackend::changePassword`: the new password and the save with it in one step, so
/// no other save writes one file with the old and the other with the new password.
pub(crate) async fn change_password<T>(
    shared: &Shared<T>,
    password: &str,
) -> Result<(), WalletError> {
    let _save = shared.save_lock.lock().await;
    shared.file.lock().password = Zeroizing::new(password.to_owned());
    save_locked(shared, true, true).await.map(|_| ())
}

async fn save_locked<T>(
    shared: &Shared<T>,
    container: bool,
    wait_for_sends: bool,
) -> Result<bool, WalletError> {
    let snapshot = {
        let _send = if wait_for_sends {
            shared.send_lock.lock().await
        } else {
            match shared.send_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(false),
            }
        };
        snapshot(shared, container)
    };
    let mark = write(snapshot).await?;
    *shared.saved.lock() = mark;
    Ok(true)
}

/// Whether an autosave is due now.
fn due<T>(shared: &Shared<T>, config: &AutosaveConfig) -> bool {
    let mark = *shared.saved.lock();
    let changes = shared.changes.load(Ordering::SeqCst);
    if changes == mark.changes {
        return false;
    }
    let elapsed = mark.at.elapsed();
    if elapsed < config.min_gap {
        return false;
    }
    let height = shared.state.lock().sync_status().height();
    elapsed >= config.interval
        || (config.on_transaction && shared.tx_changes.load(Ordering::SeqCst) != mark.tx_changes)
        || height.abs_diff(mark.height) >= config.after_blocks
}

/// The autosave task of an open wallet.
#[derive(Debug)]
pub(crate) struct AutosaveTask {
    stop: watch::Sender<bool>,
    handle: Option<JoinHandle<()>>,
}

impl AutosaveTask {
    /// Starts saving by `config`.
    pub(crate) fn start<T: Transport + 'static>(
        shared: &Arc<Shared<T>>,
        config: AutosaveConfig,
    ) -> Self {
        let (stop, stop_rx) = watch::channel(false);
        let handle = tokio::spawn(run(Arc::clone(shared), config, stop_rx));
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the task and waits for a save it is writing.
    pub(crate) async fn stop(mut self) {
        let _ = self.stop.send(true);
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for AutosaveTask {
    /// Asks the task to stop; a save it is writing is finished first (never cut off, so no
    /// older state can be written after a newer one).
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn run<T: Transport + 'static>(
    shared: Arc<Shared<T>>,
    config: AutosaveConfig,
    mut stop: watch::Receiver<bool>,
) {
    let tick = config
        .interval
        .min(config.min_gap)
        .clamp(Duration::from_millis(10), Duration::from_secs(1));
    loop {
        tokio::select! {
            () = shared.changed.notified() => {}
            () = tokio::time::sleep(tick) => {}
            changed = stop.changed() => if changed.is_err() { return },
        }
        if *stop.borrow() {
            return;
        }
        if !due(&shared, &config) {
            continue;
        }
        match save(&shared, false, false).await {
            Ok(true) => tracing::debug!("wallet saved"),
            Ok(false) => {}
            Err(e) => tracing::warn!("saving the wallet failed: {e}"),
        }
    }
}
