// Entry of one Bare worker of xkrd's Hyperswarm bridge (docs/hyper-transport.md). xkrd starts it
// through bare-kit 3.x's C API (bare_worklet_start); the worker itself is src/worker-main.js.
//
// No static imports: they are all resolved before any module runs, so one that cannot be resolved
// (a package missing from node_modules) fails before worker-guard.js has its handlers in place,
// and Bare then aborts the whole xkrd process. import() fails as an ordinary rejection instead:
// once the guard is loaded it tells xkrd why the worker could not start (an Error frame in place
// of the Hello); if the guard itself cannot load, the worker only logs and exits, and xkrd sees
// its IPC end before the Hello.

/* global Bare */

import('./worker-guard.js').then(
  ({ failWorker }) =>
    import('./worker-main.js').catch((err) => failWorker('modules failed to load', err)),
  (err) => {
    console.error(`[hyper-bridge] modules failed to load: ${(err && err.stack) || err}`)
    Bare.exitCode = 1
    Bare.exit(1)
  }
)
