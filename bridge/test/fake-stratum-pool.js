#!/usr/bin/env node
// A minimal CryptoNote stratum pool for local Hugin relay tests (testnet/hugin-relay-test.sh):
// answers `login` with a session and a job, pushes a fresh job every --job-interval seconds,
// answers `submit` (checking the share with hugin-utils' verifyShare when HUGIN_NODE_MODULES is
// set) and `keepalived`. Prints
//   POOL 127.0.0.1:<port>
// as its first stdout line, then one line per event (LOGIN <login>, SUBMIT <ok|bad> <job> <nonce>).
// Never talks to the network beyond 127.0.0.1.
//
// usage: node bridge/test/fake-stratum-pool.js [--port <n>] [--target <8 hex>] [--job-interval <s>]

import net from 'node:net'
import path from 'node:path'
import process from 'node:process'
import { createRequire } from 'node:module'

let port = 0
let target = '285c8f02' // difficulty 100: the least job difficulty relays accept in gossip
let jobInterval = 0
const args = process.argv.slice(2)
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--port') port = Number(args[++i])
  else if (args[i] === '--target') target = args[++i]
  else if (args[i] === '--job-interval') jobInterval = Number(args[++i])
}

let verifyShare = null
if (process.env.HUGIN_NODE_MODULES) {
  try {
    const require = createRequire(import.meta.url)
    verifyShare = require(path.join(process.env.HUGIN_NODE_MODULES, 'hugin-utils')).verifyShare
  } catch {
    verifyShare = null
  }
}

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

let height = 1000
let jobs = []
function makeJob() {
  height++
  const prev = Buffer.alloc(32)
  prev.writeUInt32LE(height, 0)
  // major 5, minor 0, timestamp, prev_id, nonce, merkle root, tx count (the shape hugin-utils
  // parses: nonce right after prev_id)
  const blob = Buffer.concat([
    varint(5),
    varint(0),
    varint(Math.floor(Date.now() / 1000)),
    prev,
    Buffer.alloc(4),
    Buffer.alloc(32, 0x5a),
    varint(1)
  ])
  const job = { job_id: `sim-${height}`, blob: blob.toString('hex'), target, height }
  jobs.push(job)
  if (jobs.length > 8) jobs = jobs.slice(-8)
  return job
}
let current = makeJob()
const sockets = new Set()

function send(sock, obj) {
  sock.write(JSON.stringify(obj) + '\n')
}

const server = net.createServer((sock) => {
  sockets.add(sock)
  sock.on('close', () => sockets.delete(sock))
  sock.on('error', () => {})
  let buf = ''
  sock.setEncoding('utf8')
  sock.on('data', async (d) => {
    buf += d
    let i
    while ((i = buf.indexOf('\n')) !== -1) {
      const line = buf.slice(0, i)
      buf = buf.slice(i + 1)
      if (!line.trim()) continue
      let req
      try {
        req = JSON.parse(line)
      } catch {
        continue
      }
      const { id, method, params } = req
      if (method === 'login') {
        process.stdout.write(`LOGIN ${params && params.login}\n`)
        send(sock, { id, jsonrpc: '2.0', error: null, result: { id: 'sim-session', job: current, status: 'OK' } })
      } else if (method === 'submit') {
        const job = jobs.find((j) => j.job_id === params.job_id)
        let ok = !!job
        if (ok && verifyShare) ok = await verifyShare(job, params.nonce, params.result)
        process.stdout.write(`SUBMIT ${ok ? 'ok' : 'bad'} ${params.job_id} ${params.nonce}\n`)
        if (ok) send(sock, { id, jsonrpc: '2.0', error: null, result: { status: 'OK' } })
        else send(sock, { id, jsonrpc: '2.0', error: { code: -1, message: 'Invalid share' } })
      } else {
        send(sock, { id, jsonrpc: '2.0', error: null, result: { status: 'KEEPALIVED' } })
      }
    }
  })
})

server.listen(port, '127.0.0.1', () => {
  process.stdout.write(`POOL 127.0.0.1:${server.address().port}\n`)
})

if (jobInterval > 0) {
  setInterval(() => {
    current = makeJob()
    for (const s of sockets) send(s, { jsonrpc: '2.0', method: 'job', params: current })
  }, jobInterval * 1000)
}

function stop() {
  for (const s of sockets) s.destroy()
  server.close()
  process.exit(0)
}
process.on('SIGINT', stop)
process.on('SIGTERM', stop)
