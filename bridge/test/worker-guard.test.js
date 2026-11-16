// The worker's last words (src/worker-guard.js): a worker that fails, before or after its Hello,
// sends one Error frame with the error on one line on its IPC, ends it and exits with 1.
// Runs under Node: the guard only installs its handlers when Bare.IPC exists.

import test from 'node:test'
import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import {
  EXIT_GRACE_MS,
  REASON_MAX,
  failureFrame,
  reasonOf,
  reportAndExit,
  workerIndex
} from '../src/worker-guard.js'
import { ERROR_CODE, FrameDecoder, TYPE } from '../src/frames.js'

/** An addon load error as bare-addon-resolve throws it (name, code, multi-line message). */
function addonError() {
  const err = new Error(
    'CANNOT_LOAD: Cannot load addon file:///x/node_modules/udx-native/prebuilds/linux-x64/udx-native.bare\n' +
    '  libstdc++.so.6: cannot open shared object file: No such file or directory'
  )
  err.name = 'AddonError'
  err.code = 'CANNOT_LOAD'
  return err
}

test('the reason is the error on one line, without its stack', () => {
  assert.equal(
    reasonOf(addonError()),
    'AddonError: CANNOT_LOAD: Cannot load addon ' +
    'file:///x/node_modules/udx-native/prebuilds/linux-x64/udx-native.bare ' +
    'libstdc++.so.6: cannot open shared object file: No such file or directory'
  )
  assert.equal(reasonOf('\n  plain text  \nmore'), 'plain text more')
  assert.equal(reasonOf(undefined), 'undefined')
  assert.equal(reasonOf(''), 'unknown error')
  assert.equal(reasonOf(new Error('x'.repeat(1000))).length, REASON_MAX)
})

test('the reason has no control characters', () => {
  const err = new Error('bad \u001b[31mred\u001b[0m\u0007 \u007f\u009bdone\ttab')
  assert.equal(reasonOf(err), 'Error: bad [31mred[0m done tab')
  assert.equal(reasonOf('\u0000\u001b'), 'unknown error')
})

test('the worker index comes from --worker', () => {
  assert.equal(workerIndex(['--xkr-worker', '--worker', '2', '--workers', '3']), 2)
  assert.equal(workerIndex(['--xkr-worker', '--seed', 'ab']), 0)
  assert.equal(workerIndex(['--worker', 'x']), 0)
  assert.equal(workerIndex(undefined), 0)
})

test('the failure frame is an Error frame with the reason', () => {
  const [f] = new FrameDecoder().push(failureFrame(addonError(), 1))
  assert.equal(f.type, TYPE.ERROR)
  assert.equal(f.shard, 1)
  assert.equal(f.connId, 0n)
  assert.equal(f.body.requestId, 0)
  assert.equal(f.body.code, ERROR_CODE.DHT_ERROR)
  assert.match(f.body.message, /^AddonError: CANNOT_LOAD/)
})

/** A stand-in for Bare.IPC: collects writes, emits 'finish' on end. */
class FakeIpc extends EventEmitter {
  constructor() {
    super()
    this.written = []
    this.ended = false
    this.destroyed = false
  }

  write(buf) {
    this.written.push(buf)
    return true
  }

  end() {
    this.ended = true
    queueMicrotask(() => this.emit('finish'))
  }
}

test('a startup error reaches the IPC as an Error frame before the worker exits', async () => {
  const ipc = new FakeIpc()
  const exits = []
  const timers = []
  const exited = new Promise((resolve) => {
    reportAndExit(ipc, addonError(), {
      worker: 0,
      exit: (code) => { exits.push({ code, ended: ipc.ended, written: ipc.written.length }); resolve() },
      setTimer: (fn, ms) => timers.push({ fn, ms })
    })
  })
  // Before Hello: the Error frame is the first and only thing on the IPC.
  const frames = new FrameDecoder().push(Buffer.concat(ipc.written))
  assert.equal(frames.length, 1)
  assert.equal(frames[0].type, TYPE.ERROR)
  assert.match(frames[0].body.message, /libstdc\+\+\.so\.6: cannot open shared object file/)
  assert.equal(ipc.ended, true)
  await exited
  assert.deepEqual(exits, [{ code: 1, ended: true, written: 1 }], 'exit once, after the frame')
  // The fallback timer does not exit a second time.
  assert.equal(timers.length, 1)
  assert.equal(timers[0].ms, EXIT_GRACE_MS)
  timers[0].fn()
  ipc.emit('close')
  assert.equal(exits.length, 1)
})

test('without a usable IPC the worker still exits after the grace time', () => {
  const exits = []
  const timers = []
  const destroyed = Object.assign(new FakeIpc(), { destroyed: true })
  reportAndExit(destroyed, new Error('boom'), {
    exit: (code) => exits.push(code),
    setTimer: (fn, ms) => timers.push({ fn, ms })
  })
  assert.equal(destroyed.written.length, 0)
  assert.deepEqual(exits, [])
  timers[0].fn()
  assert.deepEqual(exits, [1])

  // An IPC whose write throws: logged, and the timer still ends the worker.
  const logs = []
  const broken = { write: () => { throw new Error('closed') } }
  const t2 = []
  const e2 = []
  reportAndExit(broken, new Error('boom'), {
    exit: (code) => e2.push(code),
    log: (l) => logs.push(l),
    setTimer: (fn) => t2.push(fn)
  })
  assert.match(logs[0], /cannot send the reason/)
  t2[0]()
  assert.deepEqual(e2, [1])
})
