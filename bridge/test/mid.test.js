import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import b4a from 'b4a'
import c from 'compact-encoding'
import sodium from 'sodium-universal'
import createTestnet from 'hyperdht/testnet.js'
import {
  midNoisePayload,
  huginTopicFromKey,
  HUGIN_TESTNET_TOPIC_DOMAIN,
  huginTopic,
  fill32,
  keyPairFromSeed,
  midMessage,
  signMid,
  verifyMid,
  MidRegistry,
  HUGIN_TOPIC_DOMAIN,
  HUGIN_MID_DOMAIN
} from '../src/mid.js'
import { keccak256 } from '../src/keccak.js'
import { SwarmManager } from '../src/swarm.js'

const hex = (b) => b4a.toString(b, 'hex')

/** The mid of the earlier derivation (hyperswarm-hugin): a signature over the bare DHT key. */
function legacyMid(baseSecretKey, dhtPublicKey) {
  const sig = b4a.alloc(64)
  sodium.crypto_sign_detached(sig, dhtPublicKey, baseSecretKey)
  return sig
}

const MID = b4a.from(Array.from({ length: 64 }, (_, i) => i))

// Produced by hyperdht-hugin 1.0.3 lib/messages.js (`c.encode(m.noisePayload, payload)`).
const FORK_VECTORS = [
  {
    payload: {
      error: 0,
      firewall: 1,
      holepunch: null,
      addresses4: [{ host: '192.168.1.10', port: 49737 }],
      addresses6: [],
      udx: { reusableSocket: false, id: 7, seq: 0 },
      secretStream: {},
      relayThrough: null,
      mid: MID
    },
    hex:
      '019a000101c0a8010a49c20100070001' +
      '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f' +
      '202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f'
  },
  {
    payload: {
      error: 0,
      firewall: 1,
      holepunch: null,
      addresses4: [{ host: '192.168.1.10', port: 49737 }],
      addresses6: [],
      udx: { reusableSocket: false, id: 7, seq: 0 },
      secretStream: {},
      relayThrough: null,
      mid: null
    },
    hex: '011a000101c0a8010a49c20100070001'
  },
  {
    payload: {
      error: 0,
      firewall: 0,
      holepunch: {
        id: 3,
        relays: [{ relayAddress: { host: '1.2.3.4', port: 5 }, peerAddress: { host: '5.6.7.8', port: 9 } }]
      },
      addresses4: [],
      addresses6: null,
      udx: { reusableSocket: true, id: 300, seq: 0 },
      secretStream: {},
      relayThrough: null,
      relayAddresses: [{ host: '9.9.9.9', port: 1000 }],
      mid: MID
    },
    hex:
      '01d9000003010102030405000506070809000101fd2c0100010109090909e803' +
      '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f' +
      '202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f'
  }
]

test('noise payload with mid is byte-identical to hyperdht-hugin', () => {
  for (const v of FORK_VECTORS) {
    const enc = c.encode(midNoisePayload, v.payload)
    assert.equal(b4a.toString(enc, 'hex'), v.hex)
    const dec = c.decode(midNoisePayload, enc)
    if (v.payload.mid) assert.ok(b4a.equals(dec.mid, v.payload.mid))
    else assert.equal(dec.mid, null)
    assert.equal(dec.udx.id, v.payload.udx.id)
  }
})

// Topic keys and base public keys computed with hugin-node's own code path (kryptokrona-utils
// cn_fast_hash(toHex(s)), Buffer.alloc(32).fill(key), keypear.from(hyperdht-hugin.keyPair(secret)));
// the domain-separated derivation did not change them. `legacyTopic` is the earlier topic
// Buffer.alloc(32).fill(hex(basePublicKey)) that we must no longer produce.
const TOPIC_VECTORS = [
  {
    name: 'node mesh: hex(cn_fast_hash(networkAddress))',
    key: 'c9b4d847ffa97a1d49c74d8085b9529c191080704e325cef9229717a8f769614',
    seed: '6339623464383437666661393761316434396337346438303835623935323963',
    basePub: 'a341b41cb1f327f229cc1a71596094e1bd05f0256d836ed135b6fe377fb874f3',
    legacyTopic: '6133343162343163623166333237663232396363316137313539363039346531'
  },
  {
    name: 'public nodes: networkAddress',
    key: 'xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337',
    seed: '786b723936633066386133366539353162333939363831643434373932326636',
    basePub: '5f70ed4fd39ca7305ba72377d815dade9083749f3fd9d18b99c618cf81146112',
    legacyTopic: '3566373065643466643339636137333035626137323337376438313564616465'
  },
  {
    name: 'short key repeats like Buffer.fill',
    key: 'abc',
    seed: '6162636162636162636162636162636162636162636162636162636162636162',
    basePub: '204aaed5e86cd99a149c0d51d033261996cdbd67a2becb24dfaa43a4828181e8',
    legacyTopic: '3230346161656435653836636439396131343963306435316430333332363139'
  }
]

test('keccak256 is cn_fast_hash (original Keccak padding)', () => {
  const k = (s) => hex(keccak256(b4a.from(s, 'utf8')))
  // same values as crates/xkr-p2p-hyper/src/topic.rs (xkr-crypto cn_fast_hash)
  assert.equal(k(''), 'c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470')
  assert.equal(k('xkr/mainnet/blocks/v1'), '5a7d447e3718667d6b1b8b997b7007e438e5418ba70f57fbfac201c50750ae5b')
  assert.equal(k('xkr/testnet/blocks/v1'), '094b227a2d677ccd034c1c1547bbe2bcce7bcb41f1c8438947b604dcce609ad2')
  assert.equal(k('xkr/mainnet/stripe/4/1/v1'), '140a9b48574ce42d940df9f4d615647b7b97e8df327d711d3147cdf2f5cee3e0')
  // multi-block input and the 0x81 pad byte at the end of a block run without error
  assert.equal(keccak256(b4a.alloc(135, 1)).byteLength, 32)
  assert.equal(keccak256(b4a.alloc(300, 2)).byteLength, 32)
})

test('hugin domain separators', () => {
  assert.equal(HUGIN_TOPIC_DOMAIN, 'hugin/topic')
  assert.equal(HUGIN_MID_DOMAIN, 'hugin/mid')
  const msg = midMessage(b4a.alloc(32, 0xab))
  assert.equal(msg.byteLength, 41)
  assert.equal(b4a.toString(msg.subarray(0, 9)), 'hugin/mid')
  assert.ok(b4a.equals(msg.subarray(9), b4a.alloc(32, 0xab)))
})

test('hugin topic derivation (domain-separated)', () => {
  for (const v of TOPIC_VECTORS) {
    const t = huginTopicFromKey(v.key)
    assert.equal(hex(t.topicKey), v.seed, v.name)
    assert.equal(hex(t.base.publicKey), v.basePub, v.name)
    const data = b4a.concat([b4a.from('hugin/topic'), b4a.from(v.basePub, 'hex')])
    assert.equal(data.byteLength, 43)
    assert.equal(hex(t.topic), hex(keccak256(data)), v.name)
    assert.ok(b4a.equals(huginTopic(t.base.publicKey), t.topic))
    // the hex-fill topic of the earlier derivation is gone
    assert.equal(hex(fill32(v.basePub)), v.legacyTopic, v.name)
    assert.notEqual(hex(t.topic), v.legacyTopic, v.name)
  }
  assert.equal(hex(fill32('')), '00'.repeat(32))
})

test('hugin testnet topics use their own domain', () => {
  assert.equal(HUGIN_TESTNET_TOPIC_DOMAIN, 'hugin/testnet/topic')
  for (const v of TOPIC_VECTORS) {
    const main = huginTopicFromKey(v.key)
    const t = huginTopicFromKey(v.key, 'testnet')
    assert.ok(b4a.equals(t.topicKey, main.topicKey), v.name)
    assert.ok(b4a.equals(t.base.publicKey, main.base.publicKey), v.name)
    assert.notEqual(hex(t.topic), hex(main.topic), v.name)
    const data = b4a.concat([b4a.from('hugin/testnet/topic'), b4a.from(v.basePub, 'hex')])
    assert.equal(hex(t.topic), hex(keccak256(data)), v.name)
  }
})

// Written by crates/xkr-p2p-hyper/tests/hugin_derivation_vectors.rs (XKR_WRITE_VECTORS=1); checked here
// with libsodium and keccak.js, i.e. independently of the Rust code.
test('hugin derivation vectors in docs/test-vectors/hugin-relay.json', () => {
  const file = new URL('../../docs/test-vectors/hugin-relay.json', import.meta.url)
  const v = JSON.parse(fs.readFileSync(file, 'utf8')).hugin_derivation
  assert.ok(
    v,
    'hugin-relay.json has no hugin_derivation section: run ' +
      'XKR_WRITE_VECTORS=1 cargo test -p xkr-p2p-hyper --test hugin_derivation_vectors'
  )
  assert.equal(v.topic_domain, HUGIN_TOPIC_DOMAIN)
  assert.equal(v.mid_domain, HUGIN_MID_DOMAIN)
  assert.equal(v.dht.seed, '07'.repeat(32))
  const dht = keyPairFromSeed(b4a.alloc(32, 7))
  assert.equal(hex(dht.publicKey), v.dht.public_key)
  assert.deepEqual(v.topics.map((t) => t.name), ['node mesh', 'public nodes', 'private node'])
  assert.equal(v.topics[0].key, TOPIC_VECTORS[0].key)
  assert.equal(v.topics[1].key, TOPIC_VECTORS[1].key)
  for (const t of v.topics) {
    const h = huginTopicFromKey(t.key)
    assert.equal(hex(h.topicKey), t.topic_key, t.name)
    assert.equal(hex(h.base.publicKey), t.base_public_key, t.name)
    assert.equal(hex(h.topic), t.topic, t.name)
    assert.equal(hex(midMessage(dht.publicKey)), t.mid_message, t.name)
    const mid = signMid(h.base.secretKey, dht.publicKey)
    assert.equal(hex(mid), t.mid, t.name)
    assert.ok(verifyMid(b4a.from(t.mid, 'hex'), dht.publicKey, h.base.publicKey), t.name)
    assert.ok(!verifyMid(legacyMid(h.base.secretKey, dht.publicKey), dht.publicKey, h.base.publicKey), t.name)
  }
})

test('mid sign/verify', () => {
  const base = keyPairFromSeed(b4a.alloc(32, 1))
  const dht = keyPairFromSeed(b4a.alloc(32, 2))
  const mid = signMid(base.secretKey, dht.publicKey)
  assert.ok(verifyMid(mid, dht.publicKey, base.publicKey))
  assert.ok(!verifyMid(mid, base.publicKey, base.publicKey))
  assert.ok(!verifyMid(null, dht.publicKey, base.publicKey))
  assert.ok(!verifyMid(mid, dht.publicKey.subarray(0, 31), base.publicKey))
  // the mid signs the domain-separated message, not the bare key
  const expected = b4a.alloc(64)
  sodium.crypto_sign_detached(expected, b4a.concat([b4a.from('hugin/mid'), dht.publicKey]), base.secretKey)
  assert.ok(b4a.equals(mid, expected))
})

test('an old-scheme mid (earlier derivation) is rejected', () => {
  const base = keyPairFromSeed(b4a.alloc(32, 1))
  const dht = keyPairFromSeed(b4a.alloc(32, 2))
  const old = legacyMid(base.secretKey, dht.publicKey)
  assert.ok(sodium.crypto_sign_verify_detached(old, dht.publicKey, base.publicKey), 'valid old-scheme mid')
  assert.ok(!verifyMid(old, dht.publicKey, base.publicKey))
  // nor does a new mid pass the old check (old peers refuse us as well)
  const mid = signMid(base.secretKey, dht.publicKey)
  assert.ok(!sodium.crypto_sign_verify_detached(mid, dht.publicKey, base.publicKey))
})

test('a handshake carrying an old-scheme mid is refused; a current one is accepted', async () => {
  const base = keyPairFromSeed(b4a.alloc(32, 23))
  const a = keyPairFromSeed(b4a.alloc(32, 21))
  const b = keyPairFromSeed(b4a.alloc(32, 22))
  for (const [label, mid, ok] of [
    ['old scheme', legacyMid(base.secretKey, a.publicKey), false],
    ['current', signMid(base.secretKey, a.publicKey), true]
  ]) {
    const registry = new MidRegistry()
    registry.add(b.publicKey, { mid: signMid(base.secretKey, b.publicKey), basePublicKey: base.publicKey, topic: b4a.alloc(32) })
    registry.add(a.publicKey, { mid, basePublicKey: base.publicKey, topic: b4a.alloc(32) })
    const recv = await firstMessage(registry)
    if (ok) {
      assert.equal(hex(recv().mid), hex(mid), label)
    } else {
      assert.throws(recv, (err) => err.code === 'HUGIN_MID_REJECTED', label)
      assert.ok(registry.recentlyRejected(a.publicKey), label)
      assert.equal(registry.rejections.get(hex(a.publicKey)).reason, 'invalid mid')
    }
  }
})

test('hugin identities connect over a local testnet; plain and old-scheme peers are refused', async (t) => {
  const testnet = await createTestnet(3, { host: '127.0.0.1' })
  const managers = []
  t.after(async () => {
    for (const m of managers) await m.destroy()
    await testnet.destroy()
  })
  const got = []
  const make = (name) => {
    const m = new SwarmManager({
      keyPair: keyPairFromSeed(b4a.alloc(32, managers.length + 10)),
      bootstrap: testnet.bootstrap,
      onconnection: (socket, info) => {
        socket.on('error', () => {})
        socket.on('data', (d) => got.push([name, b4a.toString(d)]))
        socket.write(b4a.from(`hi from ${name}`))
        got.push([name, 'topic', info.topic ? b4a.toString(info.topic, 'hex') : null])
      }
    })
    managers.push(m)
    return m
  }
  const a = make('a')
  const b = make('b')
  const h = huginTopicFromKey('xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337')
  const pubA = await a.announce(h.topic, 1, { topicKey: h.topicKey })
  assert.ok(!b4a.equals(pubA, a.keyPair.publicKey), 'hugin identity uses its own key pair')
  await b.announce(h.topic, 2, { topicKey: h.topicKey })
  await waitFor(() => got.filter((g) => g[1] === 'hi from a' || g[1] === 'hi from b').length >= 2)
  assert.ok(got.some((g) => g[0] === 'a' && g[1] === 'topic' && g[2] === b4a.toString(h.topic, 'hex')))

  // a plain (mid-less) peer dialing a's hugin identity is rejected by a
  const plain = make('plain')
  const { socket } = plain.open(pubA, null, 0, null)
  const err = await new Promise((resolve) => {
    socket.on('error', resolve)
    socket.on('open', () => resolve(null))
  })
  assert.ok(err, 'connection must fail')
  assert.ok(a.registry.recentlyRejected(plain.keyPair.publicKey))

  // a client with the earlier derivation: it signs its bare DHT key (a precomputed mid without
  // topic key is sent unverified) and is refused by a's identity
  const old = make('old')
  const oldSeed = b4a.alloc(32, 0x42)
  const oldKeys = keyPairFromSeed(oldSeed)
  const opened = old.open(pubA, h.topic, 0, { mid: legacyMid(h.base.secretKey, oldKeys.publicKey), identitySeed: oldSeed })
  assert.ok(b4a.equals(opened.identity.keyPair.publicKey, oldKeys.publicKey))
  const oldErr = await new Promise((resolve) => {
    opened.socket.on('error', resolve)
    opened.socket.on('open', () => resolve(null))
  })
  assert.ok(oldErr, 'old-scheme connection must fail')
  assert.ok(a.registry.recentlyRejected(oldKeys.publicKey))
  assert.equal(a.registry.rejections.get(hex(oldKeys.publicKey)).reason, 'invalid mid')
  assert.ok(!got.some((g) => g[0] === 'old' && g[1] !== 'topic'), 'no data reached the old client')

  // the bridge refuses to join the earlier hex-fill topic, and an old-scheme mid next to a topic key
  const legacyTopic = fill32(hex(h.base.publicKey))
  assert.throws(() => old.ensureIdentity(legacyTopic, { topicKey: h.topicKey }), /does not match topic/)
  const other = huginTopicFromKey('abc')
  const seed = b4a.alloc(32, 0x43)
  const kp = keyPairFromSeed(seed)
  assert.throws(
    () => old.ensureIdentity(other.topic, { topicKey: other.topicKey, identitySeed: seed, mid: legacyMid(other.base.secretKey, kp.publicKey) }),
    /does not verify/
  )
  const ok = old.ensureIdentity(other.topic, { topicKey: other.topicKey, identitySeed: seed, mid: signMid(other.base.secretKey, kp.publicKey) })
  assert.ok(b4a.equals(ok.keyPair.publicKey, kp.publicKey))
})

async function waitFor(fn, ms = 15000) {
  const start = Date.now()
  while (!fn()) {
    if (Date.now() - start > ms) throw new Error('timeout')
    await new Promise((r) => setTimeout(r, 50))
  }
}

// Local fail-closed rules of the mid layer.
test('bytes after the mid are refused', async () => {
  const enc = c.encode(midNoisePayload, FORK_VECTORS[0].payload)
  const longer = b4a.concat([enc, b4a.from([0])])
  assert.throws(() => c.decode(midNoisePayload, longer), /trailing bytes/)
  // a payload without mid keeps upstream's handling
  assert.equal(c.decode(midNoisePayload, c.encode(midNoisePayload, FORK_VECTORS[1].payload)).mid, null)
})

/** Runs the first Noise message from an initiator to a responder whose wrap uses `registry`. */
async function firstMessage(registry, setup) {
  const { MidNoiseWrap } = await import('../src/mid.js')
  const a = keyPairFromSeed(b4a.alloc(32, 21))
  const b = keyPairFromSeed(b4a.alloc(32, 22))
  const responder = new MidNoiseWrap(registry, b, null)
  if (setup) setup(b)
  const initiator = new MidNoiseWrap(registry, a, b.publicKey)
  const msg = initiator.send(FORK_VECTORS[1].payload)
  return () => responder.recv(msg)
}

test('an identity with a mid but no topic key accepts no inbound handshake', async () => {
  const { MidRegistry } = await import('../src/mid.js')
  const registry = new MidRegistry()
  const b = keyPairFromSeed(b4a.alloc(32, 22))
  registry.add(b.publicKey, { mid: MID, basePublicKey: null, topic: b4a.alloc(32) })
  const recv = await firstMessage(registry)
  assert.throws(recv, (err) => err.code === 'HUGIN_MID_REJECTED')
})

test('an identity removed during the handshake fails it instead of skipping verification', async () => {
  const { MidRegistry } = await import('../src/mid.js')
  const registry = new MidRegistry()
  const b = keyPairFromSeed(b4a.alloc(32, 22))
  const base = keyPairFromSeed(b4a.alloc(32, 23))
  registry.add(b.publicKey, { mid: signMid(base.secretKey, b.publicKey), basePublicKey: base.publicKey, topic: b4a.alloc(32) })
  const recv = await firstMessage(registry, (kp) => registry.delete(kp.publicKey))
  assert.throws(recv, (err) => err.code === 'HUGIN_MID_REJECTED')
})
