// One Rust client session of the hyper-bridge (docs/hyper-bridge-protocol.md): the frame
// protocol on top of a SwarmManager, over any duplex byte stream with write/end/destroy and
// 'data'/'error'/'close' events. worker.js runs it over Bare.IPC inside xkrd (worker i of N);
// the tests run it under Node over in-memory streams.
//
// No Node-only APIs here.

import b4a from 'b4a'
import sodium from 'sodium-universal'
import {
  VERSION,
  TYPE,
  TYPE_NAME,
  FLAG,
  FEATURE,
  TLV,
  CLOSE_REASON,
  CLOSE_CODE,
  ERROR_CODE,
  HEADER_SIZE,
  MIN_MAX_FRAME,
  ZERO32,
  ZERO64,
  FrameDecoder,
  FrameError,
  encodeFrame,
  frames,
  findTlv,
  makeConnId,
  connIdFromBare,
  connIdShard,
  addrFromHost
} from './frames.js'

const KEEPALIVE_IDLE_MS = 10000
const KEEPALIVE_TIMEOUT_MS = 10000
const OPEN_TIMEOUT_MS = 30000
const LOOKUP_CHUNK = 256
/** The Rust side must send its Hello within this. */
const HELLO_TIMEOUT_MS = 10000

export function parseHostPort(s) {
  const m = /^\[([^\]]+)\]:(\d+)$/.exec(s) || /^([^:]+):(\d+)$/.exec(s)
  if (!m) throw new Error(`invalid host:port ${s}`)
  const port = Number(m[2])
  if (!(port > 0 && port < 65536)) throw new Error(`invalid port in ${s}`)
  return { host: m[1], port }
}

export function randomBytes(n) {
  const b = b4a.alloc(n)
  sodium.randombytes_buf(b)
  return b
}

function randomNonce() {
  const b = randomBytes(8)
  return new DataView(b.buffer, b.byteOffset, 8).getBigUint64(0, true)
}

function huginOpts(tlvs) {
  const topicKey = findTlv(tlvs, TLV.HUGIN_TOPIC_KEY)
  const mid = findTlv(tlvs, TLV.HUGIN_MID)
  const identitySeed = findTlv(tlvs, TLV.IDENTITY_SEED)
  if (topicKey && topicKey.byteLength !== 32) throw new FrameError('hugin topic key must be 32 bytes')
  if (mid && mid.byteLength !== 64) throw new FrameError('hugin mid must be 64 bytes')
  if (identitySeed && identitySeed.byteLength !== 32) throw new FrameError('identity seed must be 32 bytes')
  if (!topicKey && !mid) return null
  return { topicKey, mid, identitySeed }
}

function isZero(buf) {
  for (const x of buf) if (x !== 0) return false
  return true
}

// ---------------------------------------------------------------------------------------------
// one Rust client session

export class Session {
  /**
   * @param socket duplex byte stream to the Rust client
   * @param ctx    { opts, log, swarm, onsessionclose, worker?, workers? }; `worker` of
   *               `workers` (default 0 of 1) goes into Hello, `header.shard` and the shard bits
   *               of every conn_id
   */
  constructor(socket, ctx) {
    this.socket = socket
    this.ctx = ctx
    this.worker = ctx.worker ?? 0
    this.workers = ctx.workers ?? 1
    this.log = ctx.log
    this.swarm = ctx.swarm
    this.decoder = new FrameDecoder({ maxFrame: ctx.opts.maxFrame })
    this.peerHello = null
    this.maxFrame = ctx.opts.maxFrame
    this.initialCredit = ctx.opts.initialCredit
    this.features = 0n
    this.conns = new Map()
    this.nextInbound = 1
    this.unknownConnReported = new Set()
    this.lastRx = Date.now()
    this.ping = null
    this.closed = false
    /** The last DhtStatus sent (as a string), so a state is sent once. */
    this.dhtStatusSent = null

    if (typeof socket.setNoDelay === 'function') socket.setNoDelay(true)
    socket.on('data', (chunk) => this._ondata(chunk))
    socket.on('error', (err) => this.log('warn', 'client socket error', { err: err.message }))
    socket.on('close', () => this._onclose())

    this._send(frames.hello({
      major: VERSION.major,
      minor: VERSION.minor,
      maxFrame: ctx.opts.maxFrame,
      initialCredit: ctx.opts.initialCredit,
      workers: this.workers,
      worker: this.worker,
      features: FEATURE.HUGIN_MID,
      tlvs: [{ tag: TLV.PUBLIC_KEY, value: ctx.swarm.keyPair.publicKey }]
    }))

    this._helloTimer = setTimeout(() => {
      if (this.peerHello !== null || this.closed) return
      this.log('warn', 'client sent no Hello in time')
      this.closed = true
      this.socket.destroy()
    }, HELLO_TIMEOUT_MS)
    this._helloTimer.unref()

    this._keepalive = setInterval(() => this._tick(), 1000)
    const pi = ctx.opts.peerInfoInterval
    this._peerInfoTimer = pi > 0 ? setInterval(() => this._broadcastPeerInfo(), pi * 1000) : null
  }

  _send(frame) {
    if (this.closed) return
    // Every frame of this session belongs to this worker (conn_ids carry the same shard).
    frame.shard = this.worker
    this.socket.write(encodeFrame(frame))
  }

  /** Protocol violation on the bridge itself: report and drop the client. */
  _fatal(code, message, requestId = 0) {
    this.log('warn', 'closing bridge', { code, message })
    this._send(frames.error(requestId, code, message))
    this.closed = true
    this.socket.end()
    setTimeout(() => this.socket.destroy(), 1000).unref()
  }

  _ondata(chunk) {
    if (this.closed) return
    this.lastRx = Date.now()
    let list
    try {
      list = this.decoder.push(chunk)
    } catch (err) {
      this._fatal(ERROR_CODE.BAD_FRAME, err.message)
      return
    }
    for (const f of list) {
      if (this.closed) return
      try {
        this._onframe(f)
      } catch (err) {
        if (err instanceof FrameError) {
          this._fatal(err.code, err.message)
          return
        }
        this.log('error', 'frame handler failed', { type: TYPE_NAME[f.type], err: err.stack })
      }
    }
  }

  _onframe(f) {
    if (f.shard !== this.worker) throw new FrameError(`frame for shard ${f.shard} on worker ${this.worker}`)
    if (!f.known) {
      if (f.flags & FLAG.IGNORABLE) return
      this._fatal(ERROR_CODE.UNKNOWN_TYPE, `unknown frame type 0x${f.type.toString(16)}`)
      return
    }
    if (this.peerHello === null) {
      if (f.type !== TYPE.HELLO) throw new FrameError('expected Hello first')
      this._onhello(f.body)
      return
    }
    switch (f.type) {
      case TYPE.PING: return this._send(frames.pong(f.body.nonce))
      case TYPE.PONG:
        if (this.ping && this.ping.nonce === f.body.nonce) this.ping = null
        return
      case TYPE.OPEN: return this._onopen(f)
      case TYPE.CLOSE: return this._onclosefromrust(f)
      case TYPE.DATA: return this._ondatafromrust(f)
      case TYPE.CREDIT: return this._oncredit(f)
      case TYPE.ANNOUNCE: return this._onannounce(f.body)
      case TYPE.UNANNOUNCE: return this._onunannounce(f.body)
      case TYPE.LOOKUP: return this._onlookup(f.body)
      case TYPE.PEER_INFO: return this._onpeerinforequest(f)
      case TYPE.ERROR:
        this.log('warn', 'error from client', { ...f.body, connId: f.connId.toString() })
        return
      default:
        // Hello again, or a B->R frame sent by the client
        this._send(frames.error(0, ERROR_CODE.BAD_FRAME, `unexpected ${TYPE_NAME[f.type]}`, f.connId))
    }
  }

  _onhello(h) {
    if (h.major !== VERSION.major) {
      this._fatal(ERROR_CODE.VERSION, `major ${h.major} != ${VERSION.major}`)
      return
    }
    const maxFrame = Math.min(h.maxFrame, this.ctx.opts.maxFrame)
    const initialCredit = Math.min(h.initialCredit, this.ctx.opts.initialCredit)
    if (maxFrame < MIN_MAX_FRAME || initialCredit < 1) {
      this._fatal(ERROR_CODE.VERSION, 'max_frame or initial_credit too small')
      return
    }
    this.peerHello = h
    this.maxFrame = maxFrame
    this.decoder.maxFrame = maxFrame
    this.initialCredit = initialCredit
    this.features = h.features & FEATURE.HUGIN_MID
    this.minor = Math.min(h.minor, VERSION.minor)
    this.log('info', 'client connected', { maxFrame, initialCredit, minor: this.minor })
    this.sendDhtStatus()
  }

  /**
   * Sends a DhtStatus frame with `status` (default: the swarm's current state) unless the
   * client got the same state last time. Nothing before the client's Hello.
   */
  sendDhtStatus(status) {
    if (this.closed || this.peerHello === null) return
    const st = status || (typeof this.swarm.status === 'function' ? this.swarm.status() : null)
    if (!st) return
    const key = `${st.state}/${st.firewalled ? 1 : 0}/${st.nat}/${st.udpPort}`
    if (key === this.dhtStatusSent) return
    this.dhtStatusSent = key
    this._send(frames.dhtStatus({
      state: st.state,
      firewalled: st.firewalled,
      nat: st.nat,
      udpPort: st.udpPort
    }))
  }

  _tick() {
    const now = Date.now()
    if (this.ping) {
      if (now - this.ping.at >= KEEPALIVE_TIMEOUT_MS) {
        this.log('warn', 'client keepalive timeout')
        this.closed = true
        this.socket.destroy()
      }
      return
    }
    if (now - this.lastRx >= KEEPALIVE_IDLE_MS) {
      this.ping = { nonce: randomNonce(), at: now }
      this._send(frames.ping(this.ping.nonce))
    }
  }

  // ---- connections ------------------------------------------------------------------------

  _newConn(id, socket, meta) {
    const conn = {
      id,
      socket,
      requestId: meta.requestId,
      topic: meta.topic,
      identity: meta.identity,
      opened: false,
      closeSent: false,
      closeReceived: false,
      timedOut: false,
      lastError: null,
      sendWindow: this.initialCredit, // B -> R
      queue: [],
      paused: false,
      rxOutstanding: 0, // R -> B bytes not yet credited back
      pendingCredit: 0, // written but waiting for 'drain'
      openTimer: null
    }
    this.conns.set(id, conn)
    socket.on('error', (err) => {
      conn.lastError = err
    })
    socket.on('end', () => socket.end())
    socket.on('drain', () => this._flushPendingCredit(conn))
    socket.on('close', () => this._onsocketclose(conn))
    socket.opened.then((ok) => {
      if (!ok) return
      try {
        this._onsocketopen(conn)
      } catch (err) {
        this.log('error', 'open handler failed', { err: err.stack })
        this._closeFromBridge(conn, CLOSE_REASON.ERROR, CLOSE_CODE.UNSPECIFIED, err.message)
      }
    }, () => {})
    return conn
  }

  _onsocketopen(conn) {
    if (conn.openTimer) clearTimeout(conn.openTimer)
    conn.openTimer = null
    if (conn.closeSent || this.closed) return
    conn.opened = true
    const s = conn.socket
    const raw = s.rawStream
    const addr = addrFromHost(raw && raw.remoteHost, raw && raw.remotePort)
    const local = conn.identity ? conn.identity.keyPair.publicKey : this.swarm.keyPair.publicKey
    this._send(frames.opened(conn.id, {
      requestId: conn.requestId,
      remotePublicKey: s.remotePublicKey,
      handshakeHash: s.handshakeHash && s.handshakeHash.byteLength === 64 ? s.handshakeHash : ZERO64,
      topic: conn.topic || ZERO32,
      initiator: s.isInitiator,
      addr,
      tlvs: [{ tag: TLV.PUBLIC_KEY, value: local }]
    }))
    s.on('data', (msg) => this._deliver(conn, msg))
  }

  _deliver(conn, msg) {
    if (conn.closeSent) return
    if (msg.byteLength > this.maxFrame - HEADER_SIZE) {
      this._closeFromBridge(conn, CLOSE_REASON.PROTOCOL, CLOSE_CODE.MESSAGE_TOO_LARGE,
        `message of ${msg.byteLength} bytes exceeds max_frame`)
      return
    }
    if (conn.queue.length === 0 && this._canSend(conn, msg.byteLength)) {
      conn.sendWindow -= msg.byteLength
      this._send(frames.data(conn.id, msg))
      return
    }
    conn.queue.push(msg)
    if (!conn.paused) {
      conn.paused = true
      conn.socket.pause()
    }
  }

  /** A message fits if the window covers it, or the window is full (oversized message rule). */
  _canSend(conn, len) {
    return conn.sendWindow >= len || conn.sendWindow >= this.initialCredit
  }

  _oncredit(f) {
    const conn = this.conns.get(f.connId)
    if (!conn) return this._unknownConn(f.connId)
    conn.sendWindow += f.body.bytes
    if (conn.sendWindow > this.initialCredit) {
      this.log('debug', 'credit above initial window', { connId: f.connId.toString() })
      conn.sendWindow = this.initialCredit
    }
    while (conn.queue.length && this._canSend(conn, conn.queue[0].byteLength) && !conn.closeSent) {
      const msg = conn.queue.shift()
      conn.sendWindow -= msg.byteLength
      this._send(frames.data(conn.id, msg))
    }
    if (conn.queue.length === 0 && conn.paused && !conn.closeSent) {
      conn.paused = false
      conn.socket.resume()
    }
  }

  _ondatafromrust(f) {
    const conn = this.conns.get(f.connId)
    if (!conn) return this._unknownConn(f.connId)
    if (conn.closeSent || conn.closeReceived) return
    const len = f.body.payload.byteLength
    if (conn.rxOutstanding > 0 && conn.rxOutstanding + len > this.initialCredit) {
      this._send(frames.error(0, ERROR_CODE.CREDIT_EXCEEDED, 'credit window exceeded', conn.id))
      this._closeFromBridge(conn, CLOSE_REASON.PROTOCOL, CLOSE_CODE.CREDIT_EXCEEDED, 'credit window exceeded')
      return
    }
    conn.rxOutstanding += len
    const ok = conn.socket.write(f.body.payload)
    if (ok && conn.pendingCredit === 0) {
      this._credit(conn, len)
    } else {
      conn.pendingCredit += len
    }
  }

  _flushPendingCredit(conn) {
    if (conn.pendingCredit === 0) return
    const n = conn.pendingCredit
    conn.pendingCredit = 0
    this._credit(conn, n)
  }

  _credit(conn, n) {
    conn.rxOutstanding -= n
    if (!conn.closeSent) this._send(frames.credit(conn.id, n))
  }

  _unknownConn(id) {
    if (this.unknownConnReported.has(id)) return
    this.unknownConnReported.add(id)
    if (this.unknownConnReported.size > 4096) {
      this.unknownConnReported.delete(this.unknownConnReported.values().next().value)
    }
    this._send(frames.error(0, ERROR_CODE.UNKNOWN_CONN, 'unknown conn_id', id))
  }

  _onopen(f) {
    const b = f.body
    const id = f.connId
    if (id === 0n || connIdFromBare(id) || connIdShard(id) !== this.worker || this.conns.has(id)) {
      this._send(frames.error(b.requestId, ERROR_CODE.BAD_FRAME, 'invalid or reused conn_id', id))
      return
    }
    const topic = isZero(b.topic) ? null : b.topic
    let opened
    try {
      opened = this.swarm.open(b.remotePublicKey, topic, b.openFlags, huginOpts(b.tlvs))
    } catch (err) {
      // Reserve the id until the client answers our Close (two-way close).
      this.conns.set(id, { id, closeSent: true, closeReceived: false, socket: null, queue: [] })
      this._send(frames.close(id, CLOSE_REASON.ERROR, CLOSE_CODE.UNSPECIFIED, err.message))
      return
    }
    const conn = this._newConn(id, opened.socket, {
      requestId: b.requestId,
      topic,
      identity: opened.identity
    })
    conn.openTimer = setTimeout(() => {
      if (conn.opened) return
      conn.timedOut = true
      conn.socket.destroy()
    }, OPEN_TIMEOUT_MS)
  }

  /** Inbound connection from a swarm (server side, or a Hyperswarm auto-dial). */
  onswarmconnection(socket, info) {
    if (this.closed || this.peerHello === null) {
      socket.destroy()
      return
    }
    const id = makeConnId(true, this.worker, this.nextInbound++)
    this._newConn(id, socket, { requestId: 0, topic: info.topic, identity: info.identity })
  }

  _closeReason(conn) {
    const err = conn.lastError
    const code = err && err.code
    if (this.closed) return [CLOSE_REASON.SHUTDOWN, CLOSE_CODE.UNSPECIFIED, 'bridge shutdown']
    if (conn.timedOut) return [CLOSE_REASON.TIMEOUT, CLOSE_CODE.TIMEOUT, 'open timed out']
    const msg = err ? `${code || 'ERROR'}: ${err.message}` : ''
    if (!conn.opened && conn.socket && conn.socket.remotePublicKey &&
        this.swarm.registry.recentlyRejected(conn.socket.remotePublicKey)) {
      return [CLOSE_REASON.FIREWALL, CLOSE_CODE.MID_REJECTED, 'hugin mid rejected' + (msg ? ` (${msg})` : '')]
    }
    if (code === 'HUGIN_MID_REJECTED') return [CLOSE_REASON.FIREWALL, CLOSE_CODE.MID_REJECTED, msg]
    if (code === 'PEER_NOT_FOUND') return [CLOSE_REASON.ERROR, CLOSE_CODE.PEER_NOT_FOUND, msg]
    if (code === 'PEER_CONNECTION_FAILED') return [CLOSE_REASON.ERROR, CLOSE_CODE.PEER_CONNECTION_FAILED, msg]
    if (code && code.startsWith('HOLEPUNCH')) return [CLOSE_REASON.ERROR, CLOSE_CODE.HOLEPUNCH_FAILED, msg]
    if (code && code.includes('TIMEOUT')) return [CLOSE_REASON.TIMEOUT, CLOSE_CODE.TIMEOUT, msg]
    if (!err) return [CLOSE_REASON.NORMAL, conn.opened ? CLOSE_CODE.REMOTE_CLOSED : 0, '']
    if (/Duplicate connection/.test(err.message)) return [CLOSE_REASON.NORMAL, CLOSE_CODE.UNSPECIFIED, msg]
    return [CLOSE_REASON.ERROR, CLOSE_CODE.UNSPECIFIED, msg]
  }

  _onsocketclose(conn) {
    if (conn.openTimer) clearTimeout(conn.openTimer)
    if (conn.closeSent) {
      this._maybeRelease(conn)
      return
    }
    // Inbound sockets that never opened were never announced to the client: just forget them.
    if (!conn.opened && connIdFromBare(conn.id)) {
      this.conns.delete(conn.id)
      return
    }
    const [reason, code, message] = this._closeReason(conn)
    conn.closeSent = true
    this._send(frames.close(conn.id, reason, code, message))
    this._maybeRelease(conn)
  }

  _closeFromBridge(conn, reason, code, message) {
    if (conn.closeSent) return
    conn.closeSent = true
    conn.queue = []
    this._send(frames.close(conn.id, reason, code, message))
    if (conn.socket) conn.socket.destroy()
    this._maybeRelease(conn)
  }

  _onclosefromrust(f) {
    const conn = this.conns.get(f.connId)
    if (!conn) return this._unknownConn(f.connId)
    conn.closeReceived = true
    if (!conn.closeSent) {
      conn.closeSent = true
      conn.queue = []
      if (conn.socket) {
        if (f.body.reason === CLOSE_REASON.NORMAL) conn.socket.end()
        conn.socket.destroy()
      }
      this._send(frames.close(conn.id, CLOSE_REASON.NORMAL, 0, ''))
    }
    this._maybeRelease(conn)
  }

  _maybeRelease(conn) {
    if (conn.closeSent && conn.closeReceived) this.conns.delete(conn.id)
  }

  // ---- topics -----------------------------------------------------------------------------

  _onannounce(b) {
    const hugin = huginOpts(b.tlvs)
    if (b.mode < 1 || b.mode > 3) {
      this._send(frames.error(b.requestId, ERROR_CODE.BAD_FRAME, 'mode must be 1, 2 or 3'))
      return
    }
    this.swarm.announce(b.topic, b.mode, hugin).then(
      (publicKey) => this._send(frames.ack(b.requestId, [{ tag: TLV.PUBLIC_KEY, value: publicKey }])),
      (err) => this._send(frames.error(b.requestId, ERROR_CODE.DHT_ERROR, err.message))
    )
  }

  _onunannounce(b) {
    this.swarm.unannounce(b.topic).then(
      () => this._send(frames.ack(b.requestId)),
      (err) => this._send(frames.error(b.requestId, ERROR_CODE.DHT_ERROR, err.message))
    )
  }

  _onlookup(b) {
    const max = b.maxPeers || 0xffff
    this.swarm.lookup(b.topic, max).then(
      (peers) => {
        const entries = peers.map((p) => {
          const r = p.relayAddresses && p.relayAddresses[0]
          return { publicKey: p.publicKey, addr: addrFromHost(r && r.host, r && r.port) }
        })
        if (entries.length === 0) {
          this._send(frames.lookupResult(b.requestId, [], true))
          return
        }
        for (let i = 0; i < entries.length; i += LOOKUP_CHUNK) {
          const chunk = entries.slice(i, i + LOOKUP_CHUNK)
          this._send(frames.lookupResult(b.requestId, chunk, i + LOOKUP_CHUNK >= entries.length))
        }
      },
      (err) => this._send(frames.error(b.requestId, ERROR_CODE.DHT_ERROR, err.message))
    )
  }

  // ---- peer info --------------------------------------------------------------------------

  _peerInfo(conn) {
    const s = conn.socket
    const raw = s.rawStream
    const rtt = raw && typeof raw.rtt === 'number' ? raw.rtt : 0
    return {
      request: false,
      rttUs: Math.min(0xffffffff, Math.max(0, Math.round(rtt * 1000))),
      bytesIn: BigInt(s.rawBytesRead || 0),
      bytesOut: BigInt(s.rawBytesWritten || 0),
      relayed: false,
      holepunched: false,
      addr: addrFromHost(raw && raw.remoteHost, raw && raw.remotePort)
    }
  }

  _onpeerinforequest(f) {
    const conn = this.conns.get(f.connId)
    if (!conn || !conn.socket) return this._unknownConn(f.connId)
    if (!conn.opened || conn.closeSent) return
    this._send(frames.peerInfo(conn.id, this._peerInfo(conn)))
  }

  _broadcastPeerInfo() {
    for (const conn of this.conns.values()) {
      if (conn.opened && !conn.closeSent) this._send(frames.peerInfo(conn.id, this._peerInfo(conn)))
    }
  }

  // ---- teardown ---------------------------------------------------------------------------

  _onclose() {
    this.closed = true
    clearTimeout(this._helloTimer)
    clearInterval(this._keepalive)
    if (this._peerInfoTimer) clearInterval(this._peerInfoTimer)
    for (const conn of this.conns.values()) {
      if (conn.openTimer) clearTimeout(conn.openTimer)
      if (conn.socket) conn.socket.destroy()
    }
    this.conns.clear()
    this.ctx.onsessionclose(this)
  }
}
