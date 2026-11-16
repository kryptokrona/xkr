// Pieces xkrd's supervisor relies on (crates/xkrd/src/hyper.rs): `--max-peers` reaches the plain
// Hyperswarm of a worker, and the local DHT bootstrap helper can run detached from stdin for
// shell scripts. No public DHT traffic: every DHT here bootstraps from an unreachable or local
// address.

import test from 'node:test'
import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import crypto from 'node:crypto'
import { parseWorkerArgs } from '../src/worker.js'
import { SwarmManager } from '../src/swarm.js'
import { keyPairFromSeed } from '../src/mid.js'

const BOOTSTRAP = fileURLToPath(new URL('./testnet-bootstrap.js', import.meta.url))
const SEED = '07'.repeat(32)

function within(promise, ms, what) {
  let timer
  return Promise.race([
    promise,
    new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error(`timeout: ${what}`)), ms)
    })
  ]).finally(() => clearTimeout(timer))
}

function killLater(t, child) {
  t.after(() => {
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
  })
}

test('--max-peers is parsed and validated', () => {
  assert.equal(parseWorkerArgs(['--seed', SEED]).maxPeers, 0)
  assert.equal(parseWorkerArgs(['--seed', SEED, '--max-peers', '24']).maxPeers, 24)
  assert.throws(() => parseWorkerArgs(['--seed', SEED, '--max-peers', '0']), /invalid --max-peers/)
  assert.throws(() => parseWorkerArgs(['--seed', SEED, '--max-peers', 'many']), /invalid --max-peers/)
  assert.throws(() => parseWorkerArgs(['--seed', SEED, '--max-peers']), /missing value/)
})

test('maxPeers reaches the plain swarm only', async () => {
  const keyPair = keyPairFromSeed(crypto.randomBytes(32))
  const sm = new SwarmManager({
    keyPair,
    bootstrap: [{ host: '127.0.0.1', port: 9 }],
    maxPeers: 7,
    onconnection: (socket) => socket.destroy()
  })
  try {
    assert.equal(sm.plain.maxPeers, 7)
    const capped = new SwarmManager({
      keyPair,
      bootstrap: [{ host: '127.0.0.1', port: 9 }],
      onconnection: (socket) => socket.destroy()
    })
    try {
      assert.equal(capped.plain.maxPeers, 64, 'Hyperswarm default without --max-peers')
    } finally {
      await capped.destroy()
    }
  } finally {
    await sm.destroy()
  }
})

test('testnet-bootstrap --ignore-stdin keeps running without stdin and stops on SIGTERM', async (t) => {
  const child = spawn(process.execPath, [BOOTSTRAP, '--ignore-stdin'], { stdio: ['ignore', 'pipe', 'inherit'] })
  killLater(t, child)
  const exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)))
  const line = await within(new Promise((resolve) => {
    let buf = ''
    child.stdout.on('data', (d) => {
      buf += d
      if (buf.includes('\n')) resolve(buf.split('\n')[0])
    })
  }), 10000, 'BOOTSTRAP line')
  assert.match(line, /^BOOTSTRAP 127\.0\.0\.1:\d+$/)
  await new Promise((resolve) => setTimeout(resolve, 500))
  assert.equal(child.exitCode, null, 'must not stop although stdin is /dev/null')
  child.kill('SIGTERM')
  assert.equal(await within(exited, 8000, 'bootstrap exit'), 0)
})
