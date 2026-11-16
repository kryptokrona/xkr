#!/usr/bin/env node
// A Hugin app talking to a hugin-node, following the steps of hugin-desktop's NodeConnection
// and hugin-native's, with the topic and mid derivation of hugin-network 3.0 (domain-separated, the
// only one an xkrd relay accepts; clients using the earlier hyperswarm-hugin derivation cannot
// reach it). The swarm side is the bridge's own mid layer (src/mid.js, src/swarm.js), which
// implements that derivation:
//
//   1. join the public-node topic as a client (or, with --invite, dial the private node's DHT
//      key on its topic), mid in the handshake; the first connection is used;
//   2. wrap the connection in bare-rpc, expect { address, version };
//   3. { type: 'job_request' } -> { type: 'job', job } (a p2pool job; `job_pending` first while
//      the relay has none);
//   4. find a share with hugin-utils + kryptokrona-utils (CN-Turtle-Lite v2) in worker threads,
//      so the connection keeps answering while the search runs (as in the apps, whose search
//      runs off the event loop; kryptokrona-utils hashes synchronously, in plain JS when its
//      native addon is built for Electron), sign pow.auth with a one-time key
//      (build_pow_auth), send { type: 'post', message } -> { success: true };
//   5. { request: true, type: 'some', timestamp: 0 } -> the message (without pow.job);
//   6. wait for the { type: 'new-message' } push with the message.
//
// With --expect-hash <hash> it only connects and syncs, and passes if that message is in the
// node's pool (used to check gossip between relays).
//
// Prints one line per step and PASS (exit 0) or FAIL <reason> (exit 1). Uses only the DHT given
// with --bootstrap (a local testnet); never the public one unless --public-dht is passed.
// --testnet joins the testnet topics (the domain `hugin/testnet/topic`, as a testnet xkrd).
//
// Env:
//   HUGIN_NODE_MODULES        node_modules with bare-rpc, keypear, hugin-utils, kryptokrona-utils
//                             (hugin-desktop's has all of them)
//   KRYPTOKRONA_CRYPTO        optional path of kryptokrona-crypto 2.x; when set, shares are hashed
//                             with it (native) instead of kryptokrona-utils
//   (HUGIN_FORKS_NODE_MODULES is no longer used: the old forks cannot reach the relay.)
// usage:
//   node bridge/test/hugin-client-sim.js --bootstrap 127.0.0.1:<port> [--invite <address+key>]
//        [--timeout <s>] [--expect-address <SEKR...>] [--expect-hash <hash>] [--testnet]

import { createRequire } from 'node:module'
import path from 'node:path'
import process from 'node:process'
import crypto from 'node:crypto'
import { Worker } from 'node:worker_threads'
import { SwarmManager } from '../src/swarm.js'
import { huginTopicFromKey, keyPairFromSeed } from '../src/mid.js'

const NETWORK_ADDRESS = 'xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337'
const PACKET = 1
/** Worker threads searching disjoint nonce ranges. */
const POW_WORKERS = 3
/** Most hashes per second each worker is allowed (hugin-utils findShare's attempt cap). */
const POW_HASHES_PER_SECOND = 1000
let done = false

const opts = {
  bootstrap: null,
  invite: null,
  timeout: 120,
  publicDht: false,
  expectAddress: null,
  expectHash: null,
  testnet: false
}
const argv = process.argv.slice(2)
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === '--bootstrap') opts.bootstrap = argv[++i]
  else if (argv[i] === '--invite') opts.invite = argv[++i]
  else if (argv[i] === '--timeout') opts.timeout = Number(argv[++i])
  else if (argv[i] === '--public-dht') opts.publicDht = true
  else if (argv[i] === '--expect-address') opts.expectAddress = argv[++i]
  else if (argv[i] === '--expect-hash') opts.expectHash = argv[++i]
  else if (argv[i] === '--testnet') opts.testnet = true
}

function fail(reason) {
  process.stdout.write(`FAIL ${reason}\n`)
  process.exit(1)
}

const NM = process.env.HUGIN_NODE_MODULES
if (!NM) fail('set HUGIN_NODE_MODULES')
if (!opts.bootstrap && !opts.publicDht) fail('--bootstrap host:port required (or --public-dht)')

const require = createRequire(import.meta.url)
const RPC = require(path.join(NM, 'bare-rpc'))
const Keychain = require(path.join(NM, 'keypear'))

const log = (step, ...rest) => process.stdout.write(`${step} ${rest.join(' ')}\n`)
const randomKey = () => crypto.randomBytes(32).toString('hex')

// hugin-desktop crypto.cjs get_new_peer_keys(key), for the one-time pow.auth key
function getNewPeerKeys(key) {
  const base = Keychain.from(keyPairFromSeed(Buffer.alloc(32).fill(key)))
  const dhtKeys = Keychain.from(keyPairFromSeed(Buffer.alloc(32).fill(randomKey())))
  return [base, dhtKeys]
}

// hugin-desktop NodeConnection.build_pow_auth
function buildPowAuth(messageHash, timestamp, shares, context) {
  const share = shares[0]
  const [, oneTime] = getNewPeerKeys(randomKey())
  const signer = oneTime.get()
  const payload = `powsig:v2:${String(messageHash)}:${timestamp}:${String(share.job_id)}:${String(share.nonce).toLowerCase()}:${String(share.result).toLowerCase()}:${String(context)}`
  return {
    pub: Buffer.from(signer.publicKey).toString('hex'),
    sig: Buffer.from(signer.sign(Buffer.from(payload))).toString('hex'),
    nonce: String(share.nonce).toLowerCase()
  }
}

// One share search in a worker: hugin-utils findShare over CN-Turtle-Lite v2.
const POW_WORKER_SOURCE = `
const { parentPort, workerData } = require('node:worker_threads')
const hu = require(workerData.huginUtils)
const mod = require(workerData.kryptokronaCrypto || workerData.kryptokronaUtils)
const xkr = new (mod.Crypto || mod.default)()
hu.findShare({
  job: workerData.job,
  startNonce: workerData.startNonce,
  hashFn: (blobHex) => xkr.cn_turtle_lite_slow_hash_v2(blobHex),
  hashesPerSecond: workerData.hashesPerSecond,
  timeBudgetMs: workerData.timeBudgetMs
}).then(
  (share) => parentPort.postMessage({ share }),
  (e) => parentPort.postMessage({ error: String((e && e.message) || e) })
)
`

// The first share any of POW_WORKERS workers finds for `job` within `timeBudgetMs`, or null.
function searchShare(job, timeBudgetMs) {
  const stride = Math.floor(0x100000000 / POW_WORKERS)
  const base = crypto.randomBytes(4).readUInt32LE(0)
  const workers = []
  return new Promise((resolve, reject) => {
    let left = POW_WORKERS
    let settled = false
    const settle = (fn, v) => {
      if (settled) return
      settled = true
      for (const w of workers) w.terminate().catch(() => {})
      fn(v)
    }
    for (let i = 0; i < POW_WORKERS; i++) {
      const w = new Worker(POW_WORKER_SOURCE, {
        eval: true,
        workerData: {
          huginUtils: path.join(NM, 'hugin-utils'),
          kryptokronaUtils: path.join(NM, 'kryptokrona-utils'),
          kryptokronaCrypto: process.env.KRYPTOKRONA_CRYPTO || '',
          job,
          startNonce: (base + i * stride) >>> 0,
          hashesPerSecond: POW_HASHES_PER_SECOND,
          timeBudgetMs
        }
      })
      workers.push(w)
      w.once('message', (m) => {
        if (m.error) settle(reject, new Error(`share search: ${m.error}`))
        else if (m.share) settle(resolve, m.share)
        else if (--left === 0) settle(resolve, null)
      })
      w.once('error', (e) => settle(reject, e))
    }
  })
}

const deadline = setTimeout(() => fail(`timeout after ${opts.timeout}s`), opts.timeout * 1000)

const pub = !opts.invite
const key = pub ? NETWORK_ADDRESS : opts.invite
const h = huginTopicFromKey(key, opts.testnet ? 'testnet' : 'mainnet')
let connected = false
const swarm = new SwarmManager({
  keyPair: keyPairFromSeed(crypto.randomBytes(32)),
  bootstrap: opts.bootstrap
    ? [{ host: opts.bootstrap.split(':')[0], port: Number(opts.bootstrap.split(':')[1]) }]
    : null,
  onconnection: (socket, info) => {
    if (connected) {
      socket.destroy()
      return
    }
    connected = true
    log('CONNECTED', Buffer.from(socket.remotePublicKey).toString('hex'))
    run(socket).catch((e) => fail(e && e.stack ? e.stack.split('\n')[0] : String(e)))
  }
})
log('JOIN', pub ? 'public-node topic' : 'private-node topic', Buffer.from(h.topic).toString('hex'))

if (pub) {
  await swarm.announce(h.topic, 2, { topicKey: h.topicKey })
} else {
  // the private node's DHT key is the last 64 characters of the invite (hugin-desktop's firewall)
  const nodeKey = Buffer.from(opts.invite.slice(-64), 'hex')
  const dial = () => {
    if (connected) return
    const { socket } = swarm.open(nodeKey, h.topic, 0, { topicKey: h.topicKey })
    socket.on('error', () => setTimeout(dial, 2000))
    socket.on('open', () => {
      if (connected) {
        socket.destroy()
        return
      }
      connected = true
      log('CONNECTED', nodeKey.toString('hex'))
      run(socket).catch((e) => fail(e && e.stack ? e.stack.split('\n')[0] : String(e)))
    })
  }
  dial()
}

async function run(conn) {
  conn.on('error', () => {})
  conn.on('close', () => {
    if (!done) fail('connection closed by the node')
  })
  const waiters = []
  const seen = []
  const rpc = new RPC(conn, (req) => {
    let data = null
    try {
      data = JSON.parse(Buffer.from(req.data).toString())
    } catch {
      data = null
    }
    if (data) {
      seen.push(data)
      for (const w of [...waiters]) {
        if (w.pred(data)) {
          waiters.splice(waiters.indexOf(w), 1)
          w.resolve(data)
        }
      }
    }
    req.reply(JSON.stringify({ ok: true, data: null, error: null }))
  })
  const send = (packet) => {
    const req = rpc.request(PACKET)
    req.send(JSON.stringify(packet))
    req.reply().catch(() => {})
  }
  const waitFor = (pred) => {
    const hit = seen.find(pred)
    if (hit) return Promise.resolve(hit)
    return new Promise((resolve) => waiters.push({ pred, resolve }))
  }

  const hello = await waitFor((d) => 'address' in d)
  if (typeof hello.address !== 'string' || hello.address.length !== 99) fail('bad node address')
  if (opts.expectAddress && hello.address !== opts.expectAddress) fail('unexpected node address')
  log('HELLO', hello.address, hello.version)

  if (opts.expectHash) {
    const id = Date.now()
    send({ request: true, type: 'some', timestamp: 0, id })
    const list = await waitFor((d) => d.id === id && Array.isArray(d.response))
    if (!list.response.some((m) => m.hash === opts.expectHash)) fail('expected message not in the pool')
    log('FOUND', opts.expectHash)
    return finish(conn)
  }

  const jobId = randomKey()
  send({ type: 'job_request', id: jobId })
  let jobMsg = await waitFor((d) => d.id === jobId && (d.type === 'job' || d.type === 'job_pending'))
  if (jobMsg.type === 'job_pending') jobMsg = await waitFor((d) => d.type === 'job' && d.job)
  const cipher = crypto.randomBytes(48).toString('hex')
  const job = jobMsg.job
  log('JOB', job.job_id, job.target)

  const searchStart = Date.now()
  const share = await searchShare(job, Math.max(5000, (opts.timeout - 10) * 1000))
  if (!share) fail('no share found within the time budget')
  log('SHARE', share.job_id, share.nonce, share.result, `${Date.now() - searchStart}ms`)

  const timestamp = Date.now()
  const messageHash = randomKey()
  const shares = [{ job_id: String(share.job_id), nonce: share.nonce.toLowerCase(), result: share.result.toLowerCase() }]
  const auth = buildPowAuth(messageHash, timestamp, shares, `dm:${cipher}`)
  send({
    type: 'post',
    message: {
      cipher,
      timestamp,
      hash: messageHash,
      id: timestamp,
      push: true,
      viewtag: 'abc',
      kind: 'dm',
      pow: { version: 2, job, shares, auth }
    }
  })
  const answer = await waitFor((d) => d.id === timestamp && 'success' in d)
  if (answer.success !== true) fail(`post refused: ${JSON.stringify(answer)}`)
  log('POSTED', messageHash)

  const syncId = Date.now() + 1
  send({ request: true, type: 'some', timestamp: 0, id: syncId })
  const list = await waitFor((d) => d.id === syncId && Array.isArray(d.response))
  const mine = list.response.find((m) => m.hash === messageHash)
  if (!mine) fail('message missing from the node pool')
  if (mine.pow && mine.pow.job) fail('pool answer still carries pow.job')
  if (mine.cipher !== cipher) fail('cipher changed')
  log('SYNCED', list.response.length, 'message(s)')

  const pushed = await waitFor(
    (d) => d.type === 'new-message' && Array.isArray(d.messages) && d.messages.some((m) => m.hash === messageHash)
  )
  log('PUSHED', pushed.messages.length, 'message(s)')
  return finish(conn)
}

async function finish(conn) {
  done = true
  clearTimeout(deadline)
  process.stdout.write('PASS\n')
  conn.destroy()
  await swarm.destroy().catch(() => {})
  process.exit(0)
}
