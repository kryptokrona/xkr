//! Runs xkrd as a sidecar: start it with its own data directory, wait for its RPC, restart it
//! with new mining flags, and stop it cleanly (SIGINT / Ctrl-C, wait for exit, kill only after
//! a timeout).
//!
//! Locks: `op` serialises start/stop/restart (held for seconds); `child`, `phase` and
//! `settings` are only held for short moments, so the UI's polling never waits on a restart.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

use crate::rpc;

/// How long xkrd may take to open its database and bind the RPC port.
const RPC_START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long xkrd may take to shut down after SIGINT (it waits at most 10 s for its Hyperswarm
/// transport, then makes a durable database commit).
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// The loader's search path variable through which xkrd finds libbare-kit in the bundled `lib/`
/// resource directory (Tauri puts `externalBin` binaries and resources in different places).
#[cfg(target_os = "macos")]
const LIBRARY_PATH_VAR: &str = "DYLD_LIBRARY_PATH";
#[cfg(windows)]
const LIBRARY_PATH_VAR: &str = "PATH";
#[cfg(not(any(target_os = "macos", windows)))]
const LIBRARY_PATH_VAR: &str = "LD_LIBRARY_PATH";

/// Settings kept in `<app data>/settings.json`. Fields missing from the file take their
/// defaults; 0 thread counts mean "half of the cores".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Run on testnet (data in `<app data>/xkrd-testnet`) instead of mainnet.
    pub testnet: bool,
    /// `--fast-sync`: skip proof of work up to the last checkpoint (the rest is verified).
    pub fast_sync: bool,
    /// `--verify-threads` (0 = half of the cores).
    pub verify_threads: usize,
    /// Hyperswarm transport (needs the bundled bridge).
    pub hyper: bool,
    /// Built-in miner.
    pub mining: Mining,
    /// Extra xkrd arguments, appended as they are.
    pub extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            testnet: false,
            fast_sync: true,
            verify_threads: 0,
            hyper: true,
            mining: Mining::default(),
            extra_args: Vec::new(),
        }
    }
}

/// Built-in miner settings (`--mine <address> --mine-threads N`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mining {
    pub enabled: bool,
    pub address: String,
    /// 0 = half of the cores.
    pub threads: usize,
}

#[derive(Debug, Clone, Serialize)]
struct Phase {
    /// `starting`, `running`, `stopping`, `stopped`, `exited`, `failed`.
    name: &'static str,
    detail: String,
}

#[derive(Debug)]
struct Paths {
    xkrd: PathBuf,
    /// libbare-kit, which xkrd loads at start.
    lib_dir: PathBuf,
    bridge_dir: PathBuf,
    app_data: PathBuf,
    settings_file: PathBuf,
    log_file: PathBuf,
}

/// The xkrd sidecar.
#[derive(Debug)]
pub struct NodeManager {
    paths: Paths,
    rpc_port: u16,
    cores: usize,
    exiting: AtomicBool,
    op: Mutex<()>,
    child: Mutex<Option<Child>>,
    phase: Mutex<Phase>,
    settings: Mutex<Settings>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl NodeManager {
    pub fn new(app: &AppHandle) -> Result<Self, String> {
        // Tauri puts `externalBin` binaries next to the app executable (target triple suffix
        // removed), in development builds as well.
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe_dir = exe.parent().ok_or("executable has no parent directory")?;
        let suffix = std::env::consts::EXE_SUFFIX;
        let resource_dir = app.path().resource_dir().map_err(|e| e.to_string())?;
        let app_data = app.path().app_data_dir().map_err(|e| e.to_string())?;
        fs::create_dir_all(&app_data).map_err(|e| format!("{}: {e}", app_data.display()))?;
        let paths = Paths {
            xkrd: exe_dir.join(format!("xkrd{suffix}")),
            lib_dir: resource_dir.join("lib"),
            bridge_dir: resource_dir.join("bridge"),
            settings_file: app_data.join("settings.json"),
            log_file: app_data.join("xkrd.log"),
            app_data,
        };
        let settings = fs::read_to_string(&paths.settings_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Ok(Self {
            rpc_port: free_loopback_port()?,
            cores,
            exiting: AtomicBool::new(false),
            op: Mutex::new(()),
            child: Mutex::new(None),
            phase: Mutex::new(Phase {
                name: "stopped",
                detail: String::new(),
            }),
            settings: Mutex::new(settings),
            paths,
        })
    }

    pub fn rpc_port(&self) -> u16 {
        self.rpc_port
    }

    /// Half of the cores, at least 1: the default for mining and verification threads.
    fn half_cores(&self) -> usize {
        (self.cores / 2).max(1)
    }

    fn hyper_available(&self) -> bool {
        self.paths.bridge_dir.join("src").join("worker.js").is_file()
    }

    fn data_dir(&self, s: &Settings) -> PathBuf {
        self.paths
            .app_data
            .join(if s.testnet { "xkrd-testnet" } else { "xkrd" })
    }

    fn set_phase(&self, name: &'static str, detail: impl Into<String>) {
        *lock(&self.phase) = Phase {
            name,
            detail: detail.into(),
        };
    }

    /// State for the UI (never waits for a running start/stop).
    pub fn info(&self) -> Value {
        self.poll_exit();
        let phase = lock(&self.phase).clone();
        let settings = lock(&self.settings).clone();
        json!({
            "phase": phase.name,
            "detail": phase.detail,
            "rpc_port": self.rpc_port,
            "cores": self.cores,
            "default_threads": self.half_cores(),
            "settings": settings,
            "data_dir": self.data_dir(&settings),
            "log_file": self.paths.log_file,
            "hyper_available": self.hyper_available(),
        })
    }

    /// Notices an xkrd that exited on its own (crash, bad flag, database locked by another
    /// xkrd). Skipped while a start/stop holds the child.
    fn poll_exit(&self) {
        let Ok(mut guard) = self.child.try_lock() else {
            return;
        };
        let Some(child) = guard.as_mut() else { return };
        if let Ok(Some(status)) = child.try_wait() {
            *guard = None;
            drop(guard);
            self.set_phase("exited", format!("xkrd {status}\n{}", self.log_tail(6)));
        }
    }

    /// Starts xkrd and waits for its RPC.
    pub fn start(&self) -> Result<(), String> {
        let _op = lock(&self.op);
        self.start_locked()
    }

    /// Stops xkrd for good (app quit). Idempotent.
    pub fn shutdown(&self) {
        self.exiting.store(true, Ordering::SeqCst);
        let _op = lock(&self.op);
        self.stop_locked();
    }

    pub fn is_exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    /// Changes the miner settings. xkrd has no RPC to start or stop its built-in miner at
    /// runtime (`--mine` is a start flag), so this restarts xkrd with the new flags. If xkrd
    /// does not come up with them (e.g. it refuses the address), the previous settings are
    /// restored and xkrd is started again with those.
    pub fn set_mining(&self, enabled: bool, address: String, threads: usize) -> Result<(), String> {
        let address = address.trim().to_owned();
        if enabled {
            check_address(&address)?;
        }
        let threads = if threads == 0 {
            self.half_cores()
        } else {
            threads.min(self.cores)
        };

        let _op = lock(&self.op);
        if self.is_exiting() {
            return Err("the app is quitting".into());
        }
        let previous = lock(&self.settings).clone();
        let mut next = previous.clone();
        next.mining = Mining {
            enabled,
            address,
            threads,
        };
        let running = lock(&self.phase).name == "running";
        if next == previous && running {
            return Ok(());
        }
        *lock(&self.settings) = next.clone();
        self.stop_locked();
        match self.start_locked() {
            Ok(()) => {
                if let Err(e) = save_settings(&self.paths.settings_file, &next) {
                    eprintln!("cannot save settings: {e}");
                }
                Ok(())
            }
            Err(e) => {
                *lock(&self.settings) = previous;
                self.stop_locked();
                let _ = self.start_locked();
                Err(format!(
                    "xkrd did not start with these mining settings, previous settings restored: {e}"
                ))
            }
        }
    }

    fn start_locked(&self) -> Result<(), String> {
        if self.is_exiting() {
            return Err("the app is quitting".into());
        }
        if lock(&self.child).is_some() {
            return Ok(());
        }
        let settings = lock(&self.settings).clone();
        self.set_phase("starting", "");
        let child = match self.spawn(&settings) {
            Ok(c) => c,
            Err(e) => {
                self.set_phase("failed", e.clone());
                return Err(e);
            }
        };
        *lock(&self.child) = Some(child);
        match self.wait_for_rpc() {
            Ok(()) => {
                self.set_phase("running", "");
                Ok(())
            }
            Err(e) => {
                self.set_phase("failed", e.clone());
                Err(e)
            }
        }
    }

    fn spawn(&self, s: &Settings) -> Result<Child, String> {
        let data_dir = self.data_dir(s);
        fs::create_dir_all(&data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
        // Keep the log of the previous run (useful after a crash or a refused flag).
        let log_path = &self.paths.log_file;
        if log_path.exists() {
            let _ = fs::rename(log_path, log_path.with_extension("prev.log"));
        }
        let log = File::create(log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;

        let mut cmd = Command::new(&self.paths.xkrd);
        cmd.args(self.xkrd_args(s, &data_dir))
            .env(LIBRARY_PATH_VAR, self.library_path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window. xkrd still gets a (hidden) console, which `interrupt` attaches
            // to for Ctrl-C.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.paths.xkrd.display()))
    }

    /// `lib/` in front of the inherited search path.
    fn library_path(&self) -> OsString {
        let mut dirs = vec![self.paths.lib_dir.clone()];
        if let Some(old) = std::env::var_os(LIBRARY_PATH_VAR) {
            dirs.extend(std::env::split_paths(&old));
        }
        std::env::join_paths(dirs).unwrap_or_else(|_| self.paths.lib_dir.clone().into_os_string())
    }

    fn xkrd_args(&self, s: &Settings, data_dir: &Path) -> Vec<OsString> {
        fn os(x: impl AsRef<OsStr>) -> OsString {
            x.as_ref().to_owned()
        }
        let half = self.half_cores();
        let mut a: Vec<OsString> = Vec::new();
        if s.testnet {
            a.extend([os("--network"), os("testnet")]);
        }
        a.extend([
            os("--data-dir"),
            os(data_dir),
            os("--rpc-bind-ip"),
            os("127.0.0.1"),
            os("--rpc-bind-port"),
            os(self.rpc_port.to_string()),
            // No legacy P2P listener: a desktop node dials out, and Hyperswarm holepunches.
            os("--p2p-bind-port"),
            os("0"),
            os("--log-json"),
            os("--verify-threads"),
            os(if s.verify_threads == 0 { half } else { s.verify_threads }.to_string()),
        ]);
        if s.fast_sync {
            a.push(os("--fast-sync"));
        }
        if s.hyper && self.hyper_available() {
            a.extend([
                os("--hyper-bridge-dir"),
                os(&self.paths.bridge_dir),
            ]);
        } else {
            a.push(os("--no-hyper"));
        }
        if s.mining.enabled {
            let threads = if s.mining.threads == 0 { half } else { s.mining.threads };
            a.extend([
                os("--mine"),
                os(&s.mining.address),
                os("--mine-threads"),
                os(threads.to_string()),
            ]);
        }
        a.extend(s.extra_args.iter().map(os));
        a
    }

    fn wait_for_rpc(&self) -> Result<(), String> {
        let deadline = Instant::now() + RPC_START_TIMEOUT;
        loop {
            {
                let mut guard = lock(&self.child);
                match guard.as_mut().map(Child::try_wait) {
                    None => return Err("xkrd is gone".into()),
                    Some(Ok(Some(status))) => {
                        *guard = None;
                        return Err(format!("xkrd {status}\n{}", self.log_tail(6)));
                    }
                    Some(Ok(None)) => {}
                    Some(Err(e)) => return Err(e.to_string()),
                }
            }
            if self.is_exiting() {
                // Quit while starting: let shutdown() take the op lock and stop xkrd now.
                return Err("the app is quitting".into());
            }
            if rpc::get_json(self.rpc_port, "/status", Duration::from_secs(2)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "xkrd's RPC on 127.0.0.1:{} did not answer within {} s",
                    self.rpc_port,
                    RPC_START_TIMEOUT.as_secs()
                ));
            }
            sleep(Duration::from_millis(250));
        }
    }

    fn stop_locked(&self) {
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        if let Ok(Some(status)) = child.try_wait() {
            self.set_phase("stopped", format!("xkrd {status}"));
            return;
        }
        self.set_phase("stopping", "");
        let deadline = Instant::now()
            + if interrupt(&child) {
                STOP_TIMEOUT
            } else {
                Duration::ZERO
            };
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.set_phase("stopped", format!("xkrd {status}"));
                    return;
                }
                Ok(None) if Instant::now() < deadline => sleep(Duration::from_millis(100)),
                _ => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        self.set_phase(
            "stopped",
            "xkrd did not exit after the interrupt and was killed",
        );
    }

    /// The last `n` lines of xkrd's log.
    fn log_tail(&self, n: usize) -> String {
        let text = fs::read_to_string(&self.paths.log_file).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(n)..]
            .iter()
            .map(|l| l.chars().take(400).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Picks a free loopback port for the RPC, so the app never clashes with another xkrd or a
/// C++ node on 11898.
fn free_loopback_port() -> Result<u16, String> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| format!("no free loopback port: {e}"))
}

/// A cheap sanity check before restarting xkrd; xkrd itself decides whether the address is
/// valid for the network.
fn check_address(address: &str) -> Result<(), String> {
    const BASE58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    if address.is_empty() {
        return Err("a payout address is needed to mine".into());
    }
    if !(90..=200).contains(&address.len()) || !address.chars().all(|c| BASE58.contains(c)) {
        return Err("that does not look like an XKR address (SEKR…)".into());
    }
    Ok(())
}

fn save_settings(path: &Path, s: &Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Asks xkrd to shut down cleanly (it handles SIGINT/SIGTERM on Unix, Ctrl-C on Windows).
#[cfg(unix)]
fn interrupt(child: &Child) -> bool {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        return false;
    };
    // SAFETY: kill(2) on our own child. It has not been reaped (we still own the `Child` and
    // have not seen it exit), so the pid cannot have been reused by another process.
    unsafe { libc::kill(pid, libc::SIGINT) == 0 }
}

/// Windows has no SIGINT for another process. Attach to xkrd's (hidden) console and send
/// Ctrl-C to every process on it (xkrd, which shuts down on it).
/// This GUI process ignores Ctrl-C from then on so the event cannot end it.
#[cfg(windows)]
fn interrupt(child: &Child) -> bool {
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    // SAFETY: plain Win32 console calls without pointers; the console is detached again
    // before returning.
    unsafe {
        FreeConsole();
        if AttachConsole(child.id()) == 0 {
            return false;
        }
        SetConsoleCtrlHandler(None, 1);
        let sent = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
        FreeConsole();
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, check_address};

    #[test]
    fn settings_defaults_fill_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"mining":{"enabled":true}}"#).unwrap();
        assert!(s.mining.enabled);
        assert_eq!(s.mining.threads, 0);
        assert!(s.fast_sync && s.hyper);
    }

    #[test]
    fn address_check() {
        assert!(check_address("").is_err());
        assert!(check_address("SEKR0OIl").is_err());
        assert!(check_address(&format!("SEKR{}", "1".repeat(95))).is_ok());
    }
}
