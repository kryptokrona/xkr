//! Printing incoming transfers while the wallet waits for commands
//! (`src/zedwallet++/transaction_monitor.cpp` `TransactionMonitor`).
//!
//! A thread takes the engine's [`WalletEvent::Transaction`] events and prints the incoming,
//! non-fusion ones under the console lock, so nothing is printed while a command runs. While
//! paused (a reset prints what it finds itself) events are dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use xkr_daemon_client::Transport;
use xkr_wallet::{Wallet, WalletEvent};

use crate::format;
use crate::input::{Painter, Printer};

/// The running monitor.
#[derive(Debug)]
pub(crate) struct Monitor {
    stop: Arc<Notify>,
    paused: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    /// `TransactionMonitor::start`.
    pub(crate) fn start<T: Transport + 'static>(
        wallet: &Wallet<T>,
        rt: Handle,
        lock: Arc<Mutex<()>>,
        mut printer: Printer,
        paint: Painter,
    ) -> Self {
        let mut events = wallet.subscribe();
        let stop = Arc::new(Notify::new());
        let paused = Arc::new(AtomicBool::new(false));
        let (stop_rx, paused_rx) = (Arc::clone(&stop), Arc::clone(&paused));
        let thread = std::thread::spawn(move || {
            loop {
                let event = rt.block_on(async {
                    tokio::select! {
                        event = events.recv() => Some(event),
                        () = stop_rx.notified() => None,
                    }
                });
                let tx = match event {
                    None | Some(Err(RecvError::Closed)) => break,
                    Some(Ok(WalletEvent::Transaction(tx))) => tx,
                    Some(Ok(_) | Err(RecvError::Lagged(_))) => continue,
                };
                if paused_rx.load(Ordering::SeqCst) || tx.is_fusion() || tx.total_amount() <= 0 {
                    continue;
                }
                let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                printer(&format!(
                    "{}{}",
                    paint.info("\nNew transaction found!\n\n"),
                    format::incoming_transfer(paint, &tx)
                ));
            }
        });
        Self {
            stop,
            paused,
            thread: Some(thread),
        }
    }

    /// Drops events until resumed (`onTransaction.pause` / `resume`).
    pub(crate) fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    /// `TransactionMonitor::stop`, waiting for the thread.
    pub(crate) fn stop(mut self) {
        self.stop.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
