// Optional separation test against the real Hugin forks (hyperswarm-hugin / hyperdht-hugin) and
// keypear, as used by hugin-node 2.x and Hugin clients before hugin-network 3.0. Since the
// domain-separated topic and mid derivation (src/mid.js) those clients must NOT be able to talk
// to us, in either direction: their topic differs, and each side refuses the other's mid. The
// forks are not dependencies of the bridge, so the test only runs when their install locations
// are given:
//
//   HUGIN_FORKS_NODE_MODULES=<dir containing hyperswarm-hugin and hyperdht-hugin> \
//   KEYPEAR_DIR=<dir of the keypear package> \
//   node --test test/hugin-interop.test.js

import test from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import path from 'node:path'
import crypto from 'node:crypto'
import createTestnet from 'hyperdht/testnet.js'
import { SwarmManager } from '../src/swarm.js'
import { huginTopicFromKey, keyPairFromSeed } from '../src/mid.js'

const FORKS = process.env.HUGIN_FORKS_NODE_MODULES
const KEYPEAR = process.env.KEYPEAR_DIR
const skip = !FORKS || !KEYPEAR ? 'set HUGIN_FORKS_NODE_MODULES and KEYPEAR_DIR' : false

const NETWORK_ADDRESS = 'xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337'

function loadForks() {
  const require = createRequire(import.meta.url)
  return {
    Hyperswarm: require(path.join(FORKS, 'hyperswarm-hugin')),
    DHT: require(path.join(FORKS, 'hyperdht-hugin')),
    Keychain: require(KEYPEAR)
  }
}

// hugin-node utils.get_new_peer_keys(key)
function huginPeerKeys({ DHT, Keychain }, key) {
  const base = Keychain.from(DHT.keyPair(Buffer.alloc(32).fill(key)))
  const dhtKeys = Keychain.from(DHT.keyPair(crypto.randomBytes(32)))
  const sig = base.get().sign(dhtKeys.get().publicKey)
  return { base, dhtKeys, sig, topic: Buffer.alloc(32).fill(base.publicKey.toString('hex')) }
}

async function setup(t) {
  const forks = loadForks()
  const testnet = await createTestnet(3, { host: '127.0.0.1' })
  const k = huginPeerKeys(forks, NETWORK_ADDRESS)
  const fork = new forks.Hyperswarm({ bootstrap: testnet.bootstrap }, k.sig, k.dhtKeys, k.base)
  const events = []
  fork.on('connection', (conn) => {
    conn.on('error', () => {})
    conn.on('data', (d) => events.push(['fork', d.toString()]))
    conn.write(Buffer.from('hello from hugin'))
  })
  const ours = new SwarmManager({
    keyPair: keyPairFromSeed(crypto.randomBytes(32)),
    bootstrap: testnet.bootstrap,
    onconnection: (socket) => {
      socket.on('error', () => {})
      socket.on('data', (d) => events.push(['ours', d.toString()]))
      socket.write(Buffer.from('hello from xkr'))
    }
  })
  t.after(async () => {
    await ours.destroy()
    await fork.destroy()
    await testnet.destroy()
  })
  return { fork, ours, events, k }
}

async function waitFor(fn, ms = 15000) {
  const start = Date.now()
  while (!fn()) {
    if (Date.now() - start > ms) return false
    await new Promise((r) => setTimeout(r, 50))
  }
  return true
}

const exchanged = (events) =>
  events.some((e) => e[0] === 'fork' && e[1] === 'hello from xkr') ||
  events.some((e) => e[0] === 'ours' && e[1] === 'hello from hugin')

test('the earlier hyperswarm-hugin topic is not ours', { skip }, async (t) => {
  const { k } = await setup(t)
  const h = huginTopicFromKey(NETWORK_ADDRESS)
  assert.ok(Buffer.from(h.base.publicKey).equals(k.base.publicKey), 'same topic base key')
  assert.ok(!Buffer.from(h.topic).equals(k.topic), 'different DHT topic')
})

test('a hyperswarm-hugin peer dialing us on our topic is refused by us', { skip }, async (t) => {
  const { fork, ours, events } = await setup(t)
  const h = huginTopicFromKey(NETWORK_ADDRESS)
  await ours.announce(h.topic, 1, { topicKey: h.topicKey })
  await fork.join(h.topic, { server: false, client: true }).flushed()
  assert.equal(await waitFor(() => exchanged(events), 8000), false, 'data exchanged with an old client')
  assert.ok(
    await waitFor(() => [...ours.registry.rejections.values()].some((r) => r.reason === 'invalid mid'), 8000),
    'our side did not reject the old mid'
  )
})

test('hyperswarm-hugin refuses our mid when we dial it', { skip }, async (t) => {
  const { fork, ours, events } = await setup(t)
  const h = huginTopicFromKey(NETWORK_ADDRESS)
  await fork.join(h.topic, { server: true, client: false }).flushed()
  await ours.announce(h.topic, 2, { topicKey: h.topicKey })
  assert.equal(await waitFor(() => exchanged(events), 8000), false, 'data exchanged with an old client')
})

test('an old client on its own topic never meets us', { skip }, async (t) => {
  const { fork, ours, events, k } = await setup(t)
  const h = huginTopicFromKey(NETWORK_ADDRESS)
  await ours.announce(h.topic, 3, { topicKey: h.topicKey })
  await fork.join(k.topic, { server: true, client: true }).flushed()
  assert.equal(await waitFor(() => exchanged(events), 8000), false, 'data exchanged with an old client')
})
