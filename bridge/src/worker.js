// Entry of one Bare worker of xkrd's Hyperswarm bridge (docs/hyper-transport.md). xkrd starts it
// through bare-kit 3.x's C API (bare_worklet_start) on a thread of its own and talks to it over
// Bare.IPC with the frames of docs/hyper-bridge-protocol.md. xkrd reads or creates the key pair
// file and passes the seed.
//
// Arguments (Bare.argv, after the marker '--xkr-worker'; bare-kit may put entries before it):
//   --worker <i> --workers <n>   this worker's index and the worker count (Hello, shard bits)
//   --seed <64 hex>              seed of this worker's DHT key pair
//   --bootstrap <host:port>      DHT bootstrap node, repeatable
//   --port-fixed <udp port>      fixed UDP port of this worker's DHT node (xkrd --port); every
//                                swarm of the worker (plain and Hugin identities) uses it
//   --max-peers <n>              plain-swarm connection cap (worker 0 only)
//   --max-frame <bytes>, --initial-credit <bytes>, --peer-info-interval <s>
//   --log-json, --log-level <level>
//
// No Node-only APIs (runs on Bare only; `Bare.IPC` is the duplex stream bare-kit's worklet sets
// up for the host's IPC).

/* global Bare */

import { failWorker } from './worker-guard.js'
import b4a from 'b4a'
import { DEFAULT_MAX_FRAME, DEFAULT_INITIAL_CREDIT, MIN_MAX_FRAME } from './frames.js'
import { SwarmManager } from './swarm.js'
import { keyPairFromSeed } from './mid.js'
import { Session, parseHostPort } from './session.js'

export const ARGS_MARKER = '--xkr-worker'

export function parseWorkerArgs(argv) {
  const opts = {
    worker: 0,
    workers: 1,
    seed: null,
    bootstrap: [],
    portFixed: 0,
    maxPeers: 0,
    maxFrame: DEFAULT_MAX_FRAME,
    initialCredit: DEFAULT_INITIAL_CREDIT,
    peerInfoInterval: 30,
    logJson: false,
    logLevel: 'info'
  }
  const int = (name, v, min, max) => {
    const n = Number(v)
    if (!Number.isInteger(n) || n < min || n > max) throw new Error(`invalid ${name}: ${v}`)
    return n
  }
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`missing value for ${a}`)
      return argv[++i]
    }
    switch (a) {
      case '--worker': opts.worker = int('--worker', next(), 0, 0x7ffe); break
      case '--workers': opts.workers = int('--workers', next(), 1, 0x7fff); break
      case '--seed': {
        const hex = next()
        if (!/^[0-9a-fA-F]{64}$/.test(hex)) throw new Error('--seed must be 64 hex characters')
        opts.seed = b4a.from(hex, 'hex')
        break
      }
      case '--bootstrap': opts.bootstrap.push(parseHostPort(next())); break
      case '--port-fixed': opts.portFixed = int('--port-fixed', next(), 1, 65535); break
      case '--max-peers': opts.maxPeers = int('--max-peers', next(), 1, 65535); break
      case '--max-frame': opts.maxFrame = int('--max-frame', next(), MIN_MAX_FRAME, 0xffffffff); break
      case '--initial-credit': opts.initialCredit = int('--initial-credit', next(), 1, 0x7fffffff); break
      case '--peer-info-interval': opts.peerInfoInterval = int('--peer-info-interval', next(), 0, 86400); break
      case '--log-json': opts.logJson = true; break
      case '--log-level': opts.logLevel = next(); break
      default: throw new Error(`unknown argument ${a}`)
    }
  }
  if (opts.worker >= opts.workers) throw new Error(`--worker ${opts.worker} is not below --workers ${opts.workers}`)
  if (opts.seed === null) throw new Error('--seed is required')
  return opts
}

const LEVELS = { debug: 10, info: 20, warn: 30, error: 40 }

/** Log lines go to console.error, which bare-kit sends to the system log (bare-system-logger). */
function makeLogger(json, level, worker) {
  const min = LEVELS[level] ?? LEVELS.info
  return (lvl, msg, fields = {}) => {
    if ((LEVELS[lvl] ?? 20) < min) return
    if (json) {
      console.error(JSON.stringify({ ts: new Date().toISOString(), level: lvl, worker, msg, ...fields }))
    } else {
      const extra = Object.entries(fields).map(([k, v]) => `${k}=${v}`).join(' ')
      console.error(`[hyper-bridge ${worker}] ${lvl} ${msg}${extra ? ' ' + extra : ''}`)
    }
  }
}

/**
 * Runs one worker: a SwarmManager with this worker's key pair and one Session over `ipc`. When
 * xkrd ends its side of the IPC, this side ends too, the session closes and the swarm leaves the
 * DHT; with nothing left to do the worklet exits. xkrd terminates it if that takes more than 2 s.
 */
export function startWorker(ipc, argv) {
  const opts = parseWorkerArgs(argv)
  const log = makeLogger(opts.logJson, opts.logLevel, opts.worker)
  const keyPair = keyPairFromSeed(opts.seed)
  const ctx = {
    opts,
    log,
    worker: opts.worker,
    workers: opts.workers,
    swarm: null,
    session: null,
    onsessionclose: null
  }
  ctx.swarm = new SwarmManager({
    keyPair,
    bootstrap: opts.bootstrap,
    port: opts.portFixed,
    maxPeers: opts.maxPeers,
    log,
    onconnection: (socket, info) => {
      if (ctx.session) ctx.session.onswarmconnection(socket, info)
      else socket.destroy()
    },
    onstatus: (status) => {
      if (ctx.session) ctx.session.sendDhtStatus(status)
    }
  })
  ctx.onsessionclose = (session) => {
    if (ctx.session !== session) return
    ctx.session = null
    log('info', 'link closed; leaving the DHT')
    ctx.swarm.destroy().catch((err) => log('warn', 'swarm destroy failed', { err: err.message }))
  }
  ctx.session = new Session(ipc, ctx)
  // The host ended its side (a half-close): end ours, so the stream closes and the session with it.
  ipc.on('end', () => ipc.end())
  log('info', 'worker started', {
    worker: opts.worker,
    workers: opts.workers,
    publicKey: b4a.toString(keyPair.publicKey, 'hex')
  })
  ctx.swarm.ready().then(
    () => {
      const st = ctx.swarm.status()
      log('info', 'dht bootstrapped', { udpPort: st.udpPort, firewalled: st.firewalled, nat: st.nat })
    },
    (err) => {
      log('warn', 'dht bootstrap failed', { err: err.message, portFixed: opts.portFixed })
      ctx.swarm.markFailed()
    }
  )
  return ctx
}

if (typeof Bare !== 'undefined' && Bare && Bare.IPC) {
  const argv = Array.isArray(Bare.argv) ? Bare.argv : []
  const at = argv.indexOf(ARGS_MARKER)
  try {
    startWorker(Bare.IPC, at < 0 ? [] : argv.slice(at + 1))
  } catch (err) {
    // A bad argument comes from xkrd; the worker tells xkrd why and ends.
    failWorker('worker failed to start', err)
  }
}
