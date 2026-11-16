// Hugin `mid` on top of upstream hyperdht (no package files are modified), with the
// domain-separated topic and `mid` derivation of hugin-network 3.0.
//
// Hugin peers put one extra field in the Noise handshake payload,
//
//   flags |= 128, and after every upstream field: mid = fixed64
//
// and refuse any handshake whose `mid` does not verify against the topic base public key. The
// payload format is the one hyperdht-hugin 1.0.3 introduced; the topic and what the `mid` signs
// are domain-separated (HUGIN_TOPIC_DOMAIN / HUGIN_MID_DOMAIN below; the same constants are in
// crates/xkr-p2p-hyper/src/topic.rs and in hugin-network):
//
//   topicKey = fill32(key)                                  (Buffer.alloc(32).fill(key))
//   base     = crypto_sign_seed_keypair(topicKey)
//   topic    = cn_fast_hash("hugin/topic" || base.publicKey)              43 bytes, Keccak-256
//   mid      = crypto_sign_detached("hugin/mid" || dhtPublicKey, base.secretKey)     41 bytes
//
// and a remote `mid` is accepted only if crypto_sign_verify_detached accepts it for exactly
// "hugin/mid" || remoteDhtPublicKey under base.publicKey. Clients using the earlier derivation
// (hyperswarm-hugin: topic = fill32(hex(base.publicKey)), mid signs the bare DHT key) therefore
// neither find an updated peer on the DHT nor pass its `mid` check, in both directions. This is
// a one-time breaking change: no legacy mode, no fallback. See `huginTopicFromKey()`,
// `signMid()`, `verifyMid()`.
//
// How we add it without forking:
//   * `midNoisePayload` wraps upstream `messages.noisePayload`: it encodes/decodes the upstream
//     fields with the upstream codec and handles bit 128 + the trailing 64 bytes itself.
//   * `MidNoiseWrap extends NoiseWrap` uses that codec. It looks up the *local* key pair of the
//     handshake in a `MidRegistry`; only registered identities send a `mid` and verify the
//     remote one. Every other handshake is byte-identical to upstream.
//   * `MidDHT extends HyperDHT` passes `createHandshake` (a documented option of both
//     `dht.connect()` and `dht.createServer()` in hyperdht 6.x) into every connect/server, so
//     Hyperswarm instances sharing this DHT get the behaviour for free.

import HyperDHT from 'hyperdht'
import NoiseWrap from 'hyperdht/lib/noise-wrap.js'
import messages from 'hyperdht/lib/messages.js'
import c from 'compact-encoding'
import sodium from 'sodium-universal'
import b4a from 'b4a'
import { keccak256 } from './keccak.js'

export const MID_FLAG = 128
export const MID_BYTES = 64

/** Domain separator of a topic: `topic = cn_fast_hash(HUGIN_TOPIC_DOMAIN || basePublicKey)`. */
export const HUGIN_TOPIC_DOMAIN = 'hugin/topic'
/**
 * Domain separator of a topic on the XKR testnet (instead of HUGIN_TOPIC_DOMAIN), so testnet
 * relays and apps never meet mainnet ones. Keys, base key pairs and mids are the same.
 */
export const HUGIN_TESTNET_TOPIC_DOMAIN = 'hugin/testnet/topic'
/** Domain separator of what a `mid` signs: `HUGIN_MID_DOMAIN || dhtPublicKey`. */
export const HUGIN_MID_DOMAIN = 'hugin/mid'

const TOPIC_DOMAIN_BYTES = b4a.from(HUGIN_TOPIC_DOMAIN, 'utf8')
const TESTNET_TOPIC_DOMAIN_BYTES = b4a.from(HUGIN_TESTNET_TOPIC_DOMAIN, 'utf8')
const MID_DOMAIN_BYTES = b4a.from(HUGIN_MID_DOMAIN, 'utf8')

const upstream = messages.noisePayload

/**
 * Noise payload codec = upstream noisePayload + Hugin `mid` (flag 128, fixed64 at the end).
 * The flags field is a compact-encoding uint; upstream never sets more than bits 0..6 and the
 * largest possible value with `mid` (251) is still a single byte, so OR-ing the bit into the
 * already written byte is exactly what hyperdht-hugin's encoder produces.
 */
export const midNoisePayload = {
  preencode(state, m) {
    upstream.preencode(state, m)
    if (m.mid) state.end += MID_BYTES
  },
  encode(state, m) {
    const start = state.start
    upstream.encode(state, m)
    if (!m.mid) return
    // byte 0 = version (uint 1), byte 1 = flags (< 128 from upstream, single byte)
    if (state.buffer[start] !== 1 || state.buffer[start + 1] >= MID_FLAG) {
      throw new Error('unexpected noise payload layout')
    }
    state.buffer[start + 1] |= MID_FLAG
    c.fixed64.encode(state, m.mid)
  },
  decode(state) {
    const start = state.start
    const version = state.buffer[start]
    const flags = state.end - start > 1 ? state.buffer[start + 1] : 0
    const m = upstream.decode(state)
    m.mid = null
    // flags < 0xfd is a single byte; upstream ignores bit 7, so the cursor now sits on `mid`.
    if (version === 1 && flags < 0xfd && (flags & MID_FLAG) !== 0) {
      m.mid = c.fixed64.decode(state)
      // The mid is the last field (hyperdht-hugin writes nothing after it): bytes behind it are
      // malformed, not a future extension.
      if (state.start !== state.end) throw new Error('trailing bytes after the Hugin mid')
    }
    return m
  }
}

// ---------------------------------------------------------------------------------------------
// key helpers (sodium-universal works on Node and Bare)

/** `Buffer.alloc(32).fill(value)` without Buffer: repeats the UTF-8 bytes (or bytes) of `value`. */
export function fill32(value) {
  const src = typeof value === 'string' ? b4a.from(value, 'utf8') : value
  const out = b4a.alloc(32)
  if (src.byteLength === 0) return out
  for (let i = 0; i < 32; i++) out[i] = src[i % src.byteLength]
  return out
}

export function keyPairFromSeed(seed) {
  const publicKey = b4a.alloc(32)
  const secretKey = b4a.alloc(64)
  sodium.crypto_sign_seed_keypair(publicKey, secretKey, seed)
  return { publicKey, secretKey }
}

/**
 * Topic of a topic base public key: `cn_fast_hash("hugin/topic" || basePublicKey)`, or with
 * `network === 'testnet'` `cn_fast_hash("hugin/testnet/topic" || basePublicKey)`.
 */
export function huginTopic(basePublicKey, network = 'mainnet') {
  if (!basePublicKey || basePublicKey.byteLength !== 32) throw new TypeError('base public key must be 32 bytes')
  const domain = network === 'testnet' ? TESTNET_TOPIC_DOMAIN_BYTES : TOPIC_DOMAIN_BYTES
  return b4a.from(keccak256(b4a.concat([domain, basePublicKey])))
}

/**
 * Everything a Hugin swarm derives from its string key (`get_new_peer_keys(key)` in
 * hugin-node/hugin-network). `key` is the already-hashed string where Hugin
 * hashes (e.g. the node-mesh key is `hex(cn_fast_hash(networkAddress))`).
 */
export function huginTopicFromKey(key, network = 'mainnet') {
  const topicKey = fill32(key)
  const base = keyPairFromSeed(topicKey)
  return { topicKey, base, topic: huginTopic(base.publicKey, network) }
}

/** What a `mid` signs: `"hugin/mid" || dhtPublicKey` (41 bytes). */
export function midMessage(dhtPublicKey) {
  if (!dhtPublicKey || dhtPublicKey.byteLength !== 32) throw new TypeError('DHT public key must be 32 bytes')
  return b4a.concat([MID_DOMAIN_BYTES, dhtPublicKey])
}

/** The `mid` of `dhtPublicKey` under the topic base secret key. */
export function signMid(baseSecretKey, dhtPublicKey) {
  const sig = b4a.alloc(MID_BYTES)
  sodium.crypto_sign_detached(sig, midMessage(dhtPublicKey), baseSecretKey)
  return sig
}

/**
 * True only for a signature over exactly `"hugin/mid" || dhtPublicKey`. A `mid` of the earlier
 * derivation (signature over the bare DHT public key) is refused.
 */
export function verifyMid(mid, dhtPublicKey, basePublicKey) {
  if (!mid || mid.byteLength !== MID_BYTES) return false
  if (!dhtPublicKey || dhtPublicKey.byteLength !== 32) return false
  if (!basePublicKey || basePublicKey.byteLength !== 32) return false
  return sodium.crypto_sign_verify_detached(mid, midMessage(dhtPublicKey), basePublicKey)
}

// ---------------------------------------------------------------------------------------------
// registry

export class MidRejectedError extends Error {
  constructor(message) {
    super(message)
    this.code = 'HUGIN_MID_REJECTED'
  }
}

/**
 * Maps a local DHT public key (hex) to the Hugin identity it belongs to:
 * `{ mid: Uint8Array(64), basePublicKey: Uint8Array(32) | null, topic }`.
 * `basePublicKey === null` means "send the given mid, do not verify the remote one".
 */
export class MidRegistry {
  constructor() {
    this.identities = new Map()
    /** remote public key hex -> { at, reason } of the last mid rejection (for Close reasons). */
    this.rejections = new Map()
    this.onreject = null
  }

  add(localPublicKey, entry) {
    this.identities.set(b4a.toString(localPublicKey, 'hex'), entry)
  }

  delete(localPublicKey) {
    this.identities.delete(b4a.toString(localPublicKey, 'hex'))
  }

  get(localPublicKey) {
    if (!localPublicKey) return null
    return this.identities.get(b4a.toString(localPublicKey, 'hex')) || null
  }

  _reject(localPublicKey, remotePublicKey, reason) {
    const key = b4a.toString(remotePublicKey, 'hex')
    this.rejections.set(key, { at: Date.now(), reason })
    if (this.rejections.size > 4096) {
      const first = this.rejections.keys().next().value
      this.rejections.delete(first)
    }
    if (this.onreject) this.onreject(localPublicKey, remotePublicKey, reason)
  }

  /** True if `remotePublicKey` failed mid verification within the last `ms`. */
  recentlyRejected(remotePublicKey, ms = 60000) {
    const r = this.rejections.get(b4a.toString(remotePublicKey, 'hex'))
    return !!r && Date.now() - r.at < ms
  }
}

export class MidNoiseWrap extends NoiseWrap {
  constructor(registry, keyPair, remotePublicKey) {
    super(keyPair, remotePublicKey)
    this.registry = registry
    this.identity = registry.get(keyPair.publicKey)
    this.remoteMid = null
  }

  /**
   * The identity as the registry has it *now* (an identity added while the handshake is in
   * flight applies to it). One that was registered when the handshake started and has been
   * removed since fails the handshake instead of silently turning it into a plain one.
   */
  _currentIdentity() {
    const id = this.registry.get(this.keyPair.publicKey)
    if (!id && this.identity) {
      this.registry._reject(this.keyPair.publicKey, this.remotePublicKey || b4a.alloc(32), 'identity removed')
      throw new MidRejectedError('Hugin identity removed during the handshake')
    }
    this.identity = id
    return id
  }

  send(payload) {
    const id = this._currentIdentity()
    const p = id ? { ...payload, mid: id.mid } : payload
    return this.handshake.send(c.encode(midNoisePayload, p))
  }

  recv(buf) {
    const payload = c.decode(midNoisePayload, this.handshake.recv(buf))
    this.remotePublicKey = b4a.toBuffer(this.handshake.rs)
    this.remoteMid = payload.mid
    const id = this._currentIdentity()
    if (id && !id.basePublicKey && !this.isInitiator) {
      // A precomputed mid without the topic key cannot verify anyone: such an identity may dial
      // peers it chose, but must not accept inbound handshakes from anybody (fail closed).
      this.registry._reject(this.keyPair.publicKey, this.remotePublicKey, 'unverifiable identity')
      throw new MidRejectedError('Hugin identity without topic key accepts no inbound handshakes')
    }
    if (id && id.basePublicKey) {
      if (!payload.mid) {
        this.registry._reject(this.keyPair.publicKey, this.remotePublicKey, 'missing mid')
        throw new MidRejectedError('Hugin mid missing')
      }
      if (!verifyMid(payload.mid, this.remotePublicKey, id.basePublicKey)) {
        this.registry._reject(this.keyPair.publicKey, this.remotePublicKey, 'invalid mid')
        throw new MidRejectedError('Hugin mid does not verify')
      }
    }
    return payload
  }
}

/**
 * HyperDHT whose connects and servers use `MidNoiseWrap`. Construct with the usual hyperdht
 * options plus `midRegistry` (created if absent).
 */
export class MidDHT extends HyperDHT {
  constructor(opts = {}) {
    super(opts)
    this.midRegistry = opts.midRegistry || new MidRegistry()
    const registry = this.midRegistry
    this._createMidHandshake = (keyPair, remotePublicKey) =>
      new MidNoiseWrap(registry, keyPair, remotePublicKey)
  }

  // The mid handshake is applied last, so caller options (Hyperswarm's, or anyone's) cannot
  // replace it with an unverified one.
  connect(remotePublicKey, opts = {}) {
    return super.connect(remotePublicKey, {
      ...opts,
      createHandshake: this._createMidHandshake
    })
  }

  createServer(opts, onconnection) {
    if (typeof opts === 'function') return this.createServer({}, opts)
    return super.createServer({ ...(opts || {}), createHandshake: this._createMidHandshake }, onconnection)
  }
}
