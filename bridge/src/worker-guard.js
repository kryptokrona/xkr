// Imported first by worker.js, so the handlers are in place before the other modules (and their
// native addons) load: an uncaught error in a worklet would otherwise end the whole host process.
// Only this worker ends; xkrd reads end-of-stream on its IPC, fails the bridge and restarts it.
//
// Before it exits, the worker tells xkrd why: the error's name and message go out on one line in
// one Error frame on the IPC. A worker whose modules did not load (a native addon that cannot
// load, for example) has sent no Hello yet; xkrd takes an Error frame in place of the Hello as the
// reason the worker could not start and shows it, instead of only seeing the IPC close.

/* global Bare */

import { ERROR_CODE, encodeFrame, frames } from './frames.js'

/** How long the Error frame gets to reach xkrd before the worker exits anyway. */
export const EXIT_GRACE_MS = 1000
/** Longest reason sent, in characters. */
export const REASON_MAX = 300

/**
 * The reason sent to xkrd: the error's name and message (not its stack) on one line, with every
 * run of white space as one space and other control characters (ESC, DEL, C1) left out, at most
 * REASON_MAX characters.
 */
export function reasonOf(err) {
  let text
  try {
    text = err && typeof err === 'object' && 'message' in err
      ? `${err.name || 'Error'}: ${err.message}`
      : String(err)
  } catch {
    text = ''
  }
  // eslint-disable-next-line no-control-regex
  const line = text.replace(/\s+/g, ' ').replace(/[\u0000-\u001f\u007f-\u009f]/g, '').trim()
  return (line || 'unknown error').slice(0, REASON_MAX)
}

/** This worker's index from its arguments (`--worker <i>`), 0 if absent. */
export function workerIndex(argv) {
  const i = Array.isArray(argv) ? argv.indexOf('--worker') : -1
  const n = i < 0 ? 0 : Number(argv[i + 1])
  return Number.isInteger(n) && n >= 0 && n <= 0x7fff ? n : 0
}

/** The Error frame that tells xkrd why this worker ends. */
export function failureFrame(err, worker = 0) {
  const f = frames.error(0, ERROR_CODE.DHT_ERROR, reasonOf(err))
  f.shard = worker
  return encodeFrame(f)
}

/**
 * Sends the failure frame on `ipc` (if it is still writable), ends it and calls `exit(1)` once
 * the frame is out, or after EXIT_GRACE_MS at the latest.
 */
export function reportAndExit(ipc, err, opts = {}) {
  const { worker = 0, exit, log = console.error, setTimer = setTimeout } = opts
  let done = false
  const finish = () => {
    if (done) return
    done = true
    exit(1)
  }
  try {
    if (ipc && !ipc.destroyed && typeof ipc.write === 'function') {
      ipc.write(failureFrame(err, worker))
      if (typeof ipc.end === 'function') ipc.end()
      if (typeof ipc.once === 'function') {
        ipc.once('finish', finish)
        ipc.once('close', finish)
      }
    }
  } catch (e) {
    log(`[hyper-bridge] cannot send the reason to xkrd: ${(e && e.message) || e}`)
  }
  setTimer(finish, EXIT_GRACE_MS)
  return finish
}

let failing = false

/** Logs `err` (system log), tells xkrd and ends this worker; a later failure only logs. */
export function failWorker(what, err) {
  console.error(`[hyper-bridge] ${what}: ${(err && err.stack) || err}`)
  if (failing) return
  failing = true
  Bare.exitCode = 1
  reportAndExit(Bare.IPC, err, {
    worker: workerIndex(Bare.argv),
    exit: (code) => Bare.exit(code)
  })
}

if (typeof Bare !== 'undefined' && Bare && Bare.IPC) {
  Bare.on('uncaughtException', (err) => failWorker('uncaught exception', err))
  Bare.on('unhandledRejection', (err) => failWorker('unhandled rejection', err))
}
