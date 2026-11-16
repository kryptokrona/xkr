# xkr hyper-bridge

The JavaScript half of xkrd's Hyperswarm/HyperDHT transport. It owns HyperDHT, holepunching,
UDX and secret-stream, and speaks the binary frame protocol of
[`docs/hyper-bridge-protocol.md`](../docs/hyper-bridge-protocol.md) to the Rust side
(`crates/xkr-p2p-hyper`).

xkrd runs `src/worker.js` on Bare worker threads inside its own process through bare-kit, with
frames over bare-kit's IPC; no Node.js. Build `libbare-kit` once with
`scripts/build-bare-kit.sh`, then cargo as usual
([`docs/hyper-transport.md`](../docs/hyper-transport.md), "Building"). xkrd needs this directory
with its `node_modules` (`sfw npm ci`). Everything else (chain logic, peer policy, Hugin relay
logic) lives in Rust. See [`docs/hyper-transport.md`](../docs/hyper-transport.md) for how xkrd
uses it.

## Worker arguments

xkrd (`xkr_p2p_hyper::Bridge::start_in_process`) passes each worker its arguments in
`Bare.argv`, after the marker `--xkr-worker`:

| option | meaning |
|---|---|
| `--worker <i>`, `--workers <n>` | this worker's index and the worker count (`Hello`, shard bits of `conn_id`) |
| `--seed <64 hex>` | seed of this worker's DHT key pair (xkrd reads or creates the key pair file and passes the seed in memory) |
| `--bootstrap host:port` | DHT bootstrap node, repeatable (default: hyperdht's public bootstrap nodes) |
| `--port-fixed <udp>` | bind the worker's one DHT UDP socket to exactly this port (xkrd `--port`; worker `i` of several gets port + `i`). Every swarm of the worker, the plain one and each Hugin identity, listens through it. A taken port is not replaced by another one: the worker reports `DhtStatus` state 2 (failed). Holepunching needs no fixed port |
| `--max-peers <n>` | connection cap of the plain swarm (Hyperswarm `maxPeers`, default 64; worker 0 only); xkrd passes `--hyper-max-peers` |
| `--max-frame`, `--initial-credit` | proposals for the `Hello` negotiation |
| `--peer-info-interval <s>` | unsolicited `PeerInfo` period (default 30, 0 = off) |
| `--log-json`, `--log-level` | logging (to `console.error`, which bare-kit sends to the system log) |

When xkrd ends its side of the IPC, the worker ends its side too, the session closes and the
swarm leaves the DHT; xkrd terminates the worklet if that takes more than 2 s. The Rust side must
send its `Hello` within 10 s.

After the Rust side's `Hello` the worker sends a `DhtStatus` frame (0x41, always `IGNORABLE`)
with its DHT node's state, whether it is firewalled, what the DHT nodes saw of its NAT and the
local UDP port, and again whenever one of them changes (DHT events `listening`, `ready`,
`nat-update`, `persistent`, `network-update`, and a check every 60 s); xkrd shows it in `/status`
`hyper` and the console's `status` (`docs/hyper-bridge-protocol.md`, "DhtStatus").

xkrd runs one worker: the XKR topics and the Hugin topics (and later p2pool) share its DHT node
and so one UDP port.

## Files

| file | content | runtime |
|---|---|---|
| `src/frames.js` | frame codec, constants, `FrameDecoder` | Bare + Node (tests) |
| `src/mid.js` | Hugin `mid` layer on top of upstream hyperdht; Hugin topic/`mid` derivation (domain separators `HUGIN_TOPIC_DOMAIN`, `HUGIN_MID_DOMAIN`) | Bare + Node (tests) |
| `src/keccak.js` | Keccak-256 with the original padding (`cn_fast_hash`) for the Hugin topic, no dependency | Bare + Node (tests) |
| `src/swarm.js` | shared DHT, plain swarm, one swarm per Hugin identity | Bare + Node (tests) |
| `src/session.js` | one Rust client session: the frame protocol, credits, connections, worker index (`worker` of `workers`) | Bare + Node (tests) |
| `src/worker.js` | the worker entry: runs a `Session` over `Bare.IPC` inside a bare-kit 3.x worklet, arguments after `--xkr-worker` in `Bare.argv`; `startWorker`/`parseWorkerArgs` are importable for the tests | Bare (bare-kit) + Node (tests) |
| `src/worker-guard.js` | loaded first by `worker.js`: an uncaught error ends only that worker | Bare |
| `test/*.test.js` | `node --test` suites | Node |
| `test/vectors.js` | hand-written frames for the shared golden vectors | Bare + Node (tests) |
| `test/testnet-bootstrap.js` | local HyperDHT testnet for the Rust integration test and the testnet scenarios 20 and 40 (`--ignore-stdin` when started from a shell) | Node |
| `test/hugin-vectors.js` | regenerates `docs/test-vectors/hugin-relay.json` (bare-rpc frames, PoW shares, `pow.auth` signatures, node identity) with the packages hugin-node uses: `HUGIN_NODE_MODULES=… node test/hugin-vectors.js --write` (never redirect stdout onto the file). The `hugin_derivation` section is written by `XKR_WRITE_VECTORS=1 cargo test -p xkr-p2p-hyper --test hugin_derivation_vectors`; this script checks it against kryptokrona-utils and libsodium and carries it over | Node |
| `test/hugin-client-sim.js` | a Hugin app (bare-rpc, keypear, hugin-utils, kryptokrona-utils, and this bridge's own `mid` layer for the swarm side) that connects to a hugin-node/xkrd relay, posts a paid DM, syncs it and waits for the push; prints `PASS`/`FAIL` (`docs/hugin-relay.md` §12); `--testnet` joins the testnet Hugin topics (§14); needs only `HUGIN_NODE_MODULES` | Node |
| `test/fake-stratum-pool.js` | a local stratum pool for the Hugin relay test (`login`/`job`/`submit`, difficulty 1) | Node |

## Hugin `mid` layer

### What Hugin does

The handshake format was verified against `hyperdht-hugin` 1.0.3 / `hyperswarm-hugin` 1.0.4
(diffed against upstream hyperdht 6.36.0 / hyperswarm 4.17.2), hugin-node and hugin-native; the
topic and `mid` derivation is the one of hugin-network 3.0 (domain-separated):

- The Noise handshake payload (`messages.noisePayload`) gets one extra field: flag bit 128 and
  a `fixed64` **after** all upstream fields. Byte layout otherwise identical to upstream.
- The base key pair is `crypto_sign_seed_keypair(Buffer.alloc(32).fill(key))`. The DHT topic is
  `cn_fast_hash("hugin/topic" || basePublicKey)` (Keccak-256, original padding, 43 bytes in;
  `src/keccak.js`), and `mid = crypto_sign_detached("hugin/mid" || own DHT public key,
  baseSecretKey)` (41 bytes signed). hugin-node's three swarms use the keys
  `hex(cn_fast_hash(networkAddress))` (node mesh), `networkAddress` (public nodes) and the
  node's XKR address (private node); clients use the same derivation. The constants are in
  `src/mid.js` and `crates/xkr-p2p-hyper/src/topic.rs`; vectors in
  `docs/test-vectors/hugin-relay.json` (`hugin_derivation`).
- A peer whose `mid` is missing or does not verify against the base public key over exactly
  `"hugin/mid" || its DHT public key` is refused, on both the server and the client side.
- **Clients using the earlier derivation cannot reach us.** hyperswarm-hugin (hugin-node 2.x
  and the apps before hugin-network 3.0) used `topic = Buffer.alloc(32).fill(hex(basePublicKey))`
  and signed the bare DHT public key: they neither find our topics nor pass our `mid` check, and
  they refuse ours. This is a one-time breaking change by design; there is no legacy mode and
  no option for one.
- One `mid` per DHT key pair, so every Hugin swarm has its own DHT key pair.

### What we patch, and how

Nothing in `node_modules` is modified. `src/mid.js`:

1. **`midNoisePayload`** wraps upstream `require('hyperdht/lib/messages.js').noisePayload`:
   encode/decode the upstream fields with the upstream codec, then OR bit 128 into the flags
   byte and append/read the 64-byte `mid`. (Upstream never sets more than bits 0..6 and the
   largest flags value with `mid`, 251, is still one compact-encoding byte, so this is exactly
   what the fork's encoder emits; `test/mid.test.js` checks byte equality against vectors
   produced by the fork.)
2. **`MidNoiseWrap extends NoiseWrap`** (`hyperdht/lib/noise-wrap.js`) uses that codec. It
   looks up the *local* key pair of the handshake in a `MidRegistry`: registered Hugin
   identities send their `mid` and verify the remote one (throwing on failure, which makes
   hyperdht drop the handshake on the server side and fail the connect on the client side);
   any other key pair produces a byte-identical upstream handshake.
3. **`MidDHT extends HyperDHT`** passes `createHandshake` (a public option of both
   `dht.connect()` and `dht.createServer()` in hyperdht 6.x) to every connect and server, so
   all Hyperswarm instances built on this DHT get the behaviour.

`src/swarm.js` runs one `MidDHT` (one UDP socket, so `--port-fixed` works), a plain Hyperswarm
with the node key for topics without `mid`, and one Hyperswarm per Hugin topic with its own key
pair (derived from the node key and the topic, or from TLV `0x12`). Hyperswarm has one key pair
per swarm, so each identity needs a swarm of its own; every swarm is built as
`new Hyperswarm({ dht, keyPair })` on the shared DHT, so they all use its UDP socket. The `mid` is added only when
an `Announce`/`Open` carries TLV `0x10`/`0x11` or targets a topic that already has an identity;
plain upstream peers keep working on every other topic. With TLV `0x11` the topic must be the
domain-separated one of that topic key (an earlier hex-fill topic is refused), and a
precomputed `0x10` next to it must verify for the identity's key.

Upgrade checklist (re-run `npm test` and the separation test): `noisePayload` field order and flag
bits, `NoiseWrap` constructor/`send`/`recv`, the `createHandshake` option, and the Hyperswarm
internals `swarm._timer` / `swarm._allConnections` used by `closeSwarmKeepDht()` (needed
because `Hyperswarm#destroy()` would also destroy the shared DHT).

### Evidence

- `test/mid.test.js`: payload bytes identical to the fork's encoder; Keccak known answers;
  topic keys and base keys match hugin-node's code path (kryptokrona-utils + keypear), topics
  are `cn_fast_hash("hugin/topic" || base)` and not the earlier hex-fill ones; the
  `hugin_derivation` vectors (written by Rust) check out with libsodium; an old-scheme `mid` is
  refused by `verifyMid` and in a handshake; over a local testnet two bridge identities connect,
  a mid-less peer and a peer with an old-scheme `mid` are refused, and the bridge refuses the
  earlier topic.
- `test/hugin-interop.test.js`, a **separation** test (skipped unless the old forks are
  available): a real `hyperswarm-hugin` peer built like hugin-node 2.x's `swarm()` gets no
  connection to us in either direction (on our topic each side refuses the other's `mid`; on
  its own topic it never meets us). Run with

  ```
  HUGIN_FORKS_NODE_MODULES=<dir with hyperswarm-hugin and hyperdht-hugin> \
  KEYPEAR_DIR=<path to keypear> node --test test/hugin-interop.test.js
  ```

## Tests

```
cd bridge
node --test test/*.test.js        # or: npm test
```

`frames.test.js` (golden vectors shared with Rust), `mid.test.js` (Hugin layer + local
testnet), `session.test.js` (two workers, `startWorker` of `worker.js`, on a local testnet over
in-memory streams, driven by a minimal JS client: Hello, Announce, Lookup, Open, Data, credits
incl. the oversized-message rule, PeerInfo, two-way Close, UNKNOWN_CONN, failing Open,
UNKNOWN_TYPE; `--port-fixed` binding exactly that port with `DhtStatus` ready, a second worker on
the same port reporting failed; a plain topic and a Hugin identity on one worker's DHT announced,
looked up and dialed), `lifecycle.test.js` (`--max-peers`, `testnet-bootstrap.js --ignore-stdin`),
`worker.test.js` (`worker.js` arguments, the worker index in `Hello`, `header.shard` and inbound
`conn_id`s, `DhtStatus` only after the client's `Hello` and once per state, with a fake IPC). They run under Node, which runs the same modules as Bare. None of
them touch the public DHT.

## Packaging

`xkrd`, `libbare-kit` next to it and this directory (sources and `node_modules`); no Node.js.
`packaging/scripts/bundle-bridge.sh` makes the shipped copy (sources plus
`npm ci --omit=dev --ignore-scripts`, optionally pruned to one platform's prebuilds); the release
archives put it next to `xkrd`, and the Tauri sample (`packaging/tauri-sidecar/`) ships it as a
resource (`packaging/README.md`).

`src/frames.js`, `src/mid.js`, `src/swarm.js`, `src/session.js` and `src/worker.js` use only
`b4a`, `sodium-universal`, `compact-encoding`, typed arrays and the Holepunch packages, all of
which run on Bare. Tests use `node:test`, `node:child_process` and `node:crypto` and stay
Node-only.

## Known limitations

- Explicit `Open`s use `dht.connect()` directly and skip Hyperswarm's duplicate-connection
  handling; if a peer is also auto-dialled through a topic joined in client mode, Rust may see
  two connections to the same key and should keep one (it has the remote key in `Opened`).
- `open_flags` bit0 (allow relay) has no effect until a blind relay is configured.
- `PeerInfo.relayed` / `holepunched` are always 0 (hyperdht does not expose them on the
  stream); `rtt_us` is UDX's smoothed RTT.
- `LookupResult` addresses are relay hints (DHT nodes), not peer addresses.
