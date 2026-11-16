import test from 'node:test'
import assert from 'node:assert/strict'
import fs from 'node:fs'
import b4a from 'b4a'
import {
  TYPE,
  FLAG,
  FrameDecoder,
  FrameError,
  encodeFrame,
  decodeFrame,
  makeConnId,
  connIdFromBare,
  connIdShard,
  addrFromHost,
  hostFromAddr,
  findTlv,
  frames
} from '../src/frames.js'
import { VECTORS } from './vectors.js'

const golden = JSON.parse(
  fs.readFileSync(new URL('../../docs/test-vectors/hyper-bridge-frames.json', import.meta.url), 'utf8')
)
const goldenByName = new Map(golden.vectors.map((v) => [v.name, v.hex]))

const normalize = (f) => ({ flags: 0, shard: 0, connId: 0n, ...f })

test('every hand-written vector has a golden hex and vice versa', () => {
  assert.deepEqual(VECTORS.map((v) => v.name).sort(), [...goldenByName.keys()].sort())
})

for (const v of VECTORS) {
  test(`golden vector ${v.name}: encode`, () => {
    const hex = b4a.toString(encodeFrame(normalize(v.frame)), 'hex')
    assert.equal(hex, goldenByName.get(v.name))
  })

  test(`golden vector ${v.name}: decode`, () => {
    const decoded = decodeFrame(b4a.from(goldenByName.get(v.name), 'hex'))
    const expected = normalize(v.frame)
    assert.equal(decoded.type, expected.type)
    assert.equal(decoded.flags, expected.flags)
    assert.equal(decoded.shard, expected.shard)
    assert.equal(decoded.connId, expected.connId)
    // re-encoding the decoded frame must give the same bytes
    assert.equal(b4a.toString(encodeFrame(decoded), 'hex'), goldenByName.get(v.name))
  })
}

test('decoded bodies carry the expected fields', () => {
  const open = decodeFrame(b4a.from(goldenByName.get('open'), 'hex')).body
  assert.equal(open.requestId, 7)
  assert.equal(open.openFlags, 1)
  assert.ok(b4a.equals(findTlv(open.tlvs, 0x11), b4a.alloc(32, 0xcc)))

  const opened = decodeFrame(b4a.from(goldenByName.get('opened_inbound_v4'), 'hex'))
  assert.equal(opened.body.initiator, false)
  assert.equal(hostFromAddr(opened.body.addr), '192.168.1.10')
  assert.equal(opened.body.addr.port, 49737)
  assert.ok(connIdFromBare(opened.connId))

  const v6 = decodeFrame(b4a.from(goldenByName.get('opened_outbound_v6'), 'hex')).body
  assert.equal(hostFromAddr(v6.addr), '2001:db8:0:0:0:0:0:1')

  const close = decodeFrame(b4a.from(goldenByName.get('close'), 'hex')).body
  assert.deepEqual(close, { reason: 3, code: 5, message: 'mid rejected' })

  const lr = decodeFrame(b4a.from(goldenByName.get('lookup_result'), 'hex'))
  assert.equal(lr.flags & FLAG.END, FLAG.END)
  assert.equal(lr.body.peers.length, 2)
  assert.equal(lr.body.peers[1].addr.family, 0)

  const pi = decodeFrame(b4a.from(goldenByName.get('peer_info'), 'hex')).body
  assert.equal(pi.rttUs, 1500)
  assert.equal(pi.bytesOut, 2000n)
  assert.equal(pi.holepunched, true)
  assert.equal(decodeFrame(b4a.from(goldenByName.get('peer_info_request'), 'hex')).body.request, true)

  const ds = decodeFrame(b4a.from(goldenByName.get('dht_status'), 'hex'))
  assert.equal(ds.flags & FLAG.IGNORABLE, FLAG.IGNORABLE)
  assert.equal(ds.connId, 0n)
  assert.deepEqual(ds.body, { state: 1, firewalled: true, nat: 2, udpPort: 49737, tlvs: [] })
  const built = encodeFrame(frames.dhtStatus({ state: 1, firewalled: true, nat: 2, udpPort: 49737 }))
  assert.equal(b4a.toString(built, 'hex'), goldenByName.get('dht_status'))

  const unk = decodeFrame(b4a.from(goldenByName.get('unknown_ignorable'), 'hex'))
  assert.equal(unk.known, false)
  assert.equal(unk.flags & FLAG.IGNORABLE, FLAG.IGNORABLE)
})

test('unknown TLV tags are kept and skipped by findTlv', () => {
  const f = {
    type: TYPE.ACK,
    body: { requestId: 1, tlvs: [{ tag: 0xfe, value: b4a.from([9, 9]) }, { tag: 0x01, value: b4a.alloc(32, 7) }] }
  }
  const d = decodeFrame(encodeFrame(f))
  assert.equal(d.body.tlvs.length, 2)
  assert.ok(b4a.equals(findTlv(d.body.tlvs, 0x01), b4a.alloc(32, 7)))
  assert.equal(findTlv(d.body.tlvs, 0x02), null)
})

test('truncated TLV and trailing bytes are BAD_FRAME', () => {
  const ack = b4a.from(goldenByName.get('ack'), 'hex')
  const cut = b4a.from(ack.subarray(0, ack.byteLength - 1))
  new DataView(cut.buffer, cut.byteOffset).setUint32(0, cut.byteLength - 4, true)
  assert.throws(() => decodeFrame(cut), FrameError)

  const ping = b4a.from(goldenByName.get('ping') + '00', 'hex')
  new DataView(ping.buffer, ping.byteOffset).setUint32(0, ping.byteLength - 4, true)
  assert.throws(() => decodeFrame(ping), FrameError)
})

test('FrameDecoder handles byte-by-byte and coalesced input', () => {
  const all = b4a.concat(golden.vectors.map((v) => b4a.from(v.hex, 'hex')))
  const dec = new FrameDecoder()
  const got = []
  for (let i = 0; i < all.byteLength; i++) got.push(...dec.push(all.subarray(i, i + 1)))
  assert.equal(got.length, golden.vectors.length)
  const dec2 = new FrameDecoder()
  assert.equal(dec2.push(all).length, golden.vectors.length)
  for (let i = 0; i < got.length; i++) {
    assert.equal(b4a.toString(encodeFrame(got[i]), 'hex'), golden.vectors[i].hex)
  }
})

test('FrameDecoder enforces max_frame and minimum length', () => {
  const dec = new FrameDecoder({ maxFrame: 4096 })
  const big = encodeFrame({ type: TYPE.DATA, connId: 1n, body: { payload: b4a.alloc(4096) } })
  assert.throws(() => dec.push(big), FrameError)
  const tiny = b4a.from('0400000001000000', 'hex')
  assert.throws(() => new FrameDecoder().push(tiny), FrameError)
})

test('conn_id layout', () => {
  const id = makeConnId(true, 3, 42)
  assert.ok(connIdFromBare(id))
  assert.equal(connIdShard(id), 3)
  assert.equal(id & ((1n << 48n) - 1n), 42n)
  assert.equal(makeConnId(false, 0, 1), 1n)
  assert.throws(() => makeConnId(false, 0, 0))
})

test('address helpers', () => {
  assert.equal(hostFromAddr(addrFromHost('::ffff:10.1.2.3', 1)), '10.1.2.3')
  assert.equal(hostFromAddr(addrFromHost('::1', 1)), '0:0:0:0:0:0:0:1')
  assert.equal(addrFromHost(null, 5).family, 0)
  assert.equal(addrFromHost('not an ip', 5).family, 0)
})
