//! The only unsafe code of the crate: bare-kit's worklet and IPC C API.
//!
//! Source of every declaration: `include/bare-kit.h` of bare-kit 3.1.0
//! (github.com/holepunchto/bare-kit, tag v3.1.0), whose `src/exports.json` lists exactly these
//! names as the exports of `libbare-kit`. The call order follows the usage example of its
//! README and its C tests (`test/worklet-ipc-host-end.c`, `test/worklet-exit-code.c`,
//! `test/worklet-teardown.c`): create the worklet, register the exit callback, create the IPC and
//! its poll, start; on the way out end the IPC, wait for the exit callback (terminate if it does
//! not come), then destroy the poll, the IPC and the worklet. `bare_worklet_destroy` never blocks;
//! the worklet thread tears itself down and calls the exit callback exactly once.
//!
//! A [`Worklet`] is created, used and dropped on one thread (its pump thread); it is not `Send`.
//! The poll callback runs on a thread owned by the IPC and only touches a [`PeerWaker`]; the exit
//! callback runs on the worklet thread and only touches an [`ExitState`].
#![allow(unsafe_code)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::marker::{PhantomData, PhantomPinned};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::transport::PeerWaker;

/// Opaque C handle types.
macro_rules! opaque {
    ($($name:ident),*) => {$(
        #[repr(C)]
        #[allow(dead_code, reason = "only used behind pointers, never built in Rust")]
        struct $name {
            _data: [u8; 0],
            _marker: PhantomData<(*mut u8, PhantomPinned)>,
        }
    )*};
}
opaque!(BareWorkletT, BareIpcT, BareIpcPollT);

/// `bare_worklet_options_t`, version 0.
#[repr(C)]
struct BareWorkletOptions {
    /// `int version`; 0.
    version: c_int,
    /// `size_t memory_limit`; 0 = inferred from the device's memory.
    memory_limit: usize,
    /// `union { struct { void *jvm; void *context; } android; void *reserved[8]; } platform`:
    /// only read on Android; all null here.
    platform: [*mut c_void; 8],
}

/// `BARE_IPC_READ_BUFFER_SIZE`: the read buffer size bare-kit's examples use.
pub(super) const IPC_READ_BUFFER_SIZE: usize = 64 * 1024;

/// `bare_ipc_readable`, `bare_ipc_writable`.
const BARE_IPC_READABLE: c_int = 0x1;
const BARE_IPC_WRITABLE: c_int = 0x2;
/// `bare_ipc_would_block`, `bare_ipc_error`.
const BARE_IPC_WOULD_BLOCK: c_int = -1;
const BARE_IPC_ERROR: c_int = -2;

/// `bare_ipc_poll_cb`.
type BareIpcPollCb = unsafe extern "C" fn(poll: *mut BareIpcPollT, events: c_int);
/// `bare_worklet_exit_cb`.
type BareWorkletExitCb = unsafe extern "C" fn(exit_code: c_int, data: *mut c_void);

unsafe extern "C" {
    fn bare_worklet_teardown() -> c_int;
    fn bare_worklet_create(
        options: *const BareWorkletOptions,
        result: *mut *mut BareWorkletT,
    ) -> c_int;
    fn bare_worklet_destroy(worklet: *mut BareWorkletT);
    fn bare_worklet_on_exit(
        worklet: *mut BareWorkletT,
        cb: BareWorkletExitCb,
        data: *mut c_void,
    ) -> c_int;
    fn bare_worklet_start(
        worklet: *mut BareWorkletT,
        filename: *const c_char,
        source: *const c_void,
        len: usize,
        argc: c_int,
        argv: *const *const c_char,
    ) -> c_int;
    fn bare_worklet_terminate(worklet: *mut BareWorkletT) -> c_int;

    fn bare_ipc_create(worklet: *mut BareWorkletT, result: *mut *mut BareIpcT) -> c_int;
    fn bare_ipc_end(ipc: *mut BareIpcT);
    fn bare_ipc_destroy(ipc: *mut BareIpcT);
    fn bare_ipc_read(ipc: *mut BareIpcT, data: *mut c_void, len: usize) -> c_int;
    fn bare_ipc_write(ipc: *mut BareIpcT, data: *const c_void, len: usize) -> c_int;

    fn bare_ipc_poll_create(ipc: *mut BareIpcT, result: *mut *mut BareIpcPollT) -> c_int;
    fn bare_ipc_poll_destroy(poll: *mut BareIpcPollT);
    fn bare_ipc_poll_get_data(poll: *mut BareIpcPollT) -> *mut c_void;
    fn bare_ipc_poll_set_data(poll: *mut BareIpcPollT, data: *mut c_void);
    fn bare_ipc_poll_start(poll: *mut BareIpcPollT, events: c_int, cb: BareIpcPollCb) -> c_int;
    fn bare_ipc_poll_stop(poll: *mut BareIpcPollT) -> c_int;
}

/// Set once a worklet was started in this process (then [`teardown`] has work to do).
static STARTED: AtomicBool = AtomicBool::new(false);
/// Set by [`teardown`]; no worklet starts afterwards.
static TORN_DOWN: AtomicBool = AtomicBool::new(false);

/// `bare_worklet_teardown`: waits for every worklet still tearing down, then destroys the JS
/// platform the worklets of the process share. Runs at most once per process and only when a
/// worklet was started; no worklet can start afterwards. Blocks for as long as a worklet does
/// not finish tearing down.
pub(super) fn teardown() {
    if TORN_DOWN.swap(true, Ordering::AcqRel) || !STARTED.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: called at most once (the swap above); bare-kit allows it at any time and waits for
    // running worklets itself. Every `Worklet` destroys its handle in `Drop`, and a worklet that
    // is still running when this is called was terminated by that destroy.
    unsafe { bare_worklet_teardown() };
}

/// Called by bare-kit when the IPC became readable or writable, on a thread owned by the IPC.
unsafe extern "C" fn on_poll(poll: *mut BareIpcPollT, _events: c_int) {
    // SAFETY: `poll` is the handle bare-kit calls us for; its data pointer was set to a live
    // `Box<PeerWaker>` before the poll was started, and that box is freed only after
    // `bare_ipc_poll_destroy` returned (`Worklet::drop`), which waits for a running callback.
    let waker = unsafe { bare_ipc_poll_get_data(poll) }.cast::<PeerWaker>();
    // SAFETY: see above; `PeerWaker` is `Sync`, so a shared reference from this thread is fine.
    if let Some(w) = unsafe { waker.as_ref() } {
        w.wake();
    }
}

/// What the exit callback reports, shared between the worklet thread and the pump thread.
struct ExitState {
    /// The worklet's exit code once it has torn down.
    code: Mutex<Option<c_int>>,
    changed: Condvar,
    /// Wakes the pump, so it notices the end without waiting for its timer.
    waker: PeerWaker,
}

impl ExitState {
    fn code(&self) -> Option<c_int> {
        *self.code.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until the exit callback ran or `timeout` passed; returns the exit code if it ran.
    fn wait(&self, timeout: Duration) -> Option<c_int> {
        let guard = self.code.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _timed_out) = self
            .changed
            .wait_timeout_while(guard, timeout, |c| c.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// Called by bare-kit exactly once on the worklet thread, after the worklet tore down.
unsafe extern "C" fn on_exit(exit_code: c_int, data: *mut c_void) {
    // SAFETY: `data` is the `Arc<ExitState>` reference handed over with `Arc::into_raw` in
    // `Worklet::start` for this callback only; bare-kit calls it once, so it is taken back once.
    let state = unsafe { Arc::from_raw(data.cast::<ExitState>().cast_const()) };
    *state.code.lock().unwrap_or_else(PoisonError::into_inner) = Some(exit_code);
    state.changed.notify_all();
    state.waker.wake();
}

/// Result of [`Worklet::read`].
pub(super) enum IpcRead {
    /// This many bytes (at least 1) were copied to the start of the buffer.
    Data(usize),
    /// Nothing to read now.
    WouldBlock,
    /// End of stream: the worklet ended its side of the IPC, or exited.
    End,
    /// An unexpected bare-kit return value.
    Failed(c_int),
}

/// Result of [`Worklet::write`].
pub(super) enum IpcWrite {
    /// This many bytes were taken (0: no room now).
    Wrote(usize),
    /// The IPC is closed: the worklet closed its side or exited.
    Closed,
    /// An unexpected bare-kit return value.
    Failed(c_int),
}

/// One Bare worklet (its own thread and JS event loop inside bare-kit) and the host end of its
/// IPC channel.
pub(super) struct Worklet {
    handle: *mut BareWorkletT,
    ipc: *mut BareIpcT,
    poll: *mut BareIpcPollT,
    /// The poll callback's data: a boxed waker, freed after the poll handle is destroyed.
    poll_data: *mut PeerWaker,
    /// Filled by the exit callback (which holds its own reference while the worklet runs).
    exit: Arc<ExitState>,
    /// `bare_worklet_start` succeeded, so the exit callback will run.
    started: bool,
    /// Events the poll handle is started for (0 = stopped).
    poll_events: c_int,
}

fn check(err: c_int, what: &str) -> Result<(), String> {
    if err == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed ({err})"))
    }
}

impl Worklet {
    /// Creates a worklet, its IPC and the IPC's poll, then starts the worklet running `script`
    /// with `args` as `Bare.argv`. `wake` is woken whenever the IPC becomes readable or writable
    /// (once [`Worklet::watch`] asked for it) and when the worklet exits.
    pub(super) fn start(
        script: &Path,
        args: &[String],
        memory_limit: usize,
        wake: PeerWaker,
    ) -> Result<Worklet, String> {
        if TORN_DOWN.load(Ordering::Acquire) {
            return Err("bare-kit was torn down; no worklet can start".into());
        }
        let filename = script
            .to_str()
            .ok_or_else(|| format!("worker script path is not UTF-8: {}", script.display()))
            .and_then(|s| CString::new(s).map_err(|e| e.to_string()))?;
        let c_args = args
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let arg_ptrs: Vec<*const c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
        let arg_count = c_int::try_from(arg_ptrs.len()).map_err(|e| e.to_string())?;

        let exit = Arc::new(ExitState {
            code: Mutex::new(None),
            changed: Condvar::new(),
            waker: wake.clone(),
        });
        let mut w = Worklet {
            handle: ptr::null_mut(),
            ipc: ptr::null_mut(),
            poll: ptr::null_mut(),
            poll_data: Box::into_raw(Box::new(wake)),
            exit,
            started: false,
            poll_events: 0,
        };
        // On any error below, dropping `w` releases what was set up so far.

        let options = BareWorkletOptions {
            version: 0,
            memory_limit,
            platform: [ptr::null_mut(); 8],
        };
        // SAFETY: `options` lives across the call (bare-kit copies it); the out-pointer is valid
        // for writes.
        check(
            unsafe { bare_worklet_create(&raw const options, &raw mut w.handle) },
            "bare_worklet_create",
        )?;
        // SAFETY: `w.handle` was just created and is not started; the out-pointer is valid.
        check(
            unsafe { bare_ipc_create(w.handle, &raw mut w.ipc) },
            "bare_ipc_create",
        )?;
        // SAFETY: `w.ipc` was just created; the out-pointer is valid.
        check(
            unsafe { bare_ipc_poll_create(w.ipc, &raw mut w.poll) },
            "bare_ipc_poll_create",
        )?;
        // SAFETY: `w.poll` was just created and not started; `poll_data` stays valid until after
        // `bare_ipc_poll_destroy` (see `Drop`).
        unsafe { bare_ipc_poll_set_data(w.poll, w.poll_data.cast()) };

        // The exit callback gets a reference of its own, taken back by `on_exit`.
        let exit_data = Arc::into_raw(w.exit.clone()).cast_mut().cast::<c_void>();
        // SAFETY: the worklet is created and not started, as `bare_worklet_on_exit` requires;
        // `exit_data` stays valid until `on_exit` takes it back.
        let registered = unsafe { bare_worklet_on_exit(w.handle, on_exit, exit_data) };
        let err = if registered == 0 {
            // SAFETY: the worklet is created and not started; `filename` and every `argv` string
            // are NUL-terminated and need only stay valid until `bare_worklet_start` returns;
            // `source` null with `len` 0 makes Bare read `filename` from disk.
            unsafe {
                bare_worklet_start(
                    w.handle,
                    filename.as_ptr(),
                    ptr::null(),
                    0,
                    arg_count,
                    arg_ptrs.as_ptr(),
                )
            }
        } else {
            registered
        };
        if err != 0 {
            // Not started: the exit callback never runs, so its reference is taken back here.
            // SAFETY: `exit_data` came from `Arc::into_raw` above and was not handed out.
            drop(unsafe { Arc::from_raw(exit_data.cast::<ExitState>().cast_const()) });
            let what = if registered == 0 {
                "bare_worklet_start"
            } else {
                "bare_worklet_on_exit"
            };
            return Err(format!("{what} failed ({err})"));
        }
        w.started = true;
        STARTED.store(true, Ordering::Release);
        Ok(w)
    }

    /// Copies what the worklet wrote into `buf` (from its start).
    pub(super) fn read(&mut self, buf: &mut [u8]) -> IpcRead {
        if buf.is_empty() {
            return IpcRead::WouldBlock;
        }
        // SAFETY: `self.ipc` is created (only `start` returns a `Worklet`); `buf` is valid for
        // writes of `buf.len()` bytes (not 0) during the call, and bare-kit copies into it.
        let n = unsafe { bare_ipc_read(self.ipc, buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            0 => IpcRead::End,
            BARE_IPC_WOULD_BLOCK => IpcRead::WouldBlock,
            n => usize::try_from(n).map_or(IpcRead::Failed(n), IpcRead::Data),
        }
    }

    /// Offers `data` to the worklet; returns how much it took.
    pub(super) fn write(&mut self, data: &[u8]) -> IpcWrite {
        if data.is_empty() {
            return IpcWrite::Wrote(0);
        }
        // SAFETY: `self.ipc` is created; `data` is valid for `data.len()` bytes during the call,
        // and bare-kit copies what it takes.
        let n = unsafe { bare_ipc_write(self.ipc, data.as_ptr().cast(), data.len()) };
        match n {
            BARE_IPC_WOULD_BLOCK => IpcWrite::Wrote(0),
            BARE_IPC_ERROR => IpcWrite::Closed,
            n => usize::try_from(n).map_or(IpcWrite::Failed(n), IpcWrite::Wrote),
        }
    }

    /// Asks bare-kit to call back (waking the [`PeerWaker`]) when the IPC becomes readable
    /// and/or writable; both false stops the callbacks. False if bare-kit refused, in which case
    /// the caller has to poll on a timer.
    pub(super) fn watch(&mut self, readable: bool, writable: bool) -> bool {
        let mut events = 0;
        if readable {
            events |= BARE_IPC_READABLE;
        }
        if writable {
            events |= BARE_IPC_WRITABLE;
        }
        if events == self.poll_events {
            return true;
        }
        let err = if events == 0 {
            // SAFETY: the poll handle is created.
            unsafe { bare_ipc_poll_stop(self.poll) }
        } else {
            // SAFETY: the poll handle is created; `on_poll` only reads the data pointer set in
            // `start`.
            unsafe { bare_ipc_poll_start(self.poll, events, on_poll) }
        };
        if err == 0 {
            self.poll_events = events;
            true
        } else {
            false
        }
    }

    /// The worklet's exit code, once it has exited and torn down.
    pub(super) fn exit_code(&self) -> Option<i32> {
        self.exit.code()
    }

    /// Ends the worklet: half-closes the IPC (`Bare.IPC` reads end-of-stream, and `worker.js`
    /// then closes its session, leaves the DHT and lets the worklet exit), discarding whatever
    /// the worklet still writes, for at most `grace`; then terminates it if it has not exited
    /// and waits at most `terminate_wait` more. Returns the exit code if the worklet exited in
    /// that time. The handles are freed when the `Worklet` is dropped.
    pub(super) fn shutdown(&mut self, grace: Duration, terminate_wait: Duration) -> Option<i32> {
        if !self.started {
            return None;
        }
        self.watch(false, false);
        // SAFETY: `self.ipc` is created; ending twice is a no-op.
        unsafe { bare_ipc_end(self.ipc) };
        let deadline = Instant::now() + grace;
        let mut scratch = vec![0u8; IPC_READ_BUFFER_SIZE];
        loop {
            // Drain, so a worklet that still writes can finish its own end.
            while let IpcRead::Data(_) = self.read(&mut scratch) {}
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(code) = self
                .exit
                .wait((deadline - now).min(Duration::from_millis(20)))
            {
                return Some(code);
            }
        }
        // SAFETY: the worklet is started and not destroyed; terminating an exited worklet is a
        // no-op.
        unsafe { bare_worklet_terminate(self.handle) };
        self.exit.wait(terminate_wait)
    }
}

impl Drop for Worklet {
    fn drop(&mut self) {
        // SAFETY (whole block): every handle is destroyed at most once, only if it was created,
        // on the thread that owns the worklet. `bare_ipc_poll_destroy` waits for a running poll
        // callback, so `poll_data` is freed after the last callback. `bare_worklet_destroy`
        // terminates a worklet that still runs and returns at once; its exit callback then runs
        // later on the worklet thread with its own `ExitState` reference.
        unsafe {
            if !self.poll.is_null() {
                bare_ipc_poll_destroy(self.poll);
            }
            if !self.ipc.is_null() {
                bare_ipc_destroy(self.ipc);
            }
            if !self.handle.is_null() {
                bare_worklet_destroy(self.handle);
            }
            drop(Box::from_raw(self.poll_data));
        }
    }
}

/// `dlopen` and `dlerror` of `<dlfcn.h>` (glibc and musl), with the flag values both use on
/// Linux.
#[cfg(target_os = "linux")]
mod dl {
    use std::ffi::{c_char, c_int, c_void};

    /// `RTLD_NOW`: resolve every symbol at load time.
    pub(super) const RTLD_NOW: c_int = 0x2;
    /// `RTLD_GLOBAL`: the library's symbols serve every library loaded later.
    pub(super) const RTLD_GLOBAL: c_int = 0x100;

    unsafe extern "C" {
        pub(super) fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
        pub(super) fn dlerror() -> *mut c_char;
    }
}

/// Loads the shared library `name` (a soname, searched like the dynamic loader does, or a path
/// with a `/`) with `RTLD_NOW | RTLD_GLOBAL` for the rest of the process: the handle is never
/// closed. A library the Bare addons list as needed is then found by its soname when bare-kit
/// loads them. The error is `dlerror`'s text.
#[cfg(target_os = "linux")]
pub(super) fn load_library_global(name: &std::ffi::OsStr) -> Result<(), String> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt as _;

    let c_name = CString::new(name.as_bytes())
        .map_err(|_| format!("{} contains a NUL byte", name.to_string_lossy()))?;
    // SAFETY: `c_name` is a NUL-terminated string that outlives the call. `dlopen` runs the
    // library's constructors; the names passed here are C/C++ runtime libraries.
    let handle = unsafe { dl::dlopen(c_name.as_ptr(), dl::RTLD_NOW | dl::RTLD_GLOBAL) };
    if !handle.is_null() {
        // The handle stays open on purpose: the library must stay loaded for the addons.
        return Ok(());
    }
    // SAFETY: `dlerror` returns null or a NUL-terminated string owned by the C library that
    // stays valid until the next `dl*` call on this thread; it is copied at once.
    let err = unsafe { dl::dlerror() };
    if err.is_null() {
        Err(format!("cannot load {}", name.to_string_lossy()))
    } else {
        // SAFETY: see above.
        Err(unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned())
    }
}
