// End-to-end: two bridge workers (startWorker of src/worker.js, the code each Bare worker runs
// inside xkrd) on a local in-process testnet, run under Node over in-memory streams and driven
// by a minimal JS implementation of the Rust side of the frame protocol.

import test from 'node:test'
import assert from 'node:assert/strict'
import { EventEmitter } from 'node:events'
import crypto from 'node:crypto'
import dgram from 'node:dgram'
import b4a from 'b4a'
import createTestnet from 'hyperdht/testnet.js'
import { startWorker } from '../src/worker-main.js'
import { keyPairFromSeed, huginTopic } from '../src/mid.js'
import {
  TYPE,
  FLAG,
  TLV,
  DHT_STATE,
  CLOSE_REASON,
  FrameDecoder,
  encodeFrame,
  findTlv,
  connIdFromBare
} from '../src/frames.js'

/** One end of an in-memory byte stream pair (the part of Bare.IPC that Session uses). */
class PipeEnd extends EventEmitter {
  constructor() {
    super()
    this.peer = null
    this.closed = false
  }

  write(buf) {
    if (this.closed) return false
    const peer = this.peer
    const copy = b4a.from(buf)
    queueMicrotask(() => {
      if (!peer.closed) peer.emit('data', copy)
    })
    return true
  }

  end() {
    this.destroy()
  }

  destroy() {
    if (this.closed) return
    this.closed = true
    // Like a real stream, bytes already written reach the peer before it sees the close:
    // writes queue in order, so closing in a later microtask delivers them first.
    queueMicrotask(() => {
      for (const end of [this, this.peer]) {
        end.closed = true
        end.emit('close')
      }
    })
  }
}

function pipePair() {
  const a = new PipeEnd()
  const b = new PipeEnd()
  a.peer = b
  b.peer = a
  return [a, b]
}

/** A UDP port that was free a moment ago. */
async function freeUdpPort() {
  const sock = dgram.createSocket('udp4')
  await new Promise((resolve) => sock.bind(0, '0.0.0.0', resolve))
  const { port } = sock.address()
  await new Promise((resolve) => sock.close(resolve))
  return port
}

/** Starts one worker; returns its context and the Rust-side end of its stream. */
function spawnWorker(bootstrap, extra = []) {
  const [rust, ipc] = pipePair()
  const argv = ['--seed', crypto.randomBytes(32).toString('hex'), '--log-level', 'warn']
  for (const b of bootstrap) argv.push('--bootstrap', `${b.host}:${b.port}`)
  argv.push(...extra)
  const ctx = startWorker(ipc, argv)
  return { ctx, stream: rust }
}

class Client {
  constructor(stream, { initialCredit = 1 << 20 } = {}) {
    this.frames = []
    this.waiters = []
    this.decoder = new FrameDecoder()
    this.initialCredit = initialCredit
    this.socket = stream
    this.socket.on('data', (d) => {
      for (const f of this.decoder.push(d)) {
        if (f.type === TYPE.PING) {
          this.send({ type: TYPE.PONG, body: { nonce: f.body.nonce } })
          continue
        }
        this.frames.push(f)
      }
      this._check()
    })
    this.send({
      type: TYPE.HELLO,
      body: {
        major: 1, minor: 0, maxFrame: 1 << 24, initialCredit, workers: 1, worker: 0, features: 1n, tlvs: []
      }
    })
  }

  send(f) {
    this.socket.write(encodeFrame({ flags: 0, shard: 0, connId: 0n, ...f }))
  }

  _check() {
    for (const w of [...this.waiters]) {
      const i = this.frames.findIndex(w.pred)
      if (i !== -1) {
        const [f] = this.frames.splice(i, 1)
        this.waiters.splice(this.waiters.indexOf(w), 1)
        clearTimeout(w.timer)
        w.resolve(f)
      }
    }
  }

  next(pred, ms = 20000, what = 'frame') {
    return new Promise((resolve, reject) => {
      const w = { pred, resolve }
      w.timer = setTimeout(() => reject(new Error(`timeout waiting for ${what}`)), ms)
      this.waiters.push(w)
      this._check()
    })
  }

  close() {
    this.socket.destroy()
  }
}

test('two workers: hello, announce, lookup, open, data, credit, close', async (t) => {
  const testnet = await createTestnet(3, { host: '127.0.0.1' })
  const a = spawnWorker(testnet.bootstrap, ['--initial-credit', '65536'])
  const b = spawnWorker(testnet.bootstrap)
  const ca = new Client(a.stream, { initialCredit: 1 << 20 })
  const cb = new Client(b.stream, { initialCredit: 1 << 20 })
  t.after(async () => {
    await Promise.allSettled([a, b].map((w) => w.ctx.swarm.destroy()))
    ca.close()
    cb.close()
    await testnet.destroy()
  })

  const helloA = await ca.next((f) => f.type === TYPE.HELLO, 5000, 'hello')
  assert.equal(helloA.body.initialCredit, 65536)
  const pubA = findTlv(helloA.body.tlvs, TLV.PUBLIC_KEY)
  assert.equal(pubA.byteLength, 32)
  await cb.next((f) => f.type === TYPE.HELLO, 5000, 'hello')

  const topic = crypto.randomBytes(32)
  ca.send({ type: TYPE.ANNOUNCE, body: { requestId: 1, topic, mode: 1, tlvs: [] } })
  const ack = await ca.next((f) => f.type === TYPE.ACK && f.body.requestId === 1, 20000, 'ack')
  assert.ok(b4a.equals(findTlv(ack.body.tlvs, TLV.PUBLIC_KEY), pubA))

  cb.send({ type: TYPE.LOOKUP, body: { requestId: 2, topic, maxPeers: 10, tlvs: [] } })
  const lr = await cb.next((f) => f.type === TYPE.LOOKUP_RESULT && f.body.requestId === 2, 20000, 'lookup')
  assert.equal(lr.flags & FLAG.END, FLAG.END)
  assert.ok(lr.body.peers.some((p) => b4a.equals(p.publicKey, pubA)))

  // b opens a with the client-chosen conn id 1
  cb.send({
    type: TYPE.OPEN,
    connId: 1n,
    body: { requestId: 3, remotePublicKey: pubA, topic, openFlags: 0, tlvs: [] }
  })
  const openedB = await cb.next((f) => f.type === TYPE.OPENED && f.connId === 1n, 20000, 'opened b')
  assert.equal(openedB.body.requestId, 3)
  assert.equal(openedB.body.initiator, true)
  assert.ok(b4a.equals(openedB.body.remotePublicKey, pubA))
  const openedA = await ca.next((f) => f.type === TYPE.OPENED, 20000, 'opened a')
  assert.ok(connIdFromBare(openedA.connId))
  assert.equal(openedA.body.requestId, 0)
  assert.equal(openedA.body.initiator, false)
  assert.ok(b4a.equals(openedA.body.handshakeHash, openedB.body.handshakeHash))
  const idA = openedA.connId

  // b -> a: one Data frame per message, credits come back to b
  cb.send({ type: TYPE.DATA, connId: 1n, body: { payload: b4a.from('ping over hyperdht') } })
  const credit = await cb.next((f) => f.type === TYPE.CREDIT && f.connId === 1n, 10000, 'credit')
  assert.equal(credit.body.bytes, 18)
  const dataA = await ca.next((f) => f.type === TYPE.DATA && f.connId === idA, 10000, 'data a')
  assert.equal(b4a.toString(dataA.body.payload), 'ping over hyperdht')
  ca.send({ type: TYPE.CREDIT, connId: idA, body: { bytes: 18 } })

  // a's worker window towards its client is 64 KiB: send 3 x 40 KiB from b, a's client must
  // receive the second message only after returning credit for the first.
  for (let i = 0; i < 3; i++) {
    cb.send({ type: TYPE.DATA, connId: 1n, body: { payload: b4a.alloc(40000, i + 1) } })
  }
  const m1 = await ca.next((f) => f.type === TYPE.DATA && f.connId === idA, 10000, 'm1')
  assert.equal(m1.body.payload.byteLength, 40000)
  await new Promise((r) => setTimeout(r, 500))
  assert.equal(ca.frames.filter((f) => f.type === TYPE.DATA).length, 0, 'window must block m2')
  ca.send({ type: TYPE.CREDIT, connId: idA, body: { bytes: 40000 } })
  const m2 = await ca.next((f) => f.type === TYPE.DATA && f.connId === idA, 10000, 'm2')
  assert.equal(m2.body.payload[0], 2)
  ca.send({ type: TYPE.CREDIT, connId: idA, body: { bytes: 40000 } })
  const m3 = await ca.next((f) => f.type === TYPE.DATA && f.connId === idA, 10000, 'm3')
  assert.equal(m3.body.payload[0], 3)

  // oversized message (> whole window) passes once the window is full again (all credited)
  ca.send({ type: TYPE.CREDIT, connId: idA, body: { bytes: 40000 } })
  cb.send({ type: TYPE.DATA, connId: 1n, body: { payload: b4a.alloc(100000, 9) } })
  const big = await ca.next((f) => f.type === TYPE.DATA && f.connId === idA, 10000, 'big')
  assert.equal(big.body.payload.byteLength, 100000)

  // a -> b
  ca.send({ type: TYPE.DATA, connId: idA, body: { payload: b4a.from('pong') } })
  const dataB = await cb.next((f) => f.type === TYPE.DATA && f.connId === 1n, 10000, 'data b')
  assert.equal(b4a.toString(dataB.body.payload), 'pong')

  // PeerInfo request
  cb.send({ type: TYPE.PEER_INFO, connId: 1n, body: { request: true } })
  const pi = await cb.next((f) => f.type === TYPE.PEER_INFO && f.connId === 1n, 5000, 'peer info')
  assert.ok(pi.body.bytesOut > 0n)

  // two-way close initiated by b's client
  cb.send({ type: TYPE.CLOSE, connId: 1n, body: { reason: CLOSE_REASON.NORMAL, code: 0, message: '' } })
  await cb.next((f) => f.type === TYPE.CLOSE && f.connId === 1n, 5000, 'close echo')
  const closeA = await ca.next((f) => f.type === TYPE.CLOSE && f.connId === idA, 10000, 'close a')
  assert.ok([CLOSE_REASON.NORMAL, CLOSE_REASON.ERROR].includes(closeA.body.reason))
  ca.send({ type: TYPE.CLOSE, connId: idA, body: { reason: 0, code: 0, message: '' } })

  // data for a released id is answered with Error(UNKNOWN_CONN) once
  cb.send({ type: TYPE.DATA, connId: 1n, body: { payload: b4a.from('late') } })
  const err = await cb.next((f) => f.type === TYPE.ERROR, 5000, 'unknown conn')
  assert.equal(err.body.code, 3)

  // open to a key nobody listens on fails with Close
  cb.send({
    type: TYPE.OPEN,
    connId: 2n,
    body: { requestId: 4, remotePublicKey: crypto.randomBytes(32), topic: b4a.alloc(32), openFlags: 0, tlvs: [] }
  })
  const fail = await cb.next((f) => f.type === TYPE.CLOSE && f.connId === 2n, 40000, 'open failure')
  assert.notEqual(fail.body.reason, CLOSE_REASON.NORMAL)
  cb.send({ type: TYPE.CLOSE, connId: 2n, body: { reason: 0, code: 0, message: '' } })
})

test('unknown non-ignorable frame closes the bridge with UNKNOWN_TYPE', async (t) => {
  const w = spawnWorker([{ host: '127.0.0.1', port: 1 }])
  t.after(() => w.ctx.swarm.destroy())
  const c = new Client(w.stream)
  const closed = new Promise((resolve) => c.socket.on('close', resolve))
  await c.next((f) => f.type === TYPE.HELLO, 5000, 'hello')
  c.send({ type: 0x55, flags: FLAG.IGNORABLE, body: { raw: b4a.from([1]) } })
  c.send({ type: 0x56, body: { raw: b4a.from([1]) } })
  const err = await c.next((f) => f.type === TYPE.ERROR, 5000, 'error')
  assert.equal(err.body.code, 1)
  await closed
})

test('--port-fixed: the shared DHT binds exactly that port; a second worker on it reports FAILED', async (t) => {
  const testnet = await createTestnet(3, { host: '127.0.0.1' })
  const port = await freeUdpPort()
  const a = spawnWorker(testnet.bootstrap, ['--port-fixed', String(port)])
  const ca = new Client(a.stream)
  let b = null
  let cb = null
  t.after(async () => {
    await Promise.allSettled([a, b].filter(Boolean).map((w) => w.ctx.swarm.destroy()))
    ca.close()
    if (cb) cb.close()
    await testnet.destroy()
  })
  await ca.next((f) => f.type === TYPE.HELLO, 5000, 'hello a')
  const ready = await ca.next(
    (f) => f.type === TYPE.DHT_STATUS && f.body.state === DHT_STATE.READY, 20000, 'dht ready'
  )
  assert.equal(ready.flags & FLAG.IGNORABLE, FLAG.IGNORABLE)
  assert.equal(ready.body.udpPort, port)
  assert.equal(a.ctx.swarm.status().udpPort, port)

  b = spawnWorker(testnet.bootstrap, ['--port-fixed', String(port)])
  cb = new Client(b.stream)
  await cb.next((f) => f.type === TYPE.HELLO, 5000, 'hello b')
  const failed = await cb.next(
    (f) => f.type === TYPE.DHT_STATUS && f.body.state === DHT_STATE.FAILED, 20000, 'dht failed'
  )
  assert.equal(failed.body.udpPort, 0, 'no other port instead of the fixed one')
})

test('one DHT, two swarms: a plain topic and a Hugin identity announce and are dialed through one worker', async (t) => {
  const testnet = await createTestnet(3, { host: '127.0.0.1' })
  const port = await freeUdpPort()
  const a = spawnWorker(testnet.bootstrap, ['--port-fixed', String(port)])
  const b = spawnWorker(testnet.bootstrap)
  const ca = new Client(a.stream)
  const cb = new Client(b.stream)
  t.after(async () => {
    await Promise.allSettled([a, b].map((w) => w.ctx.swarm.destroy()))
    ca.close()
    cb.close()
    await testnet.destroy()
  })
  const helloA = await ca.next((f) => f.type === TYPE.HELLO, 5000, 'hello a')
  const nodeKey = findTlv(helloA.body.tlvs, TLV.PUBLIC_KEY)
  await cb.next((f) => f.type === TYPE.HELLO, 5000, 'hello b')

  const plainTopic = crypto.randomBytes(32)
  const topicKey = crypto.randomBytes(32)
  const hTopic = huginTopic(keyPairFromSeed(topicKey).publicKey)
  const huginTlvs = [{ tag: TLV.HUGIN_TOPIC_KEY, value: topicKey }]

  ca.send({ type: TYPE.ANNOUNCE, body: { requestId: 1, topic: plainTopic, mode: 1, tlvs: [] } })
  ca.send({ type: TYPE.ANNOUNCE, body: { requestId: 2, topic: hTopic, mode: 1, tlvs: huginTlvs } })
  const ackPlain = await ca.next((f) => f.type === TYPE.ACK && f.body.requestId === 1, 20000, 'ack plain')
  const ackHugin = await ca.next((f) => f.type === TYPE.ACK && f.body.requestId === 2, 20000, 'ack hugin')
  const plainKey = findTlv(ackPlain.body.tlvs, TLV.PUBLIC_KEY)
  const huginKey = findTlv(ackHugin.body.tlvs, TLV.PUBLIC_KEY)
  assert.ok(b4a.equals(plainKey, nodeKey), 'the plain topic uses the node key')
  assert.ok(!b4a.equals(huginKey, nodeKey), 'the Hugin topic has its own identity')

  // Both swarms run on the worker's one DHT node, and that node holds the fixed port.
  const sm = a.ctx.swarm
  assert.equal(sm.identities.size, 1)
  for (const swarm of sm._swarms()) assert.equal(swarm.dht, sm.dht)
  assert.equal(sm.status().udpPort, port)

  cb.send({ type: TYPE.LOOKUP, body: { requestId: 3, topic: plainTopic, maxPeers: 10, tlvs: [] } })
  cb.send({ type: TYPE.LOOKUP, body: { requestId: 4, topic: hTopic, maxPeers: 10, tlvs: [] } })
  const lrPlain = await cb.next((f) => f.type === TYPE.LOOKUP_RESULT && f.body.requestId === 3, 20000, 'lookup plain')
  const lrHugin = await cb.next((f) => f.type === TYPE.LOOKUP_RESULT && f.body.requestId === 4, 20000, 'lookup hugin')
  assert.ok(lrPlain.body.peers.some((p) => b4a.equals(p.publicKey, plainKey)))
  assert.ok(lrHugin.body.peers.some((p) => b4a.equals(p.publicKey, huginKey)))

  // b dials both: the plain key without mid, the Hugin identity with its mid.
  cb.send({
    type: TYPE.OPEN,
    connId: 1n,
    body: { requestId: 5, remotePublicKey: plainKey, topic: plainTopic, openFlags: 0, tlvs: [] }
  })
  cb.send({
    type: TYPE.OPEN,
    connId: 2n,
    body: { requestId: 6, remotePublicKey: huginKey, topic: hTopic, openFlags: 0, tlvs: huginTlvs }
  })
  await cb.next((f) => f.type === TYPE.OPENED && f.connId === 1n, 20000, 'opened plain')
  await cb.next((f) => f.type === TYPE.OPENED && f.connId === 2n, 20000, 'opened hugin')
  const inbound = []
  inbound.push(await ca.next((f) => f.type === TYPE.OPENED && f.body.requestId === 0, 20000, 'inbound 1'))
  inbound.push(await ca.next((f) => f.type === TYPE.OPENED && f.body.requestId === 0, 20000, 'inbound 2'))
  const locals = inbound.map((f) => findTlv(f.body.tlvs, TLV.PUBLIC_KEY))
  assert.ok(locals.some((k) => b4a.equals(k, plainKey)), 'accepted on the plain swarm')
  assert.ok(locals.some((k) => b4a.equals(k, huginKey)), 'accepted on the Hugin identity')
  const huginInbound = inbound.find((f) => b4a.equals(findTlv(f.body.tlvs, TLV.PUBLIC_KEY), huginKey))
  assert.ok(b4a.equals(huginInbound.body.topic, hTopic), 'the Hugin identity names its topic')
})
