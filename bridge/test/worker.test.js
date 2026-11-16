// The worker entry (src/worker.js) and the worker index in Session (src/session.js),
// under Node: no Bare needed (worker.js only starts itself when Bare.IPC exists).

import test from 'node:test'
import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import b4a from 'b4a'
import { parseWorkerArgs, ARGS_MARKER } from '../src/worker.js'
import { Session } from '../src/session.js'
import {
  TYPE,
  FLAG,
  DHT_STATE,
  NAT,
  VERSION,
  FEATURE,
  ERROR_CODE,
  DEFAULT_MAX_FRAME,
  DEFAULT_INITIAL_CREDIT,
  FrameDecoder,
  encodeFrame,
  frames,
  makeConnId,
  connIdShard
} from '../src/frames.js'

test('worker arguments', () => {
  assert.equal(ARGS_MARKER, '--xkr-worker')
  const seed = '07'.repeat(32)
  const o = parseWorkerArgs([
    '--worker', '1', '--workers', '2', '--seed', seed,
    '--bootstrap', '127.0.0.1:49737', '--max-frame', '65536', '--initial-credit', '4096', '--log-json'
  ])
  assert.equal(o.worker, 1)
  assert.equal(o.workers, 2)
  assert.deepEqual(o.seed, b4a.alloc(32, 7))
  assert.deepEqual(o.bootstrap, [{ host: '127.0.0.1', port: 49737 }])
  assert.equal(o.maxFrame, 65536)
  assert.equal(o.initialCredit, 4096)
  assert.equal(o.logJson, true)
  assert.equal(o.portFixed, 0, 'no fixed port by default')
  assert.equal(parseWorkerArgs(['--seed', seed, '--port-fixed', '21890']).portFixed, 21890)
  assert.throws(() => parseWorkerArgs(['--seed', seed, '--port-fixed', '0']), /invalid --port-fixed/)
  assert.throws(() => parseWorkerArgs(['--seed', seed, '--port-fixed', '65536']), /invalid --port-fixed/)
  assert.throws(() => parseWorkerArgs(['--worker', '2', '--workers', '2', '--seed', seed]))
  assert.throws(() => parseWorkerArgs(['--worker', '0']), /--seed/)
  assert.throws(() => parseWorkerArgs(['--seed', 'xx']))
  assert.throws(() => parseWorkerArgs(['--seed', seed, '--port', '1']), /unknown argument/)
})

/** A stand-in for Bare.IPC: collects what the session writes. */
class FakeIpc extends EventEmitter {
  constructor() {
    super()
    this.decoder = new FrameDecoder()
    this.frames = []
    this.destroyed = false
  }

  write(buf) {
    this.frames.push(...this.decoder.push(buf))
    return true
  }

  end() {
    this.destroy()
  }

  destroy() {
    if (this.destroyed) return
    this.destroyed = true
    this.emit('close')
  }
}

function session(worker, workers) {
  const ipc = new FakeIpc()
  const ctx = {
    opts: { maxFrame: DEFAULT_MAX_FRAME, initialCredit: DEFAULT_INITIAL_CREDIT, peerInfoInterval: 0 },
    log: () => {},
    worker,
    workers,
    swarm: { keyPair: { publicKey: b4a.alloc(32, 1) } },
    onsessionclose: () => {}
  }
  return { ipc, session: new Session(ipc, ctx) }
}

function rustHello(worker, workers) {
  return encodeFrame({
    ...frames.hello({
      major: VERSION.major,
      minor: VERSION.minor,
      maxFrame: DEFAULT_MAX_FRAME,
      initialCredit: DEFAULT_INITIAL_CREDIT,
      workers,
      worker,
      features: FEATURE.HUGIN_MID,
      tlvs: []
    }),
    shard: worker
  })
}

test('a session names its worker in Hello, header.shard and inbound conn ids', () => {
  const { ipc, session: s } = session(1, 2)
  try {
    const hello = ipc.frames[0]
    assert.equal(hello.type, TYPE.HELLO)
    assert.equal(hello.shard, 1)
    assert.equal(hello.body.worker, 1)
    assert.equal(hello.body.workers, 2)

    ipc.emit('data', rustHello(1, 2))
    ipc.emit('data', encodeFrame({ ...frames.ping(7n), shard: 1 }))
    const pong = ipc.frames.find((f) => f.type === TYPE.PONG)
    assert.ok(pong, 'Ping answered')
    assert.equal(pong.shard, 1)

    // Inbound connection ids carry the worker in bits 48..62.
    const fakeSocket = new EventEmitter()
    fakeSocket.opened = new Promise(() => {})
    fakeSocket.destroy = () => {}
    s.onswarmconnection(fakeSocket, { topic: null, identity: null })
    const [id] = s.conns.keys()
    assert.equal(id, makeConnId(true, 1, 1))
    assert.equal(connIdShard(id), 1)
  } finally {
    ipc.destroy()
  }
})

test('a frame for another worker is a fatal BAD_FRAME', () => {
  const { ipc } = session(1, 2)
  ipc.emit('data', rustHello(1, 2))
  ipc.emit('data', encodeFrame({ ...frames.ping(7n), shard: 0 }))
  const err = ipc.frames.find((f) => f.type === TYPE.ERROR)
  assert.ok(err, 'error sent')
  assert.equal(err.body.code, ERROR_CODE.BAD_FRAME)
  ipc.destroy()
})

test('a session without a worker index is worker 0 of 1', () => {
  const ipc = new FakeIpc()
  const s = new Session(ipc, {
    opts: { maxFrame: DEFAULT_MAX_FRAME, initialCredit: DEFAULT_INITIAL_CREDIT, peerInfoInterval: 0 },
    log: () => {},
    swarm: { keyPair: { publicKey: b4a.alloc(32, 1) } },
    onsessionclose: () => {}
  })
  assert.equal(s.worker, 0)
  assert.equal(ipc.frames[0].body.workers, 1)
  assert.equal(ipc.frames[0].shard, 0)
  ipc.destroy()
})

test('DhtStatus goes out after the client Hello, once per state, with IGNORABLE', () => {
  const ipc = new FakeIpc()
  let status = { state: DHT_STATE.STARTING, firewalled: true, nat: NAT.UNKNOWN, udpPort: 0 }
  const s = new Session(ipc, {
    opts: { maxFrame: DEFAULT_MAX_FRAME, initialCredit: DEFAULT_INITIAL_CREDIT, peerInfoInterval: 0 },
    log: () => {},
    swarm: { keyPair: { publicKey: b4a.alloc(32, 1) }, status: () => status },
    onsessionclose: () => {}
  })
  try {
    const statuses = () => ipc.frames.filter((f) => f.type === TYPE.DHT_STATUS)
    s.sendDhtStatus()
    assert.equal(statuses().length, 0, 'nothing before the client Hello')

    ipc.emit('data', rustHello(0, 1))
    assert.equal(statuses().length, 1, 'the current state right after the Hello')
    const first = statuses()[0]
    assert.equal(first.flags & FLAG.IGNORABLE, FLAG.IGNORABLE)
    assert.equal(first.connId, 0n)
    assert.equal(first.body.state, DHT_STATE.STARTING)

    s.sendDhtStatus()
    assert.equal(statuses().length, 1, 'the same state is not sent again')

    status = { state: DHT_STATE.READY, firewalled: true, nat: NAT.RANDOM, udpPort: 21890 }
    s.sendDhtStatus()
    assert.equal(statuses().length, 2)
    assert.deepEqual(statuses()[1].body, {
      state: DHT_STATE.READY, firewalled: true, nat: NAT.RANDOM, udpPort: 21890, tlvs: []
    })
    s.sendDhtStatus({ state: DHT_STATE.READY, firewalled: false, nat: NAT.CONSISTENT, udpPort: 21890 })
    assert.equal(statuses().length, 3, 'a state passed in is sent too')
    assert.equal(statuses()[2].body.firewalled, false)
  } finally {
    ipc.destroy()
  }
})
