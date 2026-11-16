//! Closing the wallet (`src/zedwallet++/zed_wallet.cpp` `shutdown` and `cleanup`, and the
//! saving `~WalletBackend` destructor), the same way for `exit`, the end of the input, Ctrl+C
//! and SIGTERM.
//!
//! The steps, shown with [`Steps`]: stop syncing; wait for a transaction that is being sent
//! (a send is never cut in half, and no new one starts once closing has begun); save the wallet
//! file (a container keeps its keys and the engine's state goes to `<file>.xkrstate`); save
//! the address book if it changed and is not on disk yet; close the node connection. Files are
//! replaced in one step, so a failed save leaves the last saved file as it was.
//!
//! [`ShutdownHandle::interrupt`] is what a signal does: the first one closes the wallet, a
//! second one while that runs only says to wait, a third one gives up (the caller exits).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use tokio::runtime::Handle;
use xkr_daemon_client::Transport;
use xkr_wallet::{FileFormat, Wallet};
use xkr_wallet_files::AddressBook;

use crate::ask::error_text;
use crate::format::group;
use crate::input::{Out, Painter, write_to};
use crate::steps::Steps;

/// What a wallet format is called in the output.
pub(crate) const fn file_kind(format: FileFormat) -> &'static str {
    match format {
        FileFormat::WalletBackend => "wallet file",
        FileFormat::WalletBackendJsString => "wallet file, wallet-backend-js form",
        FileFormat::WalletGreen => "container",
    }
}

// ---------------------------------------------------------------------------------------------
// sends in flight

#[derive(Default)]
struct GateState {
    busy: bool,
    closed: bool,
    last: Option<Result<String, String>>,
}

/// Keeps closing and sending apart: closing waits for a running send and stops new ones.
#[derive(Default)]
pub(crate) struct SendGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl SendGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A send may start; false once the wallet is closing.
    pub(crate) fn begin(&self) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        state.busy = true;
        state.last = None;
        true
    }

    /// The send that [`SendGate::begin`] allowed is over: `Ok` with what was sent, `Err` with
    /// why nothing was.
    pub(crate) fn end(&self, outcome: Result<String, String>) {
        let mut state = self.lock();
        state.busy = false;
        state.last = Some(outcome);
        self.changed.notify_all();
    }

    /// No new send starts from now on; whether one is running.
    pub(crate) fn close(&self) -> bool {
        let mut state = self.lock();
        state.closed = true;
        state.busy
    }

    /// Waits until the running send is over; its outcome.
    pub(crate) fn wait(&self) -> Result<String, String> {
        let mut state = self.lock();
        while state.busy {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state
            .last
            .take()
            .unwrap_or_else(|| Ok("finished".to_owned()))
    }
}

// ---------------------------------------------------------------------------------------------
// the address book

/// Whether the address book changed in this session, and a copy that could not be written.
pub(crate) struct BookState {
    path: PathBuf,
    changed: AtomicBool,
    pending: Mutex<Option<AddressBook>>,
}

impl BookState {
    /// The book at `path`.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            changed: AtomicBool::new(false),
            pending: Mutex::new(None),
        }
    }

    /// The path of the book.
    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Writes `book`; on failure keeps it to try again when closing.
    pub(crate) fn save(&self, book: &AddressBook) -> bool {
        self.changed.store(true, Ordering::SeqCst);
        let saved = book.save(&self.path).is_ok();
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) =
            (!saved).then(|| book.clone());
        saved
    }

    /// For closing: `None` if the book did not change, else the outcome of writing what is
    /// not on disk yet.
    fn flush(&self) -> Option<Result<String, String>> {
        if !self.changed.load(Ordering::SeqCst) {
            return None;
        }
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let name = self
            .path
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        Some(match pending.take() {
            None => Ok(format!("{name} is up to date")),
            Some(book) => match book.save(&self.path) {
                Ok(()) => Ok(format!("{name}, {} entries", book.entries.len())),
                Err(e) => {
                    *pending = Some(book);
                    Err(e.to_string())
                }
            },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// the steps

/// Everything closing needs, shareable with the signal watcher.
pub(crate) struct Closer<T> {
    pub wallet: Arc<Wallet<T>>,
    pub location: String,
    pub rt: Handle,
    pub gate: Arc<SendGate>,
    pub book: Arc<BookState>,
    pub steps: Steps,
}

impl<T: Transport + 'static> Closer<T> {
    /// Runs the steps; whether the wallet file was saved.
    pub(crate) fn close(&self) -> bool {
        let s = &self.steps;
        let wallet = &self.wallet;
        s.title("Closing the wallet");
        let sending = self.gate.close();
        s.step("Stopping sync", || {
            // the only step that waits for the node: bounded (a request in flight)
            // the timer is made inside the runtime (the closer may run on a plain thread)
            let stopped = self.rt.block_on(async {
                tokio::time::timeout(SYNC_STOP_LIMIT, wallet.stop_sync()).await
            });
            if stopped.is_err() {
                return Err(format!(
                    "the node did not answer within {} s; sync stopped without it",
                    SYNC_STOP_LIMIT.as_secs()
                ));
            }
            Ok(format!(
                "at height {}",
                group(wallet.status().wallet_block_count)
            ))
        });
        if sending {
            s.step("Finishing the send", || self.gate.wait());
        }
        let format = wallet.format();
        let detail = if format == FileFormat::WalletGreen {
            let sidecar = xkr_wallet::sidecar_path(&wallet.path());
            let sidecar = sidecar
                .file_name()
                .map_or_default(|n| n.to_string_lossy().into_owned());
            format!(
                "{}, {}; state in {sidecar}",
                self.location,
                file_kind(format)
            )
        } else {
            format!("{}, {}", self.location, file_kind(format))
        };
        let saved = s.step("Saving the wallet file", || {
            self.rt
                .block_on(wallet.save())
                .map(|()| detail)
                .map_err(|e| error_text(&e))
        });
        if let Some(outcome) = self.book.flush() {
            s.step("Saving the address book", || outcome);
        }
        let node = wallet.node();
        s.step("Closing the node connection", || {
            Ok(if node.host.contains("://") {
                node.host.clone()
            } else {
                format!("{}:{}", node.host, node.port)
            })
        });
        s.end(
            saved,
            &format!(
                "{} on disk is the one saved last; the wallet scans the rest again when it is \
                 opened",
                self.location
            ),
        );
        saved
    }
}

// ---------------------------------------------------------------------------------------------
// exit and signals

/// Shown once when another stop signal arrives while the wallet closes.
pub const STOPPING_NOTE: &str = "closing cleanly — please wait, the wallet is being saved";

/// How long stopping the sync may wait for the node before its tasks are stopped anyway.
const SYNC_STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);

/// What [`ShutdownHandle::interrupt`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOutcome {
    /// No wallet is open: nothing to save.
    NoWallet,
    /// The wallet was closed; whether it was saved.
    Closed {
        /// The wallet file was saved.
        saved: bool,
    },
    /// Closing runs already: the second signal shows [`STOPPING_NOTE`] once.
    PleaseWait,
    /// Any later signal: nothing; the close always finishes.
    Ignored,
}

/// Closes the wallet.
pub(crate) type CloseFn = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Default)]
struct ShutdownState {
    closer: Option<CloseFn>,
    started: bool,
    finished: Option<bool>,
    signals: u32,
}

struct Shared {
    state: Mutex<ShutdownState>,
    done: Condvar,
    out: Out,
    paint: Painter,
}

/// Starts or joins the closing of the session's wallet; cloneable, for signal handlers and
/// tests.
#[derive(Clone)]
pub struct ShutdownHandle(Arc<Shared>);

impl std::fmt::Debug for ShutdownHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("ShutdownHandle")
            .field("wallet_open", &state.closer.is_some())
            .field("started", &state.started)
            .field("finished", &state.finished)
            .finish_non_exhaustive()
    }
}

impl ShutdownHandle {
    pub(crate) fn new(out: Out, paint: Painter) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(ShutdownState::default()),
            done: Condvar::new(),
            out,
            paint,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ShutdownState> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The wallet that closing saves.
    pub(crate) fn register(&self, closer: CloseFn) {
        self.lock().closer = Some(closer);
    }

    /// Writes to the session's output.
    pub(crate) fn write(&self, text: &str) {
        write_to(&self.0.out, text);
    }

    fn run_close(&self, closer: &CloseFn) -> bool {
        let saved = closer();
        self.lock().finished = Some(saved);
        self.0.done.notify_all();
        saved
    }

    /// The normal way out (`exit`, the end of the input): closes the wallet, or waits for the
    /// closing a signal started. Whether the wallet was saved (true without a wallet).
    pub(crate) fn close(&self) -> bool {
        let mut state = self.lock();
        let Some(closer) = state.closer.clone() else {
            return true;
        };
        if !state.started {
            state.started = true;
            drop(state);
            return self.run_close(&closer);
        }
        while state.finished.is_none() {
            state = self
                .0
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.finished.unwrap_or(false)
    }

    /// `back`: closes the wallet with the same steps as `exit`, then forgets it, so the start
    /// menu can open another one. Whether it was saved, and whether the program must quit
    /// instead: a signal came in while it closed (or had started closing it already).
    pub(crate) fn close_for_back(&self) -> (bool, bool) {
        let mut state = self.lock();
        let Some(closer) = state.closer.clone() else {
            return (true, false);
        };
        if state.started {
            while state.finished.is_none() {
                state = self
                    .0
                    .done
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            return (state.finished.unwrap_or(false), true);
        }
        state.started = true;
        drop(state);
        let saved = self.run_close(&closer);
        let mut state = self.lock();
        let quit = state.signals > 0;
        if !quit {
            *state = ShutdownState::default();
        }
        (saved, quit)
    }

    /// What Ctrl+C, SIGTERM or SIGHUP does. The first closes the wallet (like `exit`) and
    /// returns when it is saved; while that runs (or a close started otherwise) a second shows
    /// [`STOPPING_NOTE`] and later ones do nothing: the close always finishes.
    #[must_use]
    pub fn interrupt(&self) -> SignalOutcome {
        let mut state = self.lock();
        let Some(closer) = state.closer.clone() else {
            return SignalOutcome::NoWallet;
        };
        if !state.started {
            state.started = true;
            drop(state);
            return SignalOutcome::Closed {
                saved: self.run_close(&closer),
            };
        }
        state.signals += 1;
        let signals = state.signals;
        drop(state);
        if signals == 1 {
            let p = self.0.paint;
            let indent = if p.colour() { "  " } else { "" };
            self.write(&format!("{indent}{}\n", p.dim(STOPPING_NOTE)));
            SignalOutcome::PleaseWait
        } else {
            SignalOutcome::Ignored
        }
    }

    /// Whether closing has begun (input is no longer taken then).
    pub(crate) fn closing(&self) -> bool {
        self.lock().started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_waits_for_a_running_send() {
        let gate = Arc::new(SendGate::default());
        assert!(!gate.close());
        assert!(!gate.begin());
        let gate = Arc::new(SendGate::default());
        assert!(gate.begin());
        assert!(gate.close());
        let sender = Arc::clone(&gate);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            sender.end(Ok("sent".to_owned()));
        });
        assert_eq!(gate.wait(), Ok("sent".to_owned()));
        thread.join().unwrap();
        assert!(!gate.begin());
    }

    #[test]
    fn signals_close_once_then_ask_to_wait() {
        let sink: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
        let out: Out = Arc::new(Mutex::new(sink));
        let handle = ShutdownHandle::new(out, Painter::new(false));
        assert_eq!(handle.interrupt(), SignalOutcome::NoWallet);
        assert!(handle.close());
        let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        handle.register(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        }));
        assert_eq!(handle.interrupt(), SignalOutcome::Closed { saved: true });
        assert_eq!(handle.interrupt(), SignalOutcome::PleaseWait);
        assert_eq!(handle.interrupt(), SignalOutcome::Ignored);
        assert_eq!(handle.interrupt(), SignalOutcome::Ignored);
        // `exit` after a signal closed the wallet: no second close
        assert!(handle.close());
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn back_closes_and_forgets_the_wallet() {
        let sink: Box<dyn std::io::Write + Send> = Box::new(std::io::sink());
        let out: Out = Arc::new(Mutex::new(sink));
        let handle = ShutdownHandle::new(out, Painter::new(false));
        let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        let closer: CloseFn = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        });
        handle.register(Arc::clone(&closer));
        assert_eq!(handle.close_for_back(), (true, false));
        // nothing open now: a signal quits at once
        assert_eq!(handle.interrupt(), SignalOutcome::NoWallet);
        // the next wallet closes again
        handle.register(closer);
        assert_eq!(handle.interrupt(), SignalOutcome::Closed { saved: true });
        assert_eq!(handle.close_for_back(), (true, true));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
