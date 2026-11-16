// Hyperswarm wrapper for the bridge: one shared DHT node (one UDP socket), one "plain" swarm for
// topics without Hugin `mid`, and one swarm per Hugin identity (each with its own DHT key pair,
// because a Hugin `mid` signs the local DHT public key and Hyperswarm has one key pair per swarm).
// Every swarm is created with `{ dht }`, so all of them listen and dial through the same UDP
// port: the one a node behind a strict firewall forwards (xkrd `--port`).
// Hugin topics and `mid`s use the domain-separated derivation only (mid.js); clients using the
// earlier derivation cannot connect.
//
// No Node-only APIs here (runs on Bare as well); callers pass callbacks instead of an emitter.

import Hyperswarm from 'hyperswarm'
import sodium from 'sodium-universal'
import b4a from 'b4a'
import { DHT_STATE, NAT } from './frames.js'
import {
  MidDHT,
  MidRegistry,
  keyPairFromSeed,
  huginTopic,
  signMid,
  verifyMid
} from './mid.js'

const IDENTITY_NS = b4a.from('xkr-hyper-bridge/hugin-identity/v1')
const DEFAULT_LOOKUP_TIMEOUT_MS = 20000
/** How often the DHT state is checked for changes that come without an event. */
const STATUS_CHECK_MS = 60000
/** DHT events after which the state may have changed. */
const STATUS_EVENTS = ['listening', 'ready', 'nat-update', 'persistent', 'network-update']

function hex(buf) {
  return b4a.toString(buf, 'hex')
}

export class SwarmManager {
  /**
   * @param {object} opts
   * @param {{publicKey, secretKey}} opts.keyPair   main DHT key pair
   * @param {Array<{host, port}>} [opts.bootstrap] DHT bootstrap nodes (default: hyperdht's)
   * @param {number} [opts.port]                    fixed UDP port of the shared DHT node: exactly
   *                                                this port, never another one
   * @param {number} [opts.maxPeers]                connection cap of the plain swarm (default 64)
   * @param {(socket, info) => void} opts.onconnection
   * @param {(status) => void} [opts.onstatus]      called when the DHT state may have changed
   *                                                (see status())
   * @param {(level, msg, fields?) => void} [opts.log]
   */
  constructor(opts) {
    this.keyPair = opts.keyPair
    this.log = opts.log || (() => {})
    this.onconnection = opts.onconnection
    this.onstatus = opts.onstatus || null
    this.failed = false
    this.registry = new MidRegistry()
    this.registry.onreject = (local, remote, reason) => {
      this.log('info', 'hugin mid rejected', { local: hex(local), remote: hex(remote), reason })
    }

    const dhtOpts = { keyPair: this.keyPair, midRegistry: this.registry }
    if (opts.bootstrap && opts.bootstrap.length) dhtOpts.bootstrap = opts.bootstrap
    if (opts.port) {
      // hyperdht would otherwise try the next four ports and then a random one; a forwarded
      // port must be the one in use, or the start fails and says so (status().state FAILED).
      dhtOpts.port = opts.port
      dhtOpts.anyPort = false
    }
    this.dht = new MidDHT(dhtOpts)
    this._onstatusevent = () => this._statuschanged()
    for (const ev of STATUS_EVENTS) this.dht.on(ev, this._onstatusevent)
    this._statusTimer = setInterval(this._onstatusevent, STATUS_CHECK_MS)
    if (typeof this._statusTimer.unref === 'function') this._statusTimer.unref()

    this.maxPeers = opts.maxPeers || 0
    this.plain = this._makeSwarm(this.keyPair, null)
    /** topic hex -> identity { topic, keyPair, swarm, basePublicKey, mid } */
    this.identities = new Map()
    /** topic hex -> swarm that joined it */
    this.joined = new Map()
    this.explicit = new Set()
    this.destroyed = false
  }

  async ready() {
    await this.dht.fullyBootstrapped()
  }

  /**
   * The DHT node's state, the body of a DhtStatus frame: `state` (DHT_STATE), `firewalled`
   * (other DHT nodes cannot reach our UDP port without holepunching), `nat` (NAT: the public
   * port the DHT nodes saw) and `udpPort` (local port of the DHT server socket, 0 if unbound).
   */
  status() {
    const dht = this.dht
    let udpPort = 0
    try {
      const local = dht.localAddress()
      if (local && local.port) udpPort = local.port
    } catch {
      udpPort = 0
    }
    let nat = NAT.UNKNOWN
    if (dht.host !== null && dht.host !== undefined) nat = dht.port === 0 ? NAT.RANDOM : NAT.CONSISTENT
    let state = DHT_STATE.STARTING
    if (this.failed) state = DHT_STATE.FAILED
    else if (dht.bootstrapped) state = DHT_STATE.READY
    return { state, firewalled: !!dht.firewalled, nat, udpPort }
  }

  /** The DHT could not start (e.g. the fixed UDP port is taken). */
  markFailed() {
    this.failed = true
    this._statuschanged()
  }

  _statuschanged() {
    if (this.destroyed || !this.onstatus) return
    try {
      this.onstatus(this.status())
    } catch (err) {
      this.log('warn', 'dht status handler failed', { err: err.message })
    }
  }

  _makeSwarm(keyPair, identity) {
    const swarmOpts = { dht: this.dht, keyPair }
    // The cap applies to the plain swarm (xkr topics); Hugin identities keep Hyperswarm's default.
    if (!identity && this.maxPeers > 0) swarmOpts.maxPeers = this.maxPeers
    const swarm = new Hyperswarm(swarmOpts)
    swarm.on('connection', (socket, peerInfo) => {
      const topic = identity
        ? identity.topic
        : peerInfo && peerInfo.topics && peerInfo.topics.length
          ? peerInfo.topics[0]
          : null
      this.onconnection(socket, { topic, identity, explicit: false })
    })
    return swarm
  }

  /**
   * Returns the Hugin identity for `topic`, creating it if `hugin` options are given.
   * `hugin = { topicKey?: 32 B seed, mid?: 64 B, identitySeed?: 32 B }`.
   */
  ensureIdentity(topic, hugin) {
    const key = hex(topic)
    const existing = this.identities.get(key)
    if (existing) return existing
    if (!hugin || (!hugin.topicKey && !hugin.mid)) return null

    let basePublicKey = null
    let base = null
    if (hugin.topicKey) {
      base = keyPairFromSeed(hugin.topicKey)
      basePublicKey = base.publicKey
      // the topic of the key on mainnet or on the testnet
      if (!b4a.equals(huginTopic(basePublicKey), topic) && !b4a.equals(huginTopic(basePublicKey, 'testnet'), topic)) {
        throw new Error('hugin topic key does not match topic')
      }
    }

    let seed = hugin.identitySeed
    if (!seed) {
      // Stable per (main key, topic) and unlinkable to the main key.
      seed = b4a.alloc(32)
      sodium.crypto_generichash_batch(seed, [IDENTITY_NS, this.keyPair.secretKey, topic])
    }
    const keyPair = keyPairFromSeed(seed)
    // A precomputed mid next to a topic key must be the mid of this identity under that key: we
    // would otherwise send a mid (e.g. one of the earlier derivation) no updated peer accepts.
    if (hugin.mid && base && !verifyMid(hugin.mid, keyPair.publicKey, basePublicKey)) {
      throw new Error('hugin mid does not verify for this identity under the topic key')
    }
    const mid = hugin.mid || signMid(base.secretKey, keyPair.publicKey)

    const identity = { topic: b4a.from(topic), keyPair, basePublicKey, mid, swarm: null }
    this.registry.add(keyPair.publicKey, { mid, basePublicKey, topic: identity.topic })
    identity.swarm = this._makeSwarm(keyPair, identity)
    this.identities.set(key, identity)
    this.log('info', 'hugin identity created', {
      topic: key,
      publicKey: hex(keyPair.publicKey),
      verify: !!basePublicKey
    })
    return identity
  }

  /** Joins `topic`; resolves with the local public key used once the topic is announced. */
  async announce(topic, mode, hugin) {
    const identity = this.ensureIdentity(topic, hugin)
    const swarm = identity ? identity.swarm : this.plain
    const key = hex(topic)
    const prev = this.joined.get(key)
    if (prev && prev !== swarm) await prev.leave(topic)
    this.joined.set(key, swarm)
    const discovery = swarm.join(topic, {
      server: (mode & 1) !== 0,
      client: (mode & 2) !== 0
    })
    await discovery.flushed()
    return identity ? identity.keyPair.publicKey : this.keyPair.publicKey
  }

  async unannounce(topic) {
    const key = hex(topic)
    const swarm = this.joined.get(key)
    if (!swarm) return
    this.joined.delete(key)
    await swarm.leave(topic)
  }

  /** DHT lookup; returns up to `max` unique `{ publicKey, relayAddresses }`. */
  async lookup(topic, max, timeoutMs = DEFAULT_LOOKUP_TIMEOUT_MS) {
    const found = new Map()
    const query = this.dht.lookup(topic)
    const timer = setTimeout(() => query.destroy(), timeoutMs)
    try {
      for await (const data of query) {
        for (const peer of data.peers) {
          const k = hex(peer.publicKey)
          if (found.has(k)) continue
          if (b4a.equals(peer.publicKey, this.keyPair.publicKey)) continue
          found.set(k, { publicKey: peer.publicKey, relayAddresses: peer.relayAddresses || [] })
          if (found.size >= max) break
        }
        if (found.size >= max) break
      }
    } catch (err) {
      if (found.size === 0 && !query.destroyed) throw err
    } finally {
      clearTimeout(timer)
      if (!query.destroyed) query.destroy()
    }
    return [...found.values()]
  }

  /**
   * Explicit outbound connection. Uses the Hugin identity of `topic` if one exists (or can be
   * created from `hugin`), otherwise the main key pair. Returns the NoiseSecretStream.
   */
  open(remotePublicKey, topic, flags, hugin) {
    const identity = topic ? this.ensureIdentity(topic, hugin) : null
    const opts = { keyPair: identity ? identity.keyPair : this.keyPair }
    if (flags & 2) opts.holepunch = () => false // no-holepunch: direct/relayed handshake only
    const socket = this.dht.connect(remotePublicKey, opts)
    this.explicit.add(socket)
    socket.on('close', () => this.explicit.delete(socket))
    return { socket, identity }
  }

  _swarms() {
    return [this.plain, ...[...this.identities.values()].map((i) => i.swarm)]
  }

  async destroy() {
    if (this.destroyed) return
    this.destroyed = true
    clearInterval(this._statusTimer)
    for (const ev of STATUS_EVENTS) this.dht.off(ev, this._onstatusevent)
    for (const socket of this.explicit) socket.destroy()
    this.explicit.clear()
    // Hyperswarm.destroy() would also destroy the shared DHT, so close each swarm by hand.
    await Promise.allSettled(this._swarms().map((swarm) => closeSwarmKeepDht(swarm)))
    try {
      await this.dht.destroy()
    } catch (err) {
      // A DHT whose UDP socket never bound cannot close it; stop its interface watcher anyway,
      // so nothing keeps the worklet alive.
      const watcher = this.dht.io && this.dht.io.networkInterfaces
      if (watcher && typeof watcher.destroy === 'function') await watcher.destroy()
      if (!this.failed) throw err
    }
  }
}

/**
 * Hyperswarm#destroy minus `dht.destroy()`. Relies on `swarm._timer` and
 * `swarm._allConnections` (internal, stable across hyperswarm 4.x); see bridge/README.md.
 */
async function closeSwarmKeepDht(swarm) {
  if (swarm.destroyed) return
  swarm.destroyed = true
  if (swarm._timer) swarm._timer.destroy()
  await swarm.clear()
  await swarm.server.close()
  const all = swarm._allConnections ? [...swarm._allConnections] : [...swarm.connections]
  for (const conn of all) conn.destroy()
}
