//! The bridge's JavaScript side (`docs/hyper-transport.md`): N Bare worker threads inside this
//! process, started through bare-kit's C API (module `ffi`).
//!
//! Every worker runs `bridge/src/worker.js`, which drives the frame protocol (`Session` in
//! `bridge/src/session.js`) with its own HyperDHT node (UDP socket) and DHT key pair, over
//! bare-kit's IPC channel (`Bare.IPC` in the worklet). A pump thread per worker copies bytes
//! between that IPC channel and an in-memory channel ([`crate::transport::mem_channel`]) whose
//! other end is an ordinary bridge link with its frames, credits and keepalive.
//!
//! With one worker (what xkrd runs) every topic group shares its DHT node and so one UDP port:
//! the plain swarm (XKR topics, node key) and one swarm per Hugin identity all sit on that node.
//! With two or more, worker 0 serves the topics without Hugin `mid` and the last worker the Hugin
//! topics; [`crate::Bridge::assign_topic`] puts further groups on the workers in between, and
//! worker `i` binds the fixed UDP port + `i`. A connection stays on the worker that made or
//! accepted it.
//!
//! Before the first worker starts, [`check_runtime`] loads the shared libraries the workers'
//! native addons need (module `runtime`).

mod ffi;
mod runtime;

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::BytesMut;
use rand_core::RngCore as _;
use tokio::sync::{oneshot, watch};
use tokio_util::codec::Decoder as _;

use self::ffi::{IPC_READ_BUFFER_SIZE, IpcRead, IpcWrite, Worklet};
pub use self::runtime::{
    CXX_RUNTIME, CXX_RUNTIME_MISSING, NIXOS_SYSTEM_LIB, RuntimeSource, check_runtime, preload_with,
    runtime_candidates,
};
use crate::bridge::{Bridge, BridgeConfig, BridgeError, MAX_WORKERS};
use crate::frame::{Body, FrameCodec};
use crate::transport::{DEFAULT_CHANNEL_CAPACITY, MemPeer, PeerWaker, mem_channel};

/// Script each worker runs, relative to the bridge directory.
pub const WORKER_SCRIPT: &str = "src/worker.js";
/// First of the worker's own arguments in `Bare.argv` (bare-kit may put entries before them).
pub const ARGS_MARKER: &str = "--xkr-worker";

/// Shortest and longest sleep of an idle pump when it polls on a timer.
const IDLE_WAIT_MIN: Duration = Duration::from_millis(1);
const IDLE_WAIT_MAX: Duration = Duration::from_millis(8);
/// Sleep of an idle pump while bare-kit's poll callback is armed (it wakes the pump early).
const POLLED_WAIT: Duration = Duration::from_millis(20);
/// Bytes moved from the bridge link to a worker per step.
const PUMP_CHUNK: usize = 64 * 1024;
/// How long a worker gets to leave the DHT and exit after its IPC was ended.
const END_GRACE: Duration = Duration::from_secs(2);
/// How long a worker gets to tear down after it was terminated. With [`END_GRACE`] this stays
/// below the 5 s xkrd gives `BareWorkers::stop`.
const TERMINATE_WAIT: Duration = Duration::from_secs(2);

/// Settings of the Bare workers (the rest comes from [`BridgeConfig`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareConfig {
    /// Number of Bare workers, 1 to [`MAX_WORKERS`].
    pub workers: u16,
    /// JS heap limit per worker in bytes (bare-kit's `memory_limit`); 0 = Bare's default.
    pub memory_limit: usize,
    /// Bytes buffered per direction between a worker and its link.
    pub channel_capacity: usize,
    /// Let bare-kit's IPC poll callback wake the pump; false: poll on a 1-8 ms timer only.
    pub ipc_poll: bool,
}

impl Default for BareConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            memory_limit: 0,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            ipc_poll: true,
        }
    }
}

/// Tears down bare-kit's process-wide state (`bare_worklet_teardown`) at process end, after
/// every [`BareWorkers`] was stopped or dropped. Does nothing if no worker was ever started, and
/// runs at most once; afterwards [`Bridge::start_in_process`] fails. Waits at most `wait` for
/// worklets that are still tearing down and returns false if that was not enough (the process
/// may exit anyway).
#[must_use]
pub fn teardown(wait: Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("xkr-bare-teardown".into())
        .spawn(move || {
            ffi::teardown();
            let _ = tx.send(());
        });
    spawned.is_ok() && rx.recv_timeout(wait).is_ok()
}

/// Checks that every package in the `dependencies` of the bridge's `package.json` is installed in
/// its `node_modules`. Bare resolves the worker's imports before any of its code runs, so a
/// missing package would end the worker before it can say why; this names the package instead.
fn check_packages(dir: &Path) -> Result<(), String> {
    let manifest = dir.join("package.json");
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("cannot read {}: {e}", manifest.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("cannot parse {}: {e}", manifest.display()))?;
    let Some(deps) = json
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(());
    };
    let modules = dir.join("node_modules");
    let missing: Vec<&str> = deps
        .keys()
        .filter(|name| !modules.join(name.as_str()).join("package.json").is_file())
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the bridge in {} lacks its npm packages ({} not in node_modules): a release archive \
         ships them in bridge/node_modules, a source checkout needs `npm ci` in that directory",
        dir.display(),
        missing.join(", ")
    ))
}

/// Key pair file of worker `worker`: `base` itself for worker 0 (the node key),
/// `<stem>.<worker>.json` next to it for the others.
#[must_use]
pub fn worker_keypair_file(base: &Path, worker: u16) -> PathBuf {
    if worker == 0 {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .map_or_else(|| "hyper-keypair".into(), |s| s.to_string_lossy());
    base.with_file_name(format!("{stem}.{worker}.json"))
}

/// The 32-byte `seed` of a key pair file: JSON with `seed` (and, in files of earlier versions,
/// `publicKey`) in hex; only `seed` is read. A missing file is created with a fresh random seed
/// and only that field (mode 0600 on Unix).
pub fn load_or_create_seed(path: &Path) -> Result<[u8; 32], BridgeError> {
    let bad = |what: &str| BridgeError::Spawn(format!("key pair file {}: {what}", path.display()));
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
            let hex_seed = v
                .get("seed")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| bad("no \"seed\" string"))?;
            let bytes = hex::decode(hex_seed).map_err(|e| bad(&e.to_string()))?;
            <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| bad("seed must be 32 bytes"))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut seed = [0u8; 32];
            xkr_crypto::os_rng().fill_bytes(&mut seed);
            let body = format!("{}\n", serde_json::json!({ "seed": hex::encode(seed) }));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut f) => {
                    f.write_all(body.as_bytes())
                        .map_err(|e| bad(&e.to_string()))?;
                    tracing::info!(file = %path.display(), "created key pair file");
                    Ok(seed)
                }
                // Created by someone else in the meantime: use that one.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => load_or_create_seed(path),
                Err(e) => Err(bad(&e.to_string())),
            }
        }
        Err(e) => Err(bad(&e.to_string())),
    }
}

/// `Bare.argv` tail of worker `worker` of `workers` (parsed by `parseWorkerArgs` in
/// `bridge/src/worker.js`). The seed travels in memory only: a worker's arguments are not a
/// process command line.
#[must_use]
pub fn worker_args(
    config: &BridgeConfig,
    worker: u16,
    workers: u16,
    seed: &[u8; 32],
) -> Vec<String> {
    let mut a = vec![
        ARGS_MARKER.to_owned(),
        "--worker".into(),
        worker.to_string(),
        "--workers".into(),
        workers.to_string(),
        "--seed".into(),
        hex::encode(seed),
        "--max-frame".into(),
        config.max_frame.to_string(),
        "--initial-credit".into(),
        config.initial_credit.to_string(),
    ];
    for b in &config.bootstrap {
        a.push("--bootstrap".into());
        a.push(b.clone());
    }
    // Worker `i` binds the fixed port + `i` (one worker: the port itself, for every topic);
    // `start_in_process` checks that the last one fits.
    if let Some(p) = config.udp_port.and_then(|p| p.checked_add(worker)) {
        a.push("--port-fixed".into());
        a.push(p.to_string());
    }
    // The peer cap belongs to the plain swarm, which lives on worker 0.
    if worker == 0
        && let Some(n) = config.max_peers
    {
        a.push("--max-peers".into());
        a.push(n.to_string());
    }
    if config.log_json {
        a.push("--log-json".into());
    }
    a
}

#[derive(Debug)]
struct WorkerThread {
    stop: Arc<AtomicBool>,
    waker: PeerWaker,
    handle: Option<JoinHandle<()>>,
}

/// The Bare worker threads of a bridge. Dropping it asks every worker to stop
/// (without waiting); [`BareWorkers::stop`] also waits.
#[derive(Debug)]
pub struct BareWorkers {
    threads: Vec<WorkerThread>,
    /// Why the first worker ended.
    ended: watch::Receiver<Option<String>>,
}

impl BareWorkers {
    /// Number of workers.
    #[must_use]
    pub fn count(&self) -> usize {
        self.threads.len()
    }

    /// Resolves when a worker ended (its worklet exited or closed its IPC, or the bridge link
    /// closed), with a line for the log.
    pub async fn wait(&self) -> String {
        let mut rx = self.ended.clone();
        loop {
            if let Some(why) = rx.borrow_and_update().clone() {
                return why;
            }
            if rx.changed().await.is_err() {
                return "every Bare worker thread ended".into();
            }
        }
    }

    /// Asks every worker to end its worklet and its thread; does not wait.
    pub fn start_kill(&self) {
        for t in &self.threads {
            t.stop.store(true, Ordering::Release);
            t.waker.wake();
        }
    }

    /// [`BareWorkers::start_kill`], then waits up to `grace` for the threads to end. Each thread
    /// ends its worklet's IPC, gives it 2 s to leave the DHT and exit, then terminates it and
    /// waits 2 s more. A thread that does not end in time is left running (threads cannot be
    /// killed).
    pub async fn stop(&mut self, grace: Duration) {
        self.start_kill();
        let handles: Vec<JoinHandle<()>> = self
            .threads
            .iter_mut()
            .filter_map(|t| t.handle.take())
            .collect();
        if handles.is_empty() {
            return;
        }
        let joined = tokio::task::spawn_blocking(move || {
            for h in handles {
                let _ = h.join();
            }
        });
        if tokio::time::timeout(grace, joined).await.is_err() {
            tracing::warn!("Bare workers did not stop in time; leaving their threads");
        }
    }
}

impl Drop for BareWorkers {
    fn drop(&mut self) {
        self.start_kill();
    }
}

impl Bridge {
    /// Starts `bare.workers` Bare workers in this process, each running
    /// `<bridge dir>/src/worker.js` on its own thread, and connects to them (one link per
    /// worker, `Hello` with `workers`/`worker`). Worker `i` uses the key pair file
    /// [`worker_keypair_file`]`(keypair_file, i)` (created if missing; a fresh key per start
    /// without `keypair_file`). Worker `i` binds the fixed UDP port + `i`; `max_peers` goes to
    /// worker 0 only.
    pub async fn start_in_process(
        config: BridgeConfig,
        bare: &BareConfig,
    ) -> Result<(Bridge, BareWorkers), BridgeError> {
        let count = bare.workers.clamp(1, MAX_WORKERS);
        if let Some(p) = config.udp_port
            && (p == 0 || p.checked_add(count - 1).is_none())
        {
            return Err(BridgeError::Spawn(format!(
                "UDP ports {p} to {} for {count} workers are not all valid ports",
                u32::from(p) + u32::from(count) - 1
            )));
        }
        let dir = config.resolve_bridge_dir()?;
        let script = dir.join(WORKER_SCRIPT);
        if !script.is_file() {
            return Err(BridgeError::Spawn(format!(
                "no {WORKER_SCRIPT} in {}",
                dir.display()
            )));
        }
        check_packages(&dir).map_err(BridgeError::Spawn)?;
        check_runtime().map_err(BridgeError::Spawn)?;
        let mut seeds = Vec::with_capacity(usize::from(count));
        for i in 0..count {
            seeds.push(if let Some(key_base) = &config.keypair_file {
                load_or_create_seed(&worker_keypair_file(key_base, i))?
            } else {
                let mut seed = [0u8; 32];
                xkr_crypto::os_rng().fill_bytes(&mut seed);
                seed
            });
        }

        let (ended_tx, ended_rx) = watch::channel(None);
        let ended_tx = Arc::new(ended_tx);
        let startup_error = Arc::new(OnceLock::new());
        // From here on an early return drops `workers`, which stops every started thread.
        let mut workers = BareWorkers {
            threads: Vec::with_capacity(usize::from(count)),
            ended: ended_rx,
        };
        let mut streams = Vec::with_capacity(usize::from(count));
        let mut started = Vec::with_capacity(usize::from(count));
        for (i, seed) in (0..count).zip(&seeds) {
            let (stream, peer) = mem_channel(bare.channel_capacity);
            let stop = Arc::new(AtomicBool::new(false));
            let waker = peer.waker();
            let (tx, rx) = oneshot::channel();
            let start = WorkerStart {
                index: i,
                script: script.clone(),
                args: worker_args(&config, i, count, seed),
                memory_limit: bare.memory_limit,
                ipc_poll: bare.ipc_poll,
                peer,
                stop: stop.clone(),
                started: tx,
                ended: ended_tx.clone(),
                startup_error: startup_error.clone(),
            };
            let handle = std::thread::Builder::new()
                .name(format!("xkr-bare-{i}"))
                .spawn(move || run_worker(start))
                .map_err(|e| {
                    BridgeError::Spawn(format!("cannot start Bare worker thread {i}: {e}"))
                })?;
            workers.threads.push(WorkerThread {
                stop,
                waker,
                handle: Some(handle),
            });
            streams.push(stream);
            started.push(rx);
        }
        drop(ended_tx);

        for (i, rx) in started.into_iter().enumerate() {
            match tokio::time::timeout(config.startup_timeout, rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    return Err(BridgeError::Spawn(format!("Bare worker {i}: {e}")));
                }
                Ok(Err(_)) => {
                    return Err(BridgeError::Spawn(format!(
                        "Bare worker {i} ended while starting"
                    )));
                }
                Err(_) => {
                    return Err(BridgeError::Spawn(format!(
                        "Bare worker {i} did not start in time"
                    )));
                }
            }
        }
        match Bridge::connect_streams(streams, config).await {
            Ok(bridge) => Ok((bridge, workers)),
            // A worker that could not load its modules says why in an `Error` frame before its
            // `Hello` (`bridge/src/worker-guard.js`); that reason is the one to show.
            Err(e) => Err(startup_error
                .get()
                .map_or(e, |why| BridgeError::Spawn(why.clone()))),
        }
    }
}

/// Everything a worker thread needs.
struct WorkerStart {
    index: u16,
    script: PathBuf,
    args: Vec<String>,
    memory_limit: usize,
    ipc_poll: bool,
    peer: MemPeer,
    stop: Arc<AtomicBool>,
    started: oneshot::Sender<Result<(), String>>,
    ended: Arc<watch::Sender<Option<String>>>,
    /// The first reason a worker gave for not starting ([`HelloWatch`]).
    startup_error: Arc<OnceLock<String>>,
}

/// Records why the first worker ended.
fn report_end(ended: &watch::Sender<Option<String>>, why: String) {
    ended.send_if_modified(|e| {
        if e.is_none() {
            *e = Some(why);
            true
        } else {
            false
        }
    });
}

/// Body of a worker thread: start the worklet, pump bytes until either side ends, end the
/// worklet.
fn run_worker(s: WorkerStart) {
    let WorkerStart {
        index,
        script,
        args,
        memory_limit,
        ipc_poll,
        peer,
        stop,
        started,
        ended,
        startup_error,
    } = s;
    let mut worklet = match Worklet::start(&script, &args, memory_limit, peer.waker()) {
        Ok(w) => w,
        Err(e) => {
            report_end(&ended, format!("Bare worker {index} did not start: {e}"));
            let _ = started.send(Err(e));
            return;
        }
    };
    let _ = started.send(Ok(()));
    tracing::info!(worker = index, "Bare worker started");
    let mut hello = HelloWatch::default();
    let on_startup_error = |why: String| {
        tracing::debug!(worker = index, reason = %why, "Bare worker could not start");
        let _ = startup_error.set(format!("Bare worker {index}: {why}"));
    };
    let reason = pump(
        &mut worklet,
        &peer,
        &stop,
        ipc_poll,
        &mut hello,
        &on_startup_error,
    );
    // Close the link first, so the bridge fails at once, then end the worklet.
    peer.close();
    let exit = worklet
        .shutdown(END_GRACE, TERMINATE_WAIT)
        .or_else(|| worklet.exit_code());
    drop(worklet);
    let exit = exit.map_or_else(|| "not reported".to_owned(), |c| c.to_string());
    tracing::info!(worker = index, reason = %reason, exit_code = %exit, "Bare worker stopped");
    report_end(
        &ended,
        format!("Bare worker {index} stopped: {reason} (exit code {exit})"),
    );
}

/// Copies bytes both ways between the worklet's IPC and the link's in-memory channel until
/// one side ends or `stop` is set; returns why it ended. The worker's first frames also go
/// through `hello`, which hands an early `Error` frame's reason to `on_startup_error`.
fn pump(
    w: &mut Worklet,
    peer: &MemPeer,
    stop: &AtomicBool,
    mut use_poll: bool,
    hello: &mut HelloWatch,
    on_startup_error: &dyn Fn(String),
) -> String {
    // Worker to Rust: bytes read from the IPC that the channel has not taken yet.
    let mut inbound = vec![0u8; IPC_READ_BUFFER_SIZE];
    let (mut in_off, mut in_len) = (0usize, 0usize);
    // Rust to worker: bytes taken from the channel that the IPC has not taken yet.
    let mut outbound = vec![0u8; PUMP_CHUNK];
    let (mut out_off, mut out_len) = (0usize, 0usize);
    let mut idle = IDLE_WAIT_MIN;
    loop {
        if stop.load(Ordering::Acquire) {
            return "stopped".into();
        }
        let seen = peer.generation();
        let mut progress = false;

        loop {
            if in_off < in_len {
                match peer.write(&inbound[in_off..in_len]) {
                    None => return "bridge link closed".into(),
                    Some(0) => break,
                    Some(n) => {
                        in_off += n;
                        progress = true;
                        continue;
                    }
                }
            }
            match w.read(&mut inbound) {
                IpcRead::WouldBlock => break,
                IpcRead::Data(n) => {
                    // Looked at before the bridge link gets the bytes, so the reason is known
                    // when the link fails.
                    if let Some(why) = hello.feed(&inbound[..n]) {
                        on_startup_error(why);
                    }
                    (in_off, in_len) = (0, n);
                    progress = true;
                }
                IpcRead::End => return "the worklet ended its IPC".into(),
                IpcRead::Failed(e) => return format!("IPC read failed ({e})"),
            }
        }

        loop {
            if out_off == out_len {
                match peer.read(&mut outbound) {
                    None => return "bridge closed".into(),
                    Some(0) => break,
                    Some(n) => {
                        out_off = 0;
                        out_len = n;
                        progress = true;
                    }
                }
            }
            match w.write(&outbound[out_off..out_len]) {
                IpcWrite::Wrote(0) => break,
                IpcWrite::Wrote(n) => {
                    out_off += n;
                    progress = true;
                }
                IpcWrite::Closed => return "the worklet closed its IPC".into(),
                IpcWrite::Failed(e) => return format!("IPC write failed ({e})"),
            }
        }

        if progress {
            idle = IDLE_WAIT_MIN;
            continue;
        }
        let mut wait = idle;
        if use_poll {
            // Read only when the channel can take more; write only when bytes are waiting.
            if w.watch(in_off >= in_len, out_off < out_len) {
                wait = POLLED_WAIT;
            } else {
                tracing::warn!("bare-kit refused the IPC poll; polling on a timer");
                use_poll = false;
            }
        }
        peer.wait(seen, wait);
        idle = (idle * 2).min(IDLE_WAIT_MAX);
    }
}

/// Longest startup reason kept from a worker's `Error` frame, in characters.
const STARTUP_REASON_MAX: usize = 300;

/// Looks at what a worker sends until its `Hello`. A worker that cannot load its modules (a
/// native addon that does not load, for example) sends one `Error` frame instead and exits
/// (`bridge/src/worker-guard.js`); [`HelloWatch::feed`] returns that frame's message. After the
/// `Hello` (or anything it cannot decode) it only skips the bytes.
#[derive(Debug, Default)]
struct HelloWatch {
    buf: BytesMut,
    done: bool,
}

impl HelloWatch {
    /// Takes the next bytes from the worker; returns the reason of an `Error` frame that came
    /// before the `Hello` (its first line, at most [`STARTUP_REASON_MAX`] characters).
    fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        if self.done {
            return None;
        }
        self.buf.extend_from_slice(bytes);
        let mut codec = FrameCodec::default();
        let found = loop {
            match codec.decode(&mut self.buf) {
                Ok(None) => return None,
                Ok(Some(f)) => match f.body {
                    Body::Error(e) => break Some(e.message),
                    Body::Hello(_) => break None,
                    _ => {}
                },
                Err(_) => break None,
            }
        };
        self.done = true;
        self.buf = BytesMut::new();
        found.map(|m| startup_reason(&m))
    }
}

/// The first non-empty line of `message`, without control characters, trimmed and cut to
/// [`STARTUP_REASON_MAX`] characters; "no reason given" for an empty message.
fn startup_reason(message: &str) -> String {
    let line = message
        .lines()
        .map(|l| crate::bridge::printable(l).trim().to_owned())
        .find(|l| !l.is_empty());
    line.as_deref()
        .unwrap_or("no reason given")
        .chars()
        .take(STARTUP_REASON_MAX)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ErrorFrame, Frame, Hello};

    #[test]
    fn missing_packages_are_named() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"b4a":"1.9.0","hyperswarm":"4.17.2"}}"#,
        )
        .unwrap();
        let err = check_packages(dir.path()).unwrap_err();
        assert!(err.contains("b4a, hyperswarm not in node_modules"), "{err}");
        for name in ["b4a", "hyperswarm"] {
            let pkg = dir.path().join("node_modules").join(name);
            std::fs::create_dir_all(&pkg).unwrap();
            std::fs::write(pkg.join("package.json"), "{}").unwrap();
        }
        check_packages(dir.path()).unwrap();
    }

    #[test]
    fn keypair_files_per_worker() {
        let base = Path::new("/data/hyper-keypair.json");
        assert_eq!(worker_keypair_file(base, 0), base);
        assert_eq!(
            worker_keypair_file(base, 1),
            Path::new("/data/hyper-keypair.1.json")
        );
    }

    #[test]
    fn seeds_are_created_once_and_read_back() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("k.json");
        let seed = load_or_create_seed(&file).expect("create");
        assert_eq!(load_or_create_seed(&file).expect("read"), seed);
        // A file with the public key next to the seed.
        let other = tmp.path().join("with-public-key.json");
        std::fs::write(
            &other,
            format!(
                "{{\"seed\": \"{}\", \"publicKey\": \"{}\"}}\n",
                "11".repeat(32),
                "22".repeat(32)
            ),
        )
        .expect("write");
        assert_eq!(load_or_create_seed(&other).expect("read"), [0x11; 32]);
        std::fs::write(&other, "{\"seed\": \"1234\"}").expect("write");
        assert!(load_or_create_seed(&other).is_err());
    }

    #[test]
    fn worker_arguments() {
        let config = BridgeConfig {
            bootstrap: vec!["127.0.0.1:49737".into()],
            udp_port: Some(21890),
            max_peers: Some(24),
            ..BridgeConfig::default()
        };
        let a = worker_args(&config, 0, 2, &[7; 32]);
        assert_eq!(a[0], ARGS_MARKER);
        let pair = |args: &[String], flag: &str| {
            args.iter()
                .position(|x| x == flag)
                .map(|i| args[i + 1].clone())
        };
        assert_eq!(pair(&a, "--worker").as_deref(), Some("0"));
        assert_eq!(pair(&a, "--workers").as_deref(), Some("2"));
        assert_eq!(pair(&a, "--seed"), Some("07".repeat(32)));
        assert_eq!(pair(&a, "--port-fixed").as_deref(), Some("21890"));
        assert_eq!(pair(&a, "--max-peers").as_deref(), Some("24"));
        assert_eq!(pair(&a, "--bootstrap").as_deref(), Some("127.0.0.1:49737"));
        let b = worker_args(&config, 1, 2, &[7; 32]);
        assert_eq!(pair(&b, "--worker").as_deref(), Some("1"));
        assert_eq!(
            pair(&b, "--port-fixed").as_deref(),
            Some("21891"),
            "worker i binds the fixed port + i"
        );
        assert_eq!(pair(&b, "--max-peers"), None);
        // One worker (xkrd): the port itself; no port: no argument.
        let one = worker_args(&config, 0, 1, &[7; 32]);
        assert_eq!(pair(&one, "--port-fixed").as_deref(), Some("21890"));
        let free = worker_args(&BridgeConfig::default(), 0, 1, &[7; 32]);
        assert_eq!(pair(&free, "--port-fixed"), None);
    }

    #[tokio::test]
    async fn fixed_ports_past_65535_are_refused_before_any_worker_starts() {
        let config = BridgeConfig {
            udp_port: Some(65_535),
            ..BridgeConfig::default()
        };
        let two = BareConfig {
            workers: 2,
            ..BareConfig::default()
        };
        let err = Bridge::start_in_process(config, &two)
            .await
            .expect_err("65535 + 1 is no port");
        assert!(matches!(err, BridgeError::Spawn(_)), "{err:?}");
        let zero = BridgeConfig {
            udp_port: Some(0),
            ..BridgeConfig::default()
        };
        let err = Bridge::start_in_process(zero, &BareConfig::default())
            .await
            .expect_err("port 0 is no fixed port");
        assert!(matches!(err, BridgeError::Spawn(_)), "{err:?}");
    }

    fn encoded(body: Body) -> Vec<u8> {
        Frame::control(body).to_bytes().expect("encode").to_vec()
    }

    fn error_frame(message: &str) -> Vec<u8> {
        encoded(Body::Error(ErrorFrame {
            request_id: 0,
            code: 5,
            message: message.into(),
        }))
    }

    fn hello_frame() -> Vec<u8> {
        encoded(Body::Hello(Hello {
            major: 1,
            minor: 0,
            max_frame: 65_536,
            initial_credit: 4096,
            workers: 1,
            worker: 0,
            features: 0,
            tlvs: Vec::new(),
        }))
    }

    /// A worker that cannot load its modules sends an `Error` frame instead of its `Hello`; the
    /// reason is its first line, also when the frame arrives in pieces.
    #[test]
    fn an_error_before_hello_is_the_startup_reason() {
        let bytes = error_frame(
            "AddonError: CANNOT_LOAD: libstdc++.so.6: cannot open shared object file\n    at x",
        );
        let mut w = HelloWatch::default();
        let (a, b) = bytes.split_at(7);
        assert_eq!(w.feed(a), None);
        assert_eq!(
            w.feed(b).as_deref(),
            Some("AddonError: CANNOT_LOAD: libstdc++.so.6: cannot open shared object file")
        );
        // Said once; later bytes are only passed on.
        assert_eq!(w.feed(&error_frame("again")), None);
    }

    #[test]
    fn frames_after_hello_are_not_looked_at() {
        let mut w = HelloWatch::default();
        let mut bytes = hello_frame();
        bytes.extend(error_frame("later"));
        assert_eq!(w.feed(&bytes), None);
        assert_eq!(w.feed(&error_frame("later still")), None);
        assert!(w.buf.is_empty());
    }

    #[test]
    fn startup_reasons_are_one_short_line() {
        assert_eq!(startup_reason("\n  first  \nsecond"), "first");
        assert_eq!(startup_reason("\u{1b}[2J\u{7}\nx"), "[2J");
        assert_eq!(startup_reason("\u{1b}\u{9b}\nnext"), "next");
        assert_eq!(startup_reason(""), "no reason given");
        assert_eq!(
            startup_reason(&"x".repeat(1000)).chars().count(),
            STARTUP_REASON_MAX
        );
    }
}
