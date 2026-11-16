#!/usr/bin/env node
// Generates docs/test-vectors/hugin-relay.json with the encoders hugin-node and the Hugin apps
// use themselves: bare-rpc (wire frames), hugin-utils (nonce offset, prev_id, target check,
// verifyShare), kryptokrona-utils (cn_fast_hash, CN-Turtle-Lite v2) and keypear (pow.auth
// signatures). crates/xkr-hugin checks its codec and PoW code against the file byte for byte.
//
// The packages are not dependencies of the bridge; point HUGIN_NODE_MODULES at a node_modules
// that has bare-rpc, hugin-utils, kryptokrona-utils and keypear (hugin-desktop's has all four):
//
//   HUGIN_NODE_MODULES=<hugin-desktop checkout>/node_modules \
//     node test/hugin-vectors.js --write
//
// `--write` replaces docs/test-vectors/hugin-relay.json in place; without it the JSON goes to
// stdout. (Do not redirect stdout onto the file itself: the shell truncates it before the
// `hugin_derivation` section below can be read and carried over.)
//
// Takes a few seconds (kryptokrona-utils hashes CN-Turtle in JavaScript).
//
// The `hugin_derivation` section (topic and mid derivation of hugin-network 3.0) is written by
// `XKR_WRITE_VECTORS=1 cargo test -p xkr-p2p-hyper --test hugin_derivation_vectors`, not here.
// This script carries it over from the current file unchanged (as the last key, where the Rust
// writer expects it) after checking it against kryptokrona-utils' cn_fast_hash and libsodium;
// regenerate it with the Rust test first if that check fails.

import { createRequire } from 'node:module'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'

const VECTORS_FILE = new URL('../../docs/test-vectors/hugin-relay.json', import.meta.url)
const NM = process.env.HUGIN_NODE_MODULES
if (!NM) {
  process.stderr.write('set HUGIN_NODE_MODULES (see the header of this file)\n')
  process.exit(2)
}
const require = createRequire(import.meta.url)
const load = (name) => require(path.join(NM, name))

const RPC = load('bare-rpc')
const hu = load('hugin-utils')
const { Crypto } = load('kryptokrona-utils')
const Keychain = load('keypear')
const sodium = load('sodium-universal')
const crypto = new Crypto()

const hex = (b) => Buffer.from(b).toString('hex')

// ---------------------------------------------------------------------------------------------
// bare-rpc frames, captured from a real RPC instance over an in-memory stream

function captureStream() {
  const handlers = {}
  const out = []
  const stream = {
    on(ev, fn) {
      handlers[ev] = fn
      return stream
    },
    write(buf) {
      out.push(Buffer.from(buf))
      return true
    },
    destroy(err) {
      out.push(Buffer.from(`DESTROYED ${err && err.message}`))
    }
  }
  return { stream, out, feed: (buf) => handlers.data(buf) }
}

const tick = () => new Promise((r) => setTimeout(r, 0))

async function rpcVectors() {
  const vectors = []
  const push = (name, out, extra = {}) => {
    vectors.push({ name, hex: hex(Buffer.concat(out)), ...extra })
    out.length = 0
  }

  // Outgoing requests: rpc.request(PACKET).send(JSON) exactly like hugin-node's rpc_send().
  {
    const { stream, out } = captureStream()
    const rpc = new RPC(stream, () => {})
    rpc.request(1).send(JSON.stringify({ version: '2.0.0' }))
    push('request_version', out, { id: 1, command: 1, data: JSON.stringify({ version: '2.0.0' }) })
    const big = JSON.stringify({ response: [], id: 'x'.repeat(300) })
    rpc.request(1).send(big)
    push('request_len_3_byte', out, { id: 2, command: 1, data: big })
    // jump the id counter to exercise 3- and 5-byte compact uints
    rpc._id = 299
    rpc.request(1).send('{}')
    push('request_id_3_byte', out, { id: 300, command: 1, data: '{}' })
    rpc._id = 69999
    rpc.request(1).send('{}')
    push('request_id_5_byte', out, { id: 70000, command: 1, data: '{}' })
    rpc.event(1).send('{"a":1}')
    push('event', out, { id: 0, command: 1, data: '{"a":1}' })
  }

  // Incoming request -> reply, as hugin-node's setup_rpc() and the apps do.
  {
    const { stream, out, feed } = captureStream()
    const reqs = []
    // eslint-disable-next-line no-new
    new RPC(stream, (req) => {
      reqs.push(req)
      if (b2s(req.data) === 'boom') {
        throw Object.assign(new Error('boom'), { code: 'E_BOOM', errno: -3 })
      }
      req.reply(JSON.stringify({ ok: true, data: null, error: null }))
    })
    // build an incoming request frame with another RPC
    const peer = captureStream()
    const prpc = new RPC(peer.stream, () => {})
    prpc._id = 4
    prpc.request(1).send('{"request":true,"type":"some","timestamp":0}')
    const reqFrame = Buffer.concat(peer.out)
    peer.out.length = 0
    feed(reqFrame)
    await tick()
    push('response_ok', out, {
      id: 5,
      data: JSON.stringify({ ok: true, data: null, error: null }),
      request_hex: hex(reqFrame)
    })
    prpc._id = 6
    prpc.request(1).send('boom')
    const boom = Buffer.concat(peer.out)
    peer.out.length = 0
    feed(boom)
    await tick()
    push('response_error', out, { id: 7, message: 'boom', code: 'E_BOOM', errno: -3 })

    // two frames in one chunk and one frame split in three chunks decode the same way
    prpc._id = 7
    prpc.request(1).send('{"x":1}')
    prpc.request(1).send('{"x":2}')
    const two = Buffer.concat(peer.out)
    peer.out.length = 0
    feed(two.subarray(0, 3))
    feed(two.subarray(3, 20))
    feed(two.subarray(20))
    await tick()
    vectors.push({
      name: 'two_requests_split',
      hex: hex(two),
      ids: reqs.slice(-2).map((r) => r.id),
      data: reqs.slice(-2).map((r) => b2s(r.data))
    })
    out.length = 0
  }
  return vectors
}

function b2s(b) {
  return b ? Buffer.from(b).toString() : null
}

// ---------------------------------------------------------------------------------------------
// proof of work (hugin-utils + kryptokrona-utils, the code path of hugin-node's verifyShare)

function varint(n) {
  const out = []
  let v = BigInt(n)
  while (v >= 0x80n) {
    out.push(Number((v & 0x7fn) | 0x80n))
    v >>= 7n
  }
  out.push(Number(v))
  return Buffer.from(out)
}

async function powVectors() {
  const prev = Buffer.alloc(32)
  for (let i = 0; i < 32; i++) prev[i] = (i * 7 + 3) & 0xff
  const root = Buffer.alloc(32, 0x5a)
  // major 5, minor 0, timestamp, prev_id, nonce, merkle root, tx count
  const blob = Buffer.concat([
    varint(5),
    varint(0),
    varint(1731500000),
    prev,
    Buffer.alloc(4),
    root,
    varint(1)
  ])
  const job = { job_id: 'job-1', blob: hex(blob), target: 'ffffffff' }
  const nonce = 'a1b2c3d4'
  const { blobHex, offset } = hu.insertNonce(job.blob, nonce)
  const result = await crypto.cn_turtle_lite_slow_hash_v2(blobHex)
  const verified = await hu.verifyShare(job, nonce, result)
  const wrong = await hu.verifyShare(job, 'a1b2c3d5', result)

  // A blob whose timestamp varint is 1 byte: offset differs from the usual 39.
  const short = Buffer.concat([varint(4), varint(2), varint(7), prev, Buffer.alloc(4)])

  const targets = ['ffffffff', '00000080', '01000000', 'e8030000', '10270000', '00000000', 'ffff0000']
  const tails = [
    '0000000000000000',
    'ffffffffffffffff',
    '0100000001000000',
    '0000000001000000',
    'ffffffff00000000',
    '6b4a0500a0410600',
    '418312b5ae4a1000',
    '1b4f1e1c78690000'
  ]
  const meets = []
  for (const target of targets) {
    for (const tail of tails) {
      const hashHex = '33'.repeat(24) + tail
      meets.push({ hash: hashHex, target, meets: hu.meetsTarget(hashHex, target) })
    }
  }

  return {
    job,
    nonce,
    nonce_offset: offset,
    hashing_blob: blobHex,
    result,
    verify_share: verified,
    verify_share_wrong_nonce: wrong,
    prev_id: hu.extractPrevIdFromBlob(job.blob),
    short_blob: hex(short),
    short_nonce_offset: hu.getNonceOffset(hex(short)),
    short_prev_id: hu.extractPrevIdFromBlob(hex(short)),
    bad_prev_id: hu.extractPrevIdFromBlob('0500ff'),
    meets_target: meets
  }
}

// ---------------------------------------------------------------------------------------------
// pow.auth (the apps' build_pow_auth, hugin-node's verify_pow_auth)

function keyPair(seed) {
  const publicKey = Buffer.alloc(32)
  const secretKey = Buffer.alloc(64)
  sodium.crypto_sign_seed_keypair(publicKey, secretKey, seed)
  return { publicKey, secretKey }
}

function authVectors(share) {
  const signer = Keychain.from(keyPair(Buffer.alloc(32, 7))).get()
  const out = []
  const cases = [
    { hash: 'ab'.repeat(32), timestamp: 1731500000000, context: 'dm:' + 'cd'.repeat(40) },
    { hash: 'ef'.repeat(32), timestamp: 1731500000123, context: 'c0ffee' },
    { hash: '01'.repeat(32), timestamp: 42, context: '' }
  ]
  for (const c of cases) {
    const payload = `powsig:v2:${c.hash}:${c.timestamp}:${share.job_id}:${share.nonce.toLowerCase()}:${share.result.toLowerCase()}:${c.context}`
    const sig = signer.sign(Buffer.from(payload))
    out.push({
      ...c,
      payload,
      pub: hex(signer.publicKey),
      sig: hex(sig),
      verifies: Keychain.verify(Buffer.from(payload), sig, signer.publicKey)
    })
  }
  return out
}

// ---------------------------------------------------------------------------------------------
// node identity (hugin-node: create_keys_from_seed(await hash(NodeId.viewkey)))

async function identityVectors() {
  const viewkey = '0f'.repeat(32)
  const toHex = (s) => [...s].map((ch) => ch.charCodeAt(0).toString(16)).join('')
  const hashed = await crypto.cn_fast_hash(toHex(viewkey))
  const seed = Buffer.alloc(32).fill(hashed)
  return { viewkey, hashed, dht_seed: hex(seed), dht_public_key: hex(keyPair(seed).publicKey) }
}

// ---------------------------------------------------------------------------------------------
// hugin_derivation (owned by crates/xkr-p2p-hyper/tests/hugin_derivation_vectors.rs)

async function carriedDerivation() {
  let v = null
  try {
    v = JSON.parse(fs.readFileSync(VECTORS_FILE, 'utf8')).hugin_derivation || null
  } catch {
    v = null
  }
  if (!v) {
    process.stderr.write('no hugin_derivation section to carry over (write it with the Rust test)\n')
    return null
  }
  const dht = keyPair(Buffer.from(v.dht.seed, 'hex'))
  if (hex(dht.publicKey) !== v.dht.public_key) throw new Error('hugin_derivation: dht key differs')
  for (const t of v.topics) {
    const base = keyPair(Buffer.alloc(32).fill(t.key))
    if (hex(base.publicKey) !== t.base_public_key) throw new Error(`hugin_derivation ${t.name}: base key differs`)
    const topicInput = Buffer.concat([Buffer.from(v.topic_domain), base.publicKey])
    const topic = await crypto.cn_fast_hash(hex(topicInput))
    if (topic !== t.topic) throw new Error(`hugin_derivation ${t.name}: topic differs from kryptokrona-utils`)
    const message = Buffer.concat([Buffer.from(v.mid_domain), dht.publicKey])
    if (hex(message) !== t.mid_message) throw new Error(`hugin_derivation ${t.name}: mid message differs`)
    const mid = Buffer.alloc(64)
    sodium.crypto_sign_detached(mid, message, base.secretKey)
    if (hex(mid) !== t.mid) throw new Error(`hugin_derivation ${t.name}: mid differs from libsodium`)
  }
  return v
}

const rpc = await rpcVectors()
const pow = await powVectors()
const auth = authVectors({ job_id: pow.job.job_id, nonce: pow.nonce, result: pow.result })
const identity = await identityVectors()
const derivation = await carriedDerivation()

const output =
  JSON.stringify(
    {
      description:
        'Hugin relay vectors produced by bridge/test/hugin-vectors.js with bare-rpc, hugin-utils, kryptokrona-utils and keypear (the packages hugin-node uses); checked by crates/xkr-hugin.',
      rpc,
      pow,
      auth,
      identity,
      ...(derivation ? { hugin_derivation: derivation } : {})
    },
    null,
    2
  ) + '\n'
if (process.argv.includes('--write')) fs.writeFileSync(VECTORS_FILE, output)
else process.stdout.write(output)
