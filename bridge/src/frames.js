// Hyper-bridge frame codec (docs/hyper-bridge-protocol.md, v1).
//
// Used by the Bare workers and the tests. Only b4a and plain typed arrays are used, so the
// module runs unchanged on Bare and Node. 64-bit fields are BigInt.

import b4a from 'b4a'

export const VERSION = Object.freeze({ major: 1, minor: 0 })

export const HEADER_SIZE = 16
export const DEFAULT_MAX_FRAME = 16 * 1024 * 1024
export const DEFAULT_INITIAL_CREDIT = 1024 * 1024
/** Smallest max_frame either side may propose (a Hello must always fit). */
export const MIN_MAX_FRAME = 4096

export const TYPE = Object.freeze({
  HELLO: 0x01,
  PING: 0x02,
  PONG: 0x03,
  OPEN: 0x10,
  OPENED: 0x11,
  CLOSE: 0x12,
  DATA: 0x20,
  CREDIT: 0x21,
  ANNOUNCE: 0x30,
  UNANNOUNCE: 0x31,
  LOOKUP: 0x32,
  LOOKUP_RESULT: 0x33,
  ACK: 0x34,
  PEER_INFO: 0x40,
  DHT_STATUS: 0x41,
  ERROR: 0x7f
})

export const TYPE_NAME = Object.freeze(
  Object.fromEntries(Object.entries(TYPE).map(([k, v]) => [v, k]))
)

export const FLAG = Object.freeze({ END: 0x01, IGNORABLE: 0x80 })

export const FEATURE = Object.freeze({
  /** The bridge understands the Hugin `mid` TLVs (0x10/0x11/0x12). */
  HUGIN_MID: 1n << 0n
})

export const TLV = Object.freeze({
  /** Hello / Ack: a node (or per-topic identity) public key, 32 B. */
  PUBLIC_KEY: 0x01,
  /** Announce / Open: precomputed Hugin `mid` signature, 64 B. */
  HUGIN_MID: 0x10,
  /** Announce / Open: Hugin topic key = the 32-byte seed of the topic base key pair. */
  HUGIN_TOPIC_KEY: 0x11,
  /** Announce / Open: seed of the DHT key pair to use for this Hugin topic, 32 B. */
  IDENTITY_SEED: 0x12
})

export const MODE = Object.freeze({ SERVER: 1, CLIENT: 2, BOTH: 3 })

/** `DhtStatus.state`. */
export const DHT_STATE = Object.freeze({ STARTING: 0, READY: 1, FAILED: 2 })

/** `DhtStatus.nat`: the public port the DHT nodes saw (unknown, one for all, a new one each). */
export const NAT = Object.freeze({ UNKNOWN: 0, CONSISTENT: 1, RANDOM: 2 })

export const OPEN_FLAG = Object.freeze({ ALLOW_RELAY: 1, NO_HOLEPUNCH: 2 })

export const CLOSE_REASON = Object.freeze({
  NORMAL: 0,
  ERROR: 1,
  TIMEOUT: 2,
  FIREWALL: 3,
  PROTOCOL: 4,
  SHUTDOWN: 5
})

/** `Close.code` values the bridge uses (informative; 0 = unspecified). */
export const CLOSE_CODE = Object.freeze({
  UNSPECIFIED: 0,
  PEER_NOT_FOUND: 1,
  PEER_CONNECTION_FAILED: 2,
  HOLEPUNCH_FAILED: 3,
  TIMEOUT: 4,
  MID_REJECTED: 5,
  REMOTE_CLOSED: 6,
  MESSAGE_TOO_LARGE: 7,
  CREDIT_EXCEEDED: 8
})

export const ERROR_CODE = Object.freeze({
  UNKNOWN_TYPE: 1,
  BAD_FRAME: 2,
  UNKNOWN_CONN: 3,
  CREDIT_EXCEEDED: 4,
  DHT_ERROR: 5,
  VERSION: 6
})

export const ZERO32 = b4a.alloc(32)
export const ZERO64 = b4a.alloc(64)
/** Noise handshake hash length in hyperdht (BLAKE2b-512). */
export const HANDSHAKE_HASH_BYTES = 64

export class FrameError extends Error {
  constructor(message, code = ERROR_CODE.BAD_FRAME) {
    super(message)
    this.code = code
  }
}

// ---------------------------------------------------------------------------------------------
// connection ids

const ORIGIN_BIT = 1n << 63n
const COUNTER_MASK = (1n << 48n) - 1n

/** Builds a conn_id: bit 63 origin (1 = Bare), bits 48..62 shard, bits 0..47 counter. */
export function makeConnId(fromBare, shard, counter) {
  const c = BigInt(counter)
  if (c < 1n || c > COUNTER_MASK) throw new RangeError('conn_id counter out of range')
  return (fromBare ? ORIGIN_BIT : 0n) | (BigInt(shard & 0x7fff) << 48n) | c
}

export function connIdFromBare(id) {
  return (id & ORIGIN_BIT) !== 0n
}

export function connIdShard(id) {
  return Number((id >> 48n) & 0x7fffn)
}

// ---------------------------------------------------------------------------------------------
// low level writer / reader

class Writer {
  constructor(size = 64) {
    this.buf = b4a.alloc(size)
    this.view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength)
    this.len = 0
  }

  _grow(n) {
    if (this.len + n <= this.buf.byteLength) return
    let size = this.buf.byteLength * 2
    while (size < this.len + n) size *= 2
    const next = b4a.alloc(size)
    next.set(this.buf.subarray(0, this.len))
    this.buf = next
    this.view = new DataView(next.buffer, next.byteOffset, next.byteLength)
  }

  u8(v) {
    this._grow(1)
    this.view.setUint8(this.len, v)
    this.len += 1
  }

  u16(v) {
    this._grow(2)
    this.view.setUint16(this.len, v, true)
    this.len += 2
  }

  u32(v) {
    this._grow(4)
    this.view.setUint32(this.len, v >>> 0, true)
    this.len += 4
  }

  u64(v) {
    this._grow(8)
    this.view.setBigUint64(this.len, BigInt.asUintN(64, BigInt(v)), true)
    this.len += 8
  }

  bytes(b) {
    this._grow(b.byteLength)
    this.buf.set(b, this.len)
    this.len += b.byteLength
  }

  fixed(b, n) {
    if (!b || b.byteLength !== n) throw new FrameError(`expected ${n} bytes`)
    this.bytes(b)
  }

  finish() {
    return this.buf.subarray(0, this.len)
  }
}

class Reader {
  constructor(buf) {
    this.buf = buf
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
    this.pos = 0
  }

  get remaining() {
    return this.buf.byteLength - this.pos
  }

  _need(n) {
    if (this.remaining < n) throw new FrameError('truncated body')
  }

  u8() {
    this._need(1)
    return this.view.getUint8(this.pos++)
  }

  u16() {
    this._need(2)
    const v = this.view.getUint16(this.pos, true)
    this.pos += 2
    return v
  }

  u32() {
    this._need(4)
    const v = this.view.getUint32(this.pos, true)
    this.pos += 4
    return v
  }

  u64() {
    this._need(8)
    const v = this.view.getBigUint64(this.pos, true)
    this.pos += 8
    return v
  }

  fixed(n) {
    this._need(n)
    const out = this.buf.subarray(this.pos, this.pos + n)
    this.pos += n
    return out
  }

  rest() {
    const out = this.buf.subarray(this.pos)
    this.pos = this.buf.byteLength
    return out
  }
}

// ---------------------------------------------------------------------------------------------
// TLV and address helpers

/** Encodes TLVs (`tag u8 | len u16 | value`). */
function writeTlvs(w, tlvs) {
  if (!tlvs) return
  for (const t of tlvs) {
    if (t.value.byteLength > 0xffff) throw new FrameError('TLV too long')
    w.u8(t.tag)
    w.u16(t.value.byteLength)
    w.bytes(t.value)
  }
}

/** Parses the TLV tail of a body. A truncated TLV is a BAD_FRAME. */
function readTlvs(r) {
  const out = []
  while (r.remaining > 0) {
    if (r.remaining < 3) throw new FrameError('truncated TLV header')
    const tag = r.u8()
    const len = r.u16()
    if (r.remaining < len) throw new FrameError('truncated TLV value')
    out.push({ tag, value: r.fixed(len) })
  }
  return out
}

/** First TLV with `tag`, or null. Unknown tags are simply never asked for. */
export function findTlv(tlvs, tag) {
  if (!tlvs) return null
  for (const t of tlvs) if (t.tag === tag) return t.value
  return null
}

export const NO_ADDR = Object.freeze({ family: 0, port: 0, addr: ZERO32.subarray(0, 16) })

/** `{ family, port, addr[16] }` from a host string. IPv4 occupies addr[0..4], rest zero. */
export function addrFromHost(host, port) {
  const addr = b4a.alloc(16)
  if (typeof host !== 'string' || host.length === 0) return { family: 0, port: 0, addr }
  if (host.startsWith('::ffff:') && host.includes('.')) host = host.slice(7)
  if (/^\d+\.\d+\.\d+\.\d+$/.test(host)) {
    const parts = host.split('.').map(Number)
    if (parts.some((p) => p > 255)) return { family: 0, port: 0, addr }
    addr.set(parts, 0)
    return { family: 4, port: port & 0xffff, addr }
  }
  const words = parseIpv6(host)
  if (!words) return { family: 0, port: 0, addr }
  for (let i = 0; i < 8; i++) {
    addr[i * 2] = words[i] >> 8
    addr[i * 2 + 1] = words[i] & 0xff
  }
  return { family: 6, port: port & 0xffff, addr }
}

function parseIpv6(host) {
  const pct = host.indexOf('%')
  if (pct !== -1) host = host.slice(0, pct)
  if (!/^[0-9a-fA-F:.]+$/.test(host)) return null
  const halves = host.split('::')
  if (halves.length > 2) return null
  const parse = (s) => {
    if (s === '') return []
    const out = []
    for (const part of s.split(':')) {
      if (part.includes('.')) {
        const v4 = part.split('.').map(Number)
        if (v4.length !== 4 || v4.some((p) => !(p >= 0 && p <= 255))) return null
        out.push((v4[0] << 8) | v4[1], (v4[2] << 8) | v4[3])
      } else {
        if (part.length === 0 || part.length > 4) return null
        out.push(parseInt(part, 16))
      }
    }
    return out
  }
  const head = parse(halves[0])
  const tail = halves.length === 2 ? parse(halves[1]) : []
  if (!head || !tail) return null
  if (halves.length === 1) return head.length === 8 ? head : null
  const fill = 8 - head.length - tail.length
  if (fill < 1) return null
  return [...head, ...new Array(fill).fill(0), ...tail]
}

/** Host string for an encoded address, or null for family 0. */
export function hostFromAddr(a) {
  if (a.family === 4) return `${a.addr[0]}.${a.addr[1]}.${a.addr[2]}.${a.addr[3]}`
  if (a.family === 6) {
    const words = []
    for (let i = 0; i < 8; i++) words.push(((a.addr[i * 2] << 8) | a.addr[i * 2 + 1]).toString(16))
    return words.join(':')
  }
  return null
}

function writeAddr(w, a) {
  const addr = a || NO_ADDR
  w.u8(addr.family)
  w.u16(addr.port)
  w.fixed(addr.addr, 16)
}

function readAddr(r) {
  const family = r.u8()
  const port = r.u16()
  const addr = b4a.from(r.fixed(16))
  return { family, port, addr }
}

const utf8 = (str) => b4a.from(str || '', 'utf8')
const fromUtf8 = (buf) => b4a.toString(buf, 'utf8')

// ---------------------------------------------------------------------------------------------
// bodies

const bodyEncoders = {
  [TYPE.HELLO](w, b) {
    w.u16(b.major)
    w.u16(b.minor)
    w.u32(b.maxFrame)
    w.u32(b.initialCredit)
    w.u16(b.workers)
    w.u16(b.worker)
    w.u64(b.features)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.PING](w, b) {
    w.u64(b.nonce)
  },
  [TYPE.PONG](w, b) {
    w.u64(b.nonce)
  },
  [TYPE.OPEN](w, b) {
    w.u32(b.requestId)
    w.fixed(b.remotePublicKey, 32)
    w.fixed(b.topic || ZERO32, 32)
    w.u32(b.openFlags)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.OPENED](w, b) {
    w.u32(b.requestId)
    w.fixed(b.remotePublicKey, 32)
    w.fixed(b.handshakeHash, 64)
    w.fixed(b.topic || ZERO32, 32)
    w.u8(b.initiator ? 1 : 0)
    writeAddr(w, b.addr)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.CLOSE](w, b) {
    w.u8(b.reason)
    w.u32(b.code)
    w.bytes(utf8(b.message))
  },
  [TYPE.DATA](w, b) {
    w.bytes(b.payload)
  },
  [TYPE.CREDIT](w, b) {
    w.u32(b.bytes)
  },
  [TYPE.ANNOUNCE](w, b) {
    w.u32(b.requestId)
    w.fixed(b.topic, 32)
    w.u8(b.mode)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.UNANNOUNCE](w, b) {
    w.u32(b.requestId)
    w.fixed(b.topic, 32)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.LOOKUP](w, b) {
    w.u32(b.requestId)
    w.fixed(b.topic, 32)
    w.u16(b.maxPeers)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.LOOKUP_RESULT](w, b) {
    w.u32(b.requestId)
    w.u16(b.peers.length)
    for (const p of b.peers) {
      w.fixed(p.publicKey, 32)
      writeAddr(w, p.addr)
    }
    writeTlvs(w, b.tlvs)
  },
  [TYPE.ACK](w, b) {
    w.u32(b.requestId)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.PEER_INFO](w, b) {
    if (b === null || b === undefined || b.request) return // request form: empty body
    w.u32(b.rttUs)
    w.u64(b.bytesIn)
    w.u64(b.bytesOut)
    w.u8(b.relayed ? 1 : 0)
    w.u8(b.holepunched ? 1 : 0)
    writeAddr(w, b.addr)
  },
  [TYPE.DHT_STATUS](w, b) {
    w.u8(b.state)
    w.u8(b.firewalled ? 1 : 0)
    w.u8(b.nat)
    w.u16(b.udpPort)
    writeTlvs(w, b.tlvs)
  },
  [TYPE.ERROR](w, b) {
    w.u32(b.requestId)
    w.u16(b.code)
    w.bytes(utf8(b.message))
  }
}

const bodyDecoders = {
  [TYPE.HELLO](r) {
    return {
      major: r.u16(),
      minor: r.u16(),
      maxFrame: r.u32(),
      initialCredit: r.u32(),
      workers: r.u16(),
      worker: r.u16(),
      features: r.u64(),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.PING](r) {
    return { nonce: r.u64() }
  },
  [TYPE.PONG](r) {
    return { nonce: r.u64() }
  },
  [TYPE.OPEN](r) {
    return {
      requestId: r.u32(),
      remotePublicKey: b4a.from(r.fixed(32)),
      topic: b4a.from(r.fixed(32)),
      openFlags: r.u32(),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.OPENED](r) {
    return {
      requestId: r.u32(),
      remotePublicKey: b4a.from(r.fixed(32)),
      handshakeHash: b4a.from(r.fixed(64)),
      topic: b4a.from(r.fixed(32)),
      initiator: r.u8() !== 0,
      addr: readAddr(r),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.CLOSE](r) {
    return { reason: r.u8(), code: r.u32(), message: fromUtf8(r.rest()) }
  },
  [TYPE.DATA](r) {
    return { payload: r.rest() }
  },
  [TYPE.CREDIT](r) {
    return { bytes: r.u32() }
  },
  [TYPE.ANNOUNCE](r) {
    return {
      requestId: r.u32(),
      topic: b4a.from(r.fixed(32)),
      mode: r.u8(),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.UNANNOUNCE](r) {
    return { requestId: r.u32(), topic: b4a.from(r.fixed(32)), tlvs: readTlvs(r) }
  },
  [TYPE.LOOKUP](r) {
    return {
      requestId: r.u32(),
      topic: b4a.from(r.fixed(32)),
      maxPeers: r.u16(),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.LOOKUP_RESULT](r) {
    const requestId = r.u32()
    const count = r.u16()
    const peers = []
    for (let i = 0; i < count; i++) {
      peers.push({ publicKey: b4a.from(r.fixed(32)), addr: readAddr(r) })
    }
    return { requestId, peers, tlvs: readTlvs(r) }
  },
  [TYPE.ACK](r) {
    return { requestId: r.u32(), tlvs: readTlvs(r) }
  },
  [TYPE.PEER_INFO](r) {
    if (r.remaining === 0) return { request: true }
    return {
      request: false,
      rttUs: r.u32(),
      bytesIn: r.u64(),
      bytesOut: r.u64(),
      relayed: r.u8() !== 0,
      holepunched: r.u8() !== 0,
      addr: readAddr(r)
    }
  },
  [TYPE.DHT_STATUS](r) {
    return {
      state: r.u8(),
      firewalled: r.u8() !== 0,
      nat: r.u8(),
      udpPort: r.u16(),
      tlvs: readTlvs(r)
    }
  },
  [TYPE.ERROR](r) {
    return { requestId: r.u32(), code: r.u16(), message: fromUtf8(r.rest()) }
  }
}

/** Frames whose body has no TLV tail must be consumed exactly. */
const EXACT = new Set([TYPE.PING, TYPE.PONG, TYPE.CREDIT, TYPE.PEER_INFO])

export function isKnownType(type) {
  return Object.prototype.hasOwnProperty.call(bodyDecoders, type)
}

// ---------------------------------------------------------------------------------------------
// frames

/**
 * Encodes one frame. `frame = { type, flags?, shard?, connId?, body }`; for unknown types
 * `body` must be `{ raw: Uint8Array }`.
 */
export function encodeFrame(frame) {
  const w = new Writer(frame.type === TYPE.DATA ? HEADER_SIZE + frame.body.payload.byteLength : 128)
  w.u32(0) // len, patched below
  w.u8(frame.type)
  w.u8(frame.flags || 0)
  w.u16(frame.shard || 0)
  w.u64(frame.connId || 0n)
  const enc = bodyEncoders[frame.type]
  if (enc) enc(w, frame.body)
  else w.bytes(frame.body.raw)
  const out = w.finish()
  new DataView(out.buffer, out.byteOffset, 4).setUint32(0, out.byteLength - 4, true)
  return out
}

/** Decodes one complete frame (header + body) from `buf`. */
export function decodeFrame(buf) {
  if (buf.byteLength < HEADER_SIZE) throw new FrameError('short frame')
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  const len = view.getUint32(0, true)
  if (len + 4 !== buf.byteLength) throw new FrameError('length mismatch')
  const type = view.getUint8(4)
  const flags = view.getUint8(5)
  const shard = view.getUint16(6, true)
  const connId = view.getBigUint64(8, true)
  const raw = buf.subarray(HEADER_SIZE)
  const dec = bodyDecoders[type]
  if (!dec) return { type, flags, shard, connId, body: { raw }, known: false }
  const r = new Reader(raw)
  const body = dec(r)
  if (EXACT.has(type) && r.remaining !== 0) throw new FrameError('trailing bytes')
  return { type, flags, shard, connId, body, known: true }
}

/**
 * Incremental decoder for a byte stream. `push(chunk)` returns the complete frames it could
 * cut; frames longer than `maxFrame` (total size incl. header) throw a FrameError.
 */
export class FrameDecoder {
  constructor({ maxFrame = DEFAULT_MAX_FRAME } = {}) {
    this.maxFrame = maxFrame
    this._chunks = []
    this._size = 0
  }

  push(chunk) {
    if (chunk.byteLength) {
      this._chunks.push(chunk)
      this._size += chunk.byteLength
    }
    const out = []
    while (this._size >= 4) {
      const head = this._peek(4)
      const len = new DataView(head.buffer, head.byteOffset, 4).getUint32(0, true)
      if (len < HEADER_SIZE - 4) throw new FrameError('frame length below header size')
      if (len + 4 > this.maxFrame) throw new FrameError('frame exceeds max_frame')
      if (this._size < len + 4) break
      out.push(decodeFrame(this._take(len + 4)))
    }
    return out
  }

  _peek(n) {
    if (this._chunks[0].byteLength >= n) return this._chunks[0].subarray(0, n)
    return this._concat(n, false)
  }

  _take(n) {
    if (this._chunks[0].byteLength === n) {
      this._size -= n
      return this._chunks.shift()
    }
    if (this._chunks[0].byteLength > n) {
      const c = this._chunks[0]
      this._chunks[0] = c.subarray(n)
      this._size -= n
      return c.subarray(0, n)
    }
    return this._concat(n, true)
  }

  _concat(n, consume) {
    const out = b4a.alloc(n)
    let off = 0
    let i = 0
    while (off < n) {
      const c = this._chunks[i]
      const take = Math.min(c.byteLength, n - off)
      out.set(c.subarray(0, take), off)
      off += take
      if (consume) {
        if (take === c.byteLength) this._chunks.shift()
        else this._chunks[0] = c.subarray(take)
      } else {
        i++
      }
    }
    if (consume) this._size -= n
    return out
  }
}

// ---------------------------------------------------------------------------------------------
// convenience constructors

export const frames = {
  hello: (b) => ({ type: TYPE.HELLO, body: b }),
  ping: (nonce) => ({ type: TYPE.PING, body: { nonce } }),
  pong: (nonce) => ({ type: TYPE.PONG, body: { nonce } }),
  opened: (connId, b) => ({ type: TYPE.OPENED, connId, body: b }),
  close: (connId, reason, code = 0, message = '') => ({
    type: TYPE.CLOSE,
    connId,
    body: { reason, code, message }
  }),
  data: (connId, payload) => ({ type: TYPE.DATA, connId, body: { payload } }),
  credit: (connId, bytes) => ({ type: TYPE.CREDIT, connId, body: { bytes } }),
  ack: (requestId, tlvs = []) => ({ type: TYPE.ACK, body: { requestId, tlvs } }),
  lookupResult: (requestId, peers, end) => ({
    type: TYPE.LOOKUP_RESULT,
    flags: end ? FLAG.END : 0,
    body: { requestId, peers, tlvs: [] }
  }),
  peerInfo: (connId, b) => ({ type: TYPE.PEER_INFO, connId, body: b }),
  /** Always IGNORABLE, so a Rust side that does not know the type drops it. */
  dhtStatus: (b) => ({ type: TYPE.DHT_STATUS, flags: FLAG.IGNORABLE, body: { tlvs: [], ...b } }),
  error: (requestId, code, message = '', connId = 0n) => ({
    type: TYPE.ERROR,
    connId,
    body: { requestId, code, message }
  })
}
