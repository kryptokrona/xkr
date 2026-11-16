#!/usr/bin/env node
// Starts a small local HyperDHT testnet (hyperdht/testnet) on 127.0.0.1 and prints
//   BOOTSTRAP 127.0.0.1:<port>
// as the first stdout line. Runs until SIGINT/SIGTERM or until stdin closes, so a test harness
// that spawns it with a piped stdin cleans it up automatically. With --ignore-stdin it only stops
// on a signal (for shell scripts that start it in the background, where stdin is /dev/null).
//
// usage: node bridge/test/testnet-bootstrap.js [--size <n>] [--ignore-stdin]

import createTestnet from 'hyperdht/testnet.js'
import process from 'node:process'

let size = 3
let watchStdin = true
const args = process.argv.slice(2)
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--size') size = Number(args[++i])
  else if (args[i] === '--ignore-stdin') watchStdin = false
}

const testnet = await createTestnet(size, { host: '127.0.0.1' })
const b = testnet.bootstrap[0]
process.stdout.write(`BOOTSTRAP ${b.host}:${b.port}\n`)

let stopping = false
async function stop() {
  if (stopping) return
  stopping = true
  setTimeout(() => process.exit(0), 3000).unref()
  await testnet.destroy()
  process.exit(0)
}

process.on('SIGINT', stop)
process.on('SIGTERM', stop)
if (watchStdin) {
  process.stdin.on('end', stop)
  process.stdin.on('close', stop)
  process.stdin.on('error', stop)
  process.stdin.resume()
}
