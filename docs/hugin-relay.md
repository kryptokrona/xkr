# Hugin relay: hugin-node in xkrd

Every xkrd with the Hyperswarm transport is a hugin-node unless it is started with
`--no-hugin` or with mode flags without `--hugin` (`--node --hugin` is the node and the relay): Hugin apps (hugin-desktop, hugin-native) can use it as their node, and hugin-node peers accept it in their mesh, provided they use the topic and `mid` derivation of hugin-network 3.0 (domain-separated, §2);
clients using the earlier derivation cannot reach the relay. Code: `crates/xkr-hugin` (protocol, PoW, pool, relay), `crates/xkrd/src/hyper.rs`
(topics, routing). Transport: [`hyper-transport.md`](hyper-transport.md), Hugin `mid`:
[`hyper-bridge-protocol.md`](hyper-bridge-protocol.md) §6 and `bridge/README.md`.

Sections 1–9 describe hugin-node 2.0.0 and the apps as they are (sources:
hugin-node, github.com/kryptokrona/hugin-node, `index.js network.js nodes.js pools.js storage.js
utils.js constants.js id.js setup.js`, hugin-desktop `src/backend/swarm.cjs messages.cjs
crypto.cjs`, hugin-native `bare/swarm.js src/services/hugin/syncer.js`, and the packages
`bare-rpc` 1.3.3, `hugin-utils` 0.4.0, `kryptokrona-utils`, `keypear`). Each point is marked
**confirmed** (read in the source, and for wire formats reproduced byte for byte with the
packages themselves, `docs/test-vectors/hugin-relay.json`) or **assumed**. Sections 10–15 are
our implementation and where it differs; §13 is the fee address and the start-up question, §14
the testnet topics.

## 1. What a hugin-node does

A store-and-forward relay for end-to-end encrypted Hugin DMs (and call/room signalling and
push registrations) on the Hyperswarm DHT. It never decrypts anything and does not use the
blockchain. Apps pay for each message with proof-of-work **shares of a real XKR mining job**
that the node got from a stratum pool; the node submits those shares to the pool under its
operator's payout address, so the operator is paid in XKR by the pool. Accepted DMs are kept
24 h, pushed to connected apps every 5 s, offered to apps that sync, and gossiped one hop to
other nodes. (confirmed)

## 2. Topics and discovery

All three swarms carry a Hugin `mid` in the Noise handshake (flag 128, 64 bytes after the
upstream fields; `bridge/README.md`). A swarm is derived from a string `key`
(`get_new_peer_keys`) with the topic and `mid` derivation of hugin-network 3.0 (domain-separated):

```
topic_key = fill32(key)                                   Buffer.alloc(32).fill(key)
base      = crypto_sign_seed_keypair(topic_key)           ed25519
topic     = cn_fast_hash("hugin/topic" || base.publicKey)            11 + 32 = 43 bytes
mid       = sign_detached("hugin/mid" || own DHT public key, base.secretKey)   9 + 32 = 41 bytes
```

`cn_fast_hash` is Keccak-256 with the original padding; a remote `mid` is accepted only if it
verifies against `base.publicKey` over exactly `"hugin/mid" || remote DHT public key`. The two
domain separators are `HUGIN_TOPIC_DOMAIN` / `HUGIN_MID_DOMAIN` in
`crates/xkr-p2p-hyper/src/topic.rs` and `bridge/src/mid.js`; vectors in
`docs/test-vectors/hugin-relay.json` (`hugin_derivation`).

**Clients using the earlier derivation cannot reach the relay.** hyperswarm-hugin (hugin-node
2.x and the apps before hugin-network 3.0) used `topic = fill32(hex(base.publicKey))` and signed the
bare DHT public key. Such a client neither finds our topics on the DHT nor passes our `mid`
check, and it refuses ours, so it is separated at both layers, in both directions; this holds for
the node mesh as well (only updated hugin-nodes can be mesh peers). This was a one-time breaking
change by design (decided 2026-11-14): there is no legacy mode and no fallback. The inputs
(`key`, `topic_key`, base key pair, the keys below) and the bare-rpc protocol to clients are
unchanged.

| swarm | key | hugin-node join | DHT key pair | who connects |
|---|---|---|---|---|
| node mesh | `hex(cn_fast_hash(NETWORK_ADDRESS))` | server + client, `discovery.refresh` every 60 s | random per start | other (updated) hugin-nodes |
| public nodes | `NETWORK_ADDRESS` | server only (skipped when `config.private`) | `keyPair(sha256("hugin-node public" \|\| fill32(hex(cn_fast_hash(viewkey)))))` (updated hugin-node; the earlier one used the private node's key here) | any app ("public node" setting) |
| private node | the node's XKR address | server only, always | `keyPair(fill32(hex(cn_fast_hash(viewkey))))`, the second half of the node address | apps with the invite `address + hex(dht public key)` |

`NETWORK_ADDRESS = xkr96c0f8a36e951b399681d447922f6c54c28c6ef3cad1c65d3568008151337`.
Apps join the public or private topic as client only, `maxPeers` 1 (desktop) or 3 (native,
extra connections kept as backups). For a private node the app's firewall accepts only the key
in the last 64 characters of the invite; since `fill32` keeps 32 bytes, the full invite and the
bare address give the same topic. There is no other discovery: no HTTP, no seed list.
(confirmed)

## 3. Wire: bare-rpc frames with JSON

Every connection is wrapped in `new RPC(conn, onrequest)` (bare-rpc). Frames
(`bare-rpc/lib/messages.js`, all integers compact-encoding, little-endian):

```
frame    = len u32 (bytes after this field) | type uint | id uint | body
REQUEST  (1): command uint | stream uint | stream == 0: data (uint length + bytes)
RESPONSE (2): error bool | stream uint | error ? message utf8, code utf8, errno zigzag-int
                                       : stream == 0: data
STREAM   (3): stream uint | stream & 0x80 ? error : stream & 0x10 ? data : -
uint     = < 0xfd: 1 byte, 0xfd + u16, 0xfe + u32, 0xff + u64
```

Example (vector `request_version`): `18000000 01 01 01 00 13 7b2276657273696f6e223a22322e302e30227d`
= request id 1, command 1, stream 0, 19 bytes `{"version":"2.0.0"}`. Request ids count from 1
per side; id 0 is an event (no reply). (confirmed, byte-exact against bare-rpc 1.3.3)

Only one command exists, `PACKET = 1`. Each side sends packets as
`rpc.request(1).send(JSON.stringify(packet))` and answers **every** request with
`req.reply('{"ok":true,"data":null,"error":null}')`; replies are ignored
(`req.reply().catch(() => {})`). Answers to a packet travel as new requests that echo the
packet's `id` field in the JSON. A request whose `data` is longer than 9000 bytes
(`MAX_CLIENT_INBOUND_BYTES`/`MAX_NODE_INBOUND_BYTES`) or is not JSON (or JSON `null`/`false`/
`0`/`""`) gets the peer banned. There is no frame size limit before bare-rpc buffers a frame.
(confirmed)

## 4. Packets

On connect the node sends `{ address, version: "2.0.0" }` to apps (99-character XKR address;
apps check the length and show it) and `{ version: "2.0.0" }` to nodes. Nobody checks versions.
There is no other handshake. (confirmed)

App → node (`client_message`, checked in this order):

| packet | answer |
|---|---|
| `{ request: true, type: "some", timestamp, id }` | `{ response: [messages], id }`: pooled messages with `timestamp >` the given one, at most 500, sorted by time, `pow.job` removed. `timestamp` not a number, `type` longer than 10 or not `some`: **ban** |
| `{ type: "job_request", id }` | `{ type: "job", job, id }` or `{ type: "job_pending", id }`; the app is subscribed to job pushes `{ type: "job", job }` |
| `{ type: "post", message }` | `{ success: true, id }`, `{ success: true, duplicate: true, id }` or `{ success: false, reason, id }` (`id` = `message.id`); see §5 |
| `{ type: "register", ... }` or `{ register: true, data, timestamp, id, hash, pow }` | like `post`, for push-notification registration; forwarded to nodes as `{ push: true, message: packet }`, not stored |
| `{ type: "pow_tag", id }` | `{ success: true, id }` (legacy no-op) |
| other `type` | `{ success: false, reason: "unknown_type", id }` |

Node → app, unsolicited: `{ type: "new-message", messages: [...] }` every 5 s with the DMs
accepted since the last push (client view), and `{ type: "job", job }` on every new pool job.

Node ↔ node (`node_message`): `{ signal: true, message }` (gossip of an accepted DM, *with*
`pow.job`), `{ push: true, message: <app packet> }` (for a separate push-notification cluster;
hugin-nodes ignore it). Nothing else; in particular **no sync**: a node that was offline never
gets older messages, and gossip goes one hop only (a received signal is stored, not
re-gossiped and not pushed to the node's apps). (confirmed)

The `chunks`/`done` answer form (`response.length > 500`) is dead code because `some` is capped
at 500. (confirmed)

## 5. Messages and proof of work

A `post` message as hugin-desktop/hugin-native build it (confirmed):

```json
{ "cipher": "<hex, encrypted payload>", "timestamp": 1731500000000, "hash": "<64 hex, random>",
  "id": 1731500000000, "push": true, "viewtag": "<3 hex of a weekly hash>", "kind": "dm",
  "pow": { "version": 2,
           "job": { "job_id": "...", "blob": "<hex>", "target": "<8 hex>", ... },
           "shares": [ { "job_id": "...", "nonce": "<8 hex>", "result": "<64 hex>" } ],
           "auth": { "pub": "<64 hex>", "sig": "<128 hex>", "nonce": "<8 hex>" } } }
```

Shape check (`check`): `cipher` hex ≤ 9000 chars, `hash` hex ≤ 64, numeric `timestamp` within
−1 h/+5 min of now, `kind` absent or `dm`/`call`/`room`, `pow.version` absent or a number,
`job.job_id` ≤ 32 chars, `job.blob` even-length hex ≤ 1024 bytes, `job.target` 8 hex, ≤ 5 shares.

Share (`hugin-utils.verifyShare`, **confirmed byte-exact**): the nonce (4 bytes as given in hex)
is written into the blob after the varints `major`, `minor`, `timestamp` and the 32-byte
`prev_id` (offset 39 for a usual XKR template), the blob is hashed with
**`cn_turtle_lite_slow_hash_v2`** (CN-Turtle-Lite v2, XKR's block PoW since major version 5),
the lowercase hex must equal `result`, and the hash must meet the target: the last 8 bytes as a
little-endian `u64` `<=` `u64::MAX / (0xFFFFFFFF / raw)`, where `raw` is the target's 4 bytes
little-endian (the usual stratum compact target; an 8-byte target is used as is). A zero
target counts as met. `pools.js` names `cn_pico` as algorithm but only for logs.

`pow.auth` (confirmed byte-exact with keypear/libsodium): the app signs, with a fresh one-time
ed25519 key, `powsig:v2:<hash>:<timestamp>:<job_id>:<nonce>:<result>:<context>` where the share
is `shares[0]` (or the one whose nonce equals `auth.nonce`), nonce/result lowercase, and
`context` is `<kind>:<cipher>` for messages, `data` for registrations.

Node checks for an app post, in order: cooldown (1 s after the client's last accepted post),
duplicate (`hash:sig` accepted within 2 h → `success, duplicate`), no shares → `no_share` and a
60 s ban, then `cheap_pow`: shape, `pow.version === 2`, the job's `prev_id` is one of the last
three pool jobs' `prev_id`s (freshness), one share passes the fast shape check, the signature
verifies; then `pow_check`: the signature again and **one randomly chosen share is hashed**;
then all (≤ 5) shares go to the pool until one is accepted (a "duplicate share" answer from the
pool, or a share accepted within the last 10 minutes, also counts as accepted). Pool rejection →
`pool_reject` with the `rejects` list plus a fresh job push (no strike). Only then is a `dm`
stored and gossiped; `call`/`room` are accepted but neither stored nor gossiped; anything with
a `push` key is forwarded to nodes. Two PoW failures on one connection are a ban.

Gossip check (`pow_check_gossip`): signature, shape without freshness, one random share hashed.
Failure bans the sending node. hugin-node also refuses all gossip while it has no pool
connection (`validate_pow_payload` needs `poolConnector`). (confirmed)

**Share reuse**: the one-time `auth` key is not an identity,
so the signature only binds a share to the message its creator chose. Within 10 minutes a
client can pay for several messages with one share (the accepted-share cache makes later
submits "accepted" without asking the pool), and a node can mint any number of gossip messages
from one valid share, since gossip has neither freshness nor share accounting. `NONCE_TAG_BITS`
(binding nonces to the message hash) is defined but unused ("challenge flow removed").

## 6. Pool and payout (`pools.js`)

`PoolConnector`: line-delimited JSON-RPC over plain TCP (no TLS by default) to the first of
`techypool.ddns.net`, `pool-pay.com`, `fastpool.xyz`, `pool-pay.com`, `kalf.org`, port 3333.
`login {login: payoutAddress, pass: "x", agent: "hugin-node"}` → `result {id, job}`; the pool
pushes `{method: "job", params: job}`; `submit {id, job_id, nonce, result}`; the last 8 job ids
are valid; `unauthenticated` → re-login and one retry. Reconnect after 2 s on disconnect, next
pool on login failure or `ECONNREFUSED`. `payoutAddress` comes from `config.json` (prompted on
first start, default = the generated node address). No submit timeout. (confirmed; pool
behaviour beyond these calls assumed from the CryptoNote stratum convention)

## 7. Pool of messages and storage (`storage.js`)

`Map(hash → message)` in memory, `cleaner` every 10 minutes drops messages whose `timestamp`
is ≥ 24 h old. Saved to a hyperdb/RocksDB (`./db/messages.db`, schema `cipher, timestamp, pub,
hash, signature`, keyed by `timestamp`) only on SIGINT/SIGTERM and loaded at start. The schema
predates v2 messages (no `pub`/`signature`, no `kind`/`viewtag`/`pow`), so persistence of
current messages very likely fails or loses fields (assumed: not run). `DAY_LIMIT` (100 per
sender per day) and `limit()` exist but are never called. (confirmed)

## 8. Rate limits and bans

| rule | effect |
|---|---|
| PACKET data > 9000 bytes, not JSON, falsy JSON | ban (Hyperswarm `info.ban(true)`: until the swarm restarts) |
| invalid `request` | ban |
| `job_request` > 10 per 10 s per app or > 500 per 10 s total | `rate_limit`; 3rd strike → 60 s ban |
| post/register within 1 s of the last accepted | `cooldown` (no strike) |
| post/register while the same connection's previous one is still checked | `cooldown` (no strike; xkr-hugin only) |
| post without shares | `no_share`, 60 s ban |
| 2 PoW failures (shape, signature, hash) on one connection | ban |
| invalid gossip from a node | ban |

## 9. Identity (`id.js`, `setup.js`, `config.json`)

`config.json = { payoutAddress, private, nodeId }`, `nodeId = <address 99><private view key
64 hex>`. On first start hugin-node generates independent spend and view key pairs, keeps only
the address and the view key (the spend key is discarded, so the default payout address cannot
spend), and derives the DHT key pair of the public/private swarms from the view key (§2). The
mesh swarm uses a fresh random key pair each start. `setup.js` only generates the hyperdb
schema. (confirmed)

### What the apps do differently from hugin-node's assumptions

- hugin-desktop syncs with `{ request: true, type: "some", timestamp: store.get('pool.checked') }`;
  on a fresh install `pool.checked` is undefined, `timestamp` disappears from the JSON, and
  hugin-node **bans** the app (until hugin-node restarts). hugin-native starts at 0 and is fine.
- Both use the sync request's `timestamp` as its `id` (desktop: `Date.now()` if undefined) and
  keep chunks as an array of arrays (never triggered).
- hugin-native keeps up to 3 node connections and falls back to the next on error; desktop
  keeps one and refreshes discovery every 10 s.
- Apps validate pushed jobs (`validate_pow_job`); hugin-native also tracks the last three
  `prev_id`s, and after 4 failed reconnect attempts resets to the public topic.
- `viewtag` is computed by the sender (`cn_fast_hash(recipient view public key + week)[0..3]`)
  so recipients could filter; hugin-node does not index or filter by it.

## 10. Our implementation (`crates/xkr-hugin`)

| module | content |
|---|---|
| `protocol` | bare-rpc codec (`BareRpcCodec`, all three message types, size limit), JSON packet builders |
| `pow` | blob helpers, target, `verify_share` (xkr-crypto `cn_turtle_lite_slow_hash_v2`), `check_message`, `validate_pow_payload`, `verify_pow_auth` with libsodium-equivalent ed25519 verification on curve25519-dalek |
| `store` | `MessagePool`: dedupe by hash, time index, 24 h TTL, bounds (100 000 messages, 128 MiB, oldest out), append-only `messages.jsonl` compacted on open and at 2× the live set. Each message is kept as two serialized JSON views (full, and without `pow.job` for apps); the byte bound counts both, the hash twice and 256 bytes per entry |
| `pool` | `JobSource` trait; `StratumPool` (hugin-node's `PoolConnector` incl. fail-over and re-login, plain TCP or `stratum+ssl://` TLS), `PoolAddr`, `NoJobSource`, `ManualJobSource` (tests) |
| `p2pool` | `P2poolSource`: the relay's jobs from p2pool (local node first, public nodes as fall-back, back to the local one when it is up), login `<fee address>+<difficulty>` (`login_difficulty`: 1000 or the app difficulty if higher), jobs only while the node is synchronized (§11) |
| `relay` | `Relay::serve(stream, PeerMeta { role, remote_key, remote_ip })` over any `AsyncRead + AsyncWrite`; client and node packets, app jobs and targets, gossip, push batching, xkrd sync, rate limits, connection caps, bans, `HuginStats` |
| `identity` | `NodeIdentity` (`identity.json`, hugin-node `config.json` compatible) |

### Deliberate differences from hugin-node

| hugin-node | xkr-hugin | why |
|---|---|---|
| bans an app whose sync has no `timestamp` | missing/null `timestamp` = 0 | hugin-desktop's first sync |
| shifts `prev_id`s on every job (a pool re-sending jobs for one block narrows freshness to one block) | shifts only when `prev_id` changes | intent of the three-block window |
| share cached as accepted 10 min, pool "duplicate share" = accepted | each share is bound to the message it paid for (hash and signed content `kind:cipher`, 25 h), and held for a message from the moment its check starts (a pending binding, given up when the check fails, so of several posts paid with one share at once exactly one is accepted); a post whose share already paid for the same hash or the same content is answered `success, duplicate` without hashing, submitting or forwarding it again; reuse for other content → `share_reused` (client, no strike) or dropped (gossip, no ban); pool "duplicate" counts as a pool refusal, which still pays for the post while the job is at most one block old (§11, job age) | one share pays for one message (§5) |
| zero target counts as met | refused | otherwise the "work" is one free hash |
| hashes one random share and submits all shares | hashes the share `pow.auth` signs (the one whose nonce is `auth.nonce`, else `shares[0]`) and submits only that one, and only when it meets the pool's target (below) | the stored and gossiped message carries exactly that share |
| hands out the pool's jobs as they are; every accepted share is a pool share | jobs go to apps with the app target (`RelayConfig::app_difficulty`, `--hugin-share-difficulty`, default 200, compact target `14ae4701`; `job_id` and `blob` stay the pool's); a post or registration needs one share that meets it; that share is submitted to the pool only when its hash also meets the pool's own target of that job, the others pay for the message without a submit (§11) | about one second of mining per message on a phone, while p2pool takes no fixed difficulty below 1000 |
| stores and gossips the message as received | stores and gossips its canonical form: `cipher`, `timestamp`, `hash`, and if present `id` (number or string ≤ 64), `push` (bool), `viewtag` (string ≤ 64), `kind`; `pow` with `version`, `job { job_id, blob, target }`, `auth { pub, sig, nonce }` and `shares` = the signed share; at most 9000 bytes of JSON (larger: refused, or dropped without a ban in gossip and sync) | bounded message size; the signature covers hash, timestamp, the signed share and `kind:cipher`, so it stays valid |
| jobs and payout from a hardcoded list of plain-TCP pools | jobs from p2pool (the local node first), shares submitted under the operator's fee address (§11) | the shares go to the decentralised pool every node can run |
| a post on any job with a known `prev_id` | the job id and blob must be one of the last 8 jobs the relay handed out, issued for the current main-chain tip or the previous one, and the job's target at least as hard as our app target, else `{ success: false, reason: "stale_job" }` and the current job (no strike); a share is submitted only for the job id and blob of the current p2pool connection | p2pool job ids restart with every connection |
| gossip: signature, shape, one random share hashed | the updated hugin-node rules, in order: at most 200 `signal` packets at once per node connection, refilled at 20 per second (`GOSSIP_BURST`, `GOSSIP_PER_SECOND`; over that dropped unread, no ban); message shape with a timestamp at most 1 h old and 5 min ahead (drop); shape (ban); `pow.version` ≠ 2 (drop); job target zero or not 8 hex (ban), difficulty `floor(0xFFFFFFFF / raw)` below the gossip floor (drop; `Relay::gossip_floor`: `RelayConfig::min_share_difficulty`, by default `GOSSIP_MIN_DIFFICULTY` = 200, the higher of hugin-node's 100 and the default app difficulty, the same on every relay whatever its own app difficulty); signature (ban); the job's `prev_id` must be zero or one of the last 6 distinct `prev_id`s of our own pool jobs, otherwise the message waits (at most 256 messages, 60 s) and is checked again when our job source moves to that `prev_id`; the share `pow.auth` signs must match the job (ban); that share bound to another message, or held for one being checked (drop); that share, and only that one, is hashed: not valid (ban). The other shares of the message are not looked at, and the message is stored with the signed share only | gossip is verified locally with one hash per message; the hashing blob of every XKR job (p2pool or any XKR pool) is the merge-mining parent header, whose `prev_id` is zero, so relays on different p2pool nodes, and relays without one, take each other's gossip |
| one-hop gossip, not pushed to own apps | a new gossiped DM is pushed to our apps and re-signalled to every other node while its timestamp is at most 1 h old (dedupe by hash ends loops; synced messages are stored only) | delivery in seconds across the mesh; hugin-nodes receiving it verify it like any gossip |
| no sync | `xkrd_sync_request`/`xkrd_sync` between xkrd nodes (below) | a restarted or new node gets the last 24 h |
| pool saved on exit, broken schema | appended per message, full JSON | survives crashes |
| spend key discarded | spend key kept in `identity.json` | the node address keeps its keys |
| node ban until restart | 24 h ban by DHT key; timeout 60 s | bans survive bridge restarts |
| no submit timeout, stuck on an unresolvable pool | 10 s submit timeout; next pool after 3 failed connects | liveness |
| sends any packet to nodes | never sends a packet > 9000 bytes to a non-xkrd node | hugin-node would ban us |
| unlimited connections (Hyperswarm default 64 per swarm) | `--hugin-max-clients` (200) apps, new apps paused while many apps sync (§11, admission), of them at most `--hugin-max-clients-per-ip` (4) from one IP address or IPv6 /64, 8 per /56 and 16 per /48, and 32 nodes, at most 4 per IP address or /64 (§11, connection limits) | bounded memory; many apps need many addresses |
| `job` pushed twice to pending subscribers | once | |
| answers every `some` request with the oldest 500 after `timestamp` | at most 5 per 10 s per app, then `rate_limit` (no ban); the answer is the newest 500 after `timestamp` (and at most 4 MiB), oldest first, as the updated hugin-node; the apps of one address share a sync allowance of 16 MiB of answers per 10 min (§11, sync allowance) | each answer is up to 500 messages (~4.5 MB) for a 60-byte request; the limits keep the answer volume bounded |
| a `job_request` refused by the per-app limit still counts against the global 500 per 10 s, and global refusals give strikes | only requests within the app's own limit use the global window; only the app's own excess gives strikes | one busy app does not use up the global window of the others |
| shares hashed with no global bound | at most `RelayConfig::max_concurrent_hashes` CN-Turtle hashes at once relay-wide (xkrd: from `--verify-threads`), further checks wait FIFO for a permit | the blocking pool keeps free threads |
| per-connection queue by count only | per app: 1024 packets **and** 2 MiB of control packets and notifications (`OUTBOX_BYTES`), with `some` answers sent apart from it from one send budget shared by all apps (§11, send budget); 3 MiB per relay (`NODE_OUTBOX_BYTES`; an xkrd sync we serve waits for room instead of queueing all its batches); over either queue limit the connection is stopped | memory stays bounded, also for apps that do not read: with the defaults (200 apps, 32 relays, a 128 MiB pool, a 256 MiB budget) at most budget + pool + relay queues + app queues ≈ 256 + 128 + 96 + 400 MiB, in practice far less (an app's own queue holds only its rate-limited answers and the notifications, which all apps share as one copy) |
| no hello or idle timeout | the first packet must arrive within 60 s (`hello_timeout`, closed without a ban); when a role is at its limit, one connection of that role whose last served request is at least 10 min old (`idle_evict_after`) is closed for the newcomer: connections that never had a post, registration or gossiped message accepted first (longest without a served request first), the others only after 60 min. For an app without an accepted post or registration only those count, so its slot can go to another app 10 min after it connected; otherwise served requests are answered `some` and `job_request`, accepted posts and registrations, and from nodes `version`, new valid gossip and sync traffic. `pow_tag`, unknown types, refused requests and replies do not count | apps stay silent for hours (no keepalive packet), so there is no hard idle timeout |

### xkrd sync extension

Our node hello is `{ version: "2.0.0", xkrd: { version, sync: 1 } }` (hugin-node ignores the
extra key). Two peers that both announce `sync >= 1` raise their PACKET limit to 1 MiB, and each
sends `{ type: "xkrd_sync_request", id, since }` (`since` = now − 24 h); the answer is a series
of `{ type: "xkrd_sync", id, messages, done }` batches (≤ 500 messages and ≤ 256 KiB each,
≤ 20 000 per sync) with full messages including `pow.job`. The receiver checks each new DM like
gossip with a 24 h age limit (the pool's) and without the `prev_id` window (shape, version,
target against the same gossip floor, signature, share binding, the signed share hashed) and
stores it; what bans in gossip bans the sender here too, and a synced message is not
re-gossiped. Only the first `xkrd_sync_request` of a connection is answered
(a repeated one is ignored). hugin-node never sees these packets.

Limits on the extension: a sync is served once per remote key per
10 minutes (`SYNC_COOLDOWN`) and to at most 16 distinct keys per 10 minutes
(`MAX_SYNC_SERVES_PER_COOLDOWN`); a refused request gets an empty final batch. We receive at
most 2 syncs at once (`MAX_CONCURRENT_SYNCS_IN`, a slot is reused after 2 minutes); a node we
do not ask still gossips. The 1 MiB PACKET limit applies only while we receive that node's
sync; otherwise packets above 18 000 bytes (twice the Hugin limit, room for a `signal` around
a maximal post) are dropped unparsed, without a ban.

Bans are keyed by the remote DHT public key. Keys are free (any client can generate a new
key pair per connection), so a ban only stops a peer that keeps its key; the connection caps
(in total and per IP address and prefix), rate limits, hello timeout and idle eviction are what
bound a peer that changes keys.

Messages between relays (gossip and the xkrd sync) follow one set of rules on every relay: the
job target is at least `GOSSIP_MIN_DIFFICULTY` (200, whatever the relay's own app difficulty),
one hash checks the share `pow.auth` signs, a gossiped message is taken and passed on only
while its timestamp is at most 1 h old (a synced one within the pool's 24 h, stored without
passing it on), and each node connection may send 200 gossip packets at once and 20 per second
after that.

## 11. In xkrd

The relay runs in every xkrd with the Hyperswarm transport; `--no-hugin` turns it off, and so
does `--no-hyper` (one start-up line says so). With the mode flags `--node`, `--hugin` and
`--p2pool` only the ones named run: `xkrd --node --hugin` is the node and the relay without
p2pool, `xkrd --node --hugin --p2pool` all three; `--no-hugin` with mode flags and `--hugin`
with `--no-hyper` stop the start with an error. The relay also runs without the node
(`xkrd --hugin`, `xkrd --hugin --p2pool`; "The relay without the node" below). The start-up
step `Mode` shows what runs (`node + Hugin + p2pool`, `Hugin (remote chain)`). `--hugin-relay`
is still accepted and does nothing. `crates/xkrd/src/hyper.rs`:

1. loads or creates `<data dir>/hugin/identity.json` (mode 0600) and opens
   `<data dir>/hugin/messages.jsonl`;
2. starts the p2pool client (`xkr_hugin::P2poolSource`): the nodes of `--hugin-p2pool`
   (repeatable, in order, used as given), else the local p2pool `127.0.0.1:3333` followed by
   the public p2pool nodes built into xkrd (`PUBLIC_P2POOL_NODES`). On Linux the local entry is
   used only when no other user's process listens on port 3333 (`/proc/net/tcp` and
   `/proc/net/tcp6`, `p2pool::default_nodes_checked`); otherwise it is left out with one
   warning. The "Hugin relay" start-up step shows the node tried first
   (`max 200 apps · fee to nilsjr · p2pool 127.0.0.1:3333`). It logs in with `<fee address>+1000`
   (`P2POOL_MIN_DIFF`, p2pool's least fixed difficulty; `+<n>` when `--hugin-share-difficulty`
   is above 1000),
   moves to the next node when one cannot be reached, waits longer after every round in which
   no node answered (2 s doubling to 60 s), and while it uses another node tries the first one
   every 30 s and moves back as soon as it accepts a login. `stratum+ssl://host:port` uses TLS;
3. hands p2pool's jobs to the apps, with the app target (below), only while the node is
   synchronized (synced and not downloading, what the built-in miner waits for); until then
   `job_request` is answered `job_pending`. Gossip, the xkrd sync and the stored messages work
   all the time;
4. submits the share of an accepted post or registration to p2pool under the fee address
   (§13) when it also meets p2pool's target. Without a fee address nothing is submitted (the
   relay checks every share itself either way). When no p2pool node can be reached the relay keeps running (it stores,
   gossips, syncs, and answers `job_pending`), and xkrd logs once (info level; the normal
   state while the node syncs and the built-in p2pool has not started)
   `no p2pool reachable — Hugin shares are not submitted` and shows the p2pool connection in
   `status`;
5. announces, with their `mid`s, the node mesh (both), public nodes (server; not with
   `--hugin-private`, and only once the node is synchronized) and private node (server)
   topics; each gets its own DHT identity, the private one with hugin-node's view-key-derived
   seed, and the log prints the private invite
   (`Hugin private node invite invite=<address><key>`). On the testnet the topics are the
   testnet ones (§14). An xkrd private node (`--private`) is different: it joins only the node
   mesh, as a client, and serves apps through its own Hugin identity, below;
6. routes connections by topic: XKR topic → Levin P2P manager (unchanged), Hugin topic → the
   relay as `Role::Node` (mesh) or `Role::Client` (public/private); Hugin connections are not
   XKR peers and do not count against `--hyper-max-peers`;
7. keeps the relay, message pool and p2pool client across bridge restarts; on shutdown
   unannounces all topics and closes the Hugin connections.

p2pool builds every block with the wallet it was started with and pays its sidechain shares to
that wallet; the stratum login only names the user. The shares of a relay therefore pay the
wallet of the p2pool node that receives them: run the local p2pool with
`--wallet <fee address>` to have them paid to the fee address (`docs/mining.md`, "The Hugin
relay as a p2pool miner").

### The relay without the node

`xkrd --hugin` and `xkrd --hugin --p2pool` run the relay without a database, sync or Levin P2P
(`crates/xkrd/src/remote.rs`, `run_hyper`). The apps notice no difference: nothing they get from
a relay depends on where its chain comes from (hello, jobs, messages, sync answers are the
same). What changes:

- **Sync state and tip.** The relay reads them from the remote nodes on the clients topic
  (`RemoteNodes`, `docs/hyper-transport.md`, "Client role"), through `/status` `remote`
  (`hyper::follow_remote_sync`, `follow_remote_tip`). At the start it is not synchronized and
  has no tip (no jobs dated at height 0). It hands out jobs only while the remote nodes are
  synchronized (a node answers and says it is synchronized, and the tip was confirmed less
  than three minutes ago); a remote status that stops changing for three minutes (it changes
  every few seconds while a node answers) counts as not synchronized. Until then, and again
  after it, `job_request` is answered `job_pending`. Jobs are dated with the remote tip
  (`stale_job` as with the node).
- **Topics.** The node mesh and the private node topic are announced at once. The public-node
  topic is announced only while the remote nodes are synchronized, and left again when they
  are not, so a relay without a working chain is never found by new apps. `--private` needs the
  node; `--hugin-private` works.
- **Jobs**, in this order (`hyper::remote_relay_nodes`, the "Hugin relay" start-up step shows
  the first): the built-in p2pool's stratum server (`--p2pool`; left out when its address cannot
  be bound), the nodes of `--hugin-p2pool` (else the local p2pool `127.0.0.1:3333` and the
  public nodes, as with the node), then `hyperswarm`: the stratum service of a connected remote
  node that runs p2pool (`remote::RemoteStratum`, "Service stratum" in
  `docs/hyper-transport.md`). The stratum client moves on when one cannot be reached and goes
  back to the first every 30 s, as above. Nodes whose hello lists `stratum` and that have the
  tip in use are tried first; the session runs on the connection that carries the chain.
- **Payout.** Shares go to the p2pool that receives them, and p2pool pays every share to the
  wallet it was started with (above). With `xkrd --hugin --p2pool` that is the relay's own
  p2pool, paying the fee address (`--hugin-payout`). Shares submitted through a remote node's
  stratum service count for that node's p2pool wallet; run `--p2pool` (or a p2pool of your
  own with `--hugin-p2pool`) to have the relay's shares paid to its fee address.
- **Status.** `/status` has `hugin` as with the node, next to `remote` and `p2pool`.

### Mining difficulty and the fee

Apps mine at the relay's **app difficulty** (`--hugin-share-difficulty`, default 200, at least
1): about one second per message on a multi-core phone. p2pool takes no fixed share difficulty
below 1000, so the two are split:

* the relay logs in to p2pool with `+1000`, or `+<n>` when the app difficulty `n` is above 1000
  (`xkr_hugin::p2pool::login_difficulty`);
* every job reaches the apps with `target` replaced by the 4-byte compact target of the app
  difficulty (`floor(0xFFFFFFFF / n)` little-endian, `pow::target_for_difficulty`; the same
  numbers as hugin-pow `difficultyToTarget` / `targetToDifficulty`, checked with vectors in
  `xkr-stratum` `target::tests::difficulty_targets_match_hugin_p2p`). `job_id` and `blob` stay
  p2pool's;
* a post or registration is checked against the app target as before (shape, freshness,
  signature, share binding, the CN-Turtle hash); a job whose target is easier than ours is
  answered `stale_job` with the current job;
* the share is submitted to p2pool only when its hash also meets p2pool's target of that job
  (kept for the last 8 jobs). With the defaults about one accepted share in five is a p2pool
  share; the others pay for their message and are not submitted. A submitted share that p2pool
  refuses (stale, duplicate, too easy after a new block, ...) still pays for the post while its
  job is fresh (below): the app did its work. It is counted in `shares_rejected_by_pool` and
  logged at debug level.

**Job age.** The relay dates every job it hands out, and every p2pool job it keeps, with the
node's main-chain tip at the time the job arrived (`NetStatus` `top_index`/`top_hash`, the
status the sync gate reads; `xkr_hugin::ChainTip`, `JobSource::tip`). A post or registration
pays only with a job issued for the current tip, or for the previous one during the first 15 s
after the relay saw the new tip (`relay::PREVIOUS_BLOCK_GRACE`; tip height − job height ≤ 1,
`pool::MAX_JOB_AGE`); an older job, or one the relay did not date, is answered
`stale_job` with the current job, without a strike. The check runs before the signature and
again when p2pool refuses the share (a block may arrive meanwhile). Gossip and synced messages
keep their own rules (the `prev_id` window above), since other relays' job ids are not ours.

Other relays check gossip and synced messages against the target in the message, which is the
app target of the relay that took the post; they drop it when that is easier than their gossip
floor (the lower of 100 and their own app difficulty). An app difficulty below 100 therefore
keeps those messages on relays with the same setting (xkrd warns at start-up).

Counters: `shares_accepted` (app shares that paid for an accepted post or registration,
p2pool-refused ones within the job age included),
`shares_submitted` (sent to p2pool, a retry after a re-login counts again) and
`shares_rejected_by_pool` (submits p2pool did not accept, duplicates included).

### Connection limits

* `--hugin-max-clients` (200): app connections in total, public and private topics together;
  at the limit an idle connection makes room (§10, idle eviction), otherwise the newcomer is
  closed. Below it, new apps can be paused (admission, below).
* `--hugin-max-clients-per-ip` (4, 0 = none of these limits): app connections from one remote
  IP address. The address is the one the bridge reports for the secret stream (`Opened.addr`,
  `docs/hyper-bridge-protocol.md`): the app's own address for direct and holepunched
  connections, the relaying node's for relayed ones; IPv4-mapped IPv6 counts as IPv4, and an
  IPv6 address counts per /64 (one host or home network, as xkrd counts XKR peers), with
  further limits of 8 per /56 and 16 per /48 (`MAX_CLIENTS_PER_56`, `MAX_CLIENTS_PER_48`; a
  higher per-address limit raises them to it). A connection without an address is not limited
  by it, only by the total. A connection over a limit is closed at once (a debug log line
  `Hugin connections per address reached`).
* Node-mesh connections (other relays) have their own limit (32), at most 4 per IP address or
  IPv6 /64 (`MAX_NODES_PER_ADDRESS`).
* Per connection, as before: 1 s between accepted posts and one post or registration checked
  at a time, 10 `job_request`s per 10 s (500 for all apps), 5 `some` requests per 10 s, 200
  gossip packets at once and 20 per second from a relay, strikes and bans. More messages per second therefore
  take more connections, more addresses and the work of every share.

### Send budget and history syncs

What the relay sends to apps is one shared queue with two classes:

1. **Control packets and notifications** go first: the hello, answers to posts, registrations
   and job requests, PoW jobs, bare-rpc replies, and the `new-message` notification that a
   stored message arrived. They go through each connection's own queue (`OUTBOX_BYTES`) and
   never wait for a history sync.
2. **History syncs** (`some` answers: the stored messages an app fetches, up to 500 messages
   and 4 MiB) come second:
   * at most `--hugin-sync-slots` (8) are prepared and sent at once; further `some` requests
     wait in arrival order. A request whose connection closes while it waits is dropped, and a
     newer request of the same connection takes the place of one that still waits for that
     connection's previous answer. The rate limit of 5 `some` requests per 10 s stays.
   * all app connections share one send budget, `--hugin-queue-budget-mb` (256 MiB): the
     queued control packets and notifications plus the sync answers not yet written. An answer
     is prepared only when it fits (or when nothing is queued at all); until then it keeps its
     slot and waits.
   * the answers being sent are written in turns of `SYNC_CHUNK` (64 KiB), two at a time
     (`RelayConfig::sync_writers`), each turn at most 250 ms; an answer that used its turn
     waits behind the others, so every syncing app makes progress and one large sync cannot
     hold up the rest. Written bytes go back to the budget at once; the slot is free when the
     answer is written or its connection closes.
   * a connection writes its queued control packets and notifications before it starts a sync
     answer. An answer is one bare-rpc packet (hugin-node's `{ id, response }`), so once its
     first byte is out, that connection's next packet follows when the answer is complete. An
     app that reads nothing of its answer for 60 s is closed.

### Sync allowance

The app connections of one address share a sync allowance: 16 MiB of messages in `some` answers
per 10 minutes (`SYNC_ALLOWANCE_BYTES`, `SYNC_ALLOWANCE_WINDOW`), refilled continuously. An
address is counted as in the per-address app limits: an IPv4 address, or an IPv6 /64
(IPv4-mapped IPv6 as IPv4); a connection without an address has an allowance of its own. The
relay keeps an address's allowance while it is not full again or an app of that address is
connected, so an app that reconnects continues with the allowance it had. An answer larger than
what is left keeps its newest messages that fit (at least one), in the same order as every
`some` answer, so the app goes on from its newest message; the allowance is then used up. Until
it is full again, a connection that was already answered gets only the messages after the newest
one it was sent, and requests from that address wait for a sync slot behind those of all other
apps. Independently of the allowance, a `some` request within 30 s of the connection's previous
answer (`SYNC_REPEAT_WINDOW`) whose `timestamp` is older than the newest message the connection
was already sent is answered with the messages after that message. An app that syncs once on
connect and then asks for new messages stays well within it (one answer is at most 4 MiB).
Answers made smaller by the allowance are counted (`syncs_limited`), and `status` shows
`, N on allowance` after the queued syncs while N connected apps have a used-up allowance
(`apps_on_allowance`).

### Admission

New app connections are paused (closed right after the secret stream opens, before the hello,
like a connection over `--hugin-max-clients`) while more `some` requests wait than there are
sync slots, or the send budget is 80 % used. They resume once at most half the slots' worth of
requests wait and less than 60 % of the budget is used. Connected apps are never closed for
this, and node-mesh connections are not affected. Each paused connection is logged at debug
level and counted (`refused_busy`). An app that is closed this way reconnects as it does after
any lost connection (hugin-p2p `NodeClient`): it promotes a backup connection if it has one,
otherwise it refreshes discovery on the node topic and connects to another node announcing
there; with a private node invite it retries the same node until there is room again.

Memory with the defaults: at most the budget (256 MiB) + the pool (128 MiB) + the relay
queues (32 × 3 MiB) + the app queues (200 × 2 MiB, which only fill with an app's own
rate-limited answers; notifications are one shared copy). `relay::tests::memory_bound_of_the_default_limits`
checks that this stays below 900 MiB.

### Counters and flags

`HyperConfig::hugin_stats: Arc<HuginStats>` holds live counters (clients, nodes, xkrd nodes,
messages, bytes, posts, gossip, sync, shares, bans, the app cap, history syncs running and
queued, bytes queued for apps, whether new apps are paused and how many were closed for it,
answers made smaller by the sync allowance and apps whose allowance is used up,
whether a job can be handed out, the fee address and the p2pool connection; `snapshot()` is
serializable); `xkrd` hands it to the RPC,
and `/status` shows the snapshot as `hugin` (`null` with the relay off), among others
`shares_accepted`, `shares_submitted`, `shares_rejected_by_pool`, `apps_cap`,
`syncs_running`, `syncs_queued`, `queued_bytes`, `admission_paused`, `refused_busy`,
`syncs_limited`, `apps_on_allowance`, `fee_address`
(`null`: none), `fee_donation` and `p2pool { host, connected }`. `fee_address`,
`p2pool.host` and `hyper.udp_port`, `hyper.firewalled` and `hyper.nat` are in the answer only
for a client on a loopback address; other clients get the rest.

| flag | default | meaning |
|---|---|---|
| `--hugin` | without mode flags: relay on | mode flag: run the relay (with `--node`; only the modes named run) |
| `--no-hugin` | relay on | do not run the relay (not with mode flags) |
| `--hugin-payout <address>` | the saved address, else the donation address | fee address; saved (§13) |
| `--hugin-p2pool <host:port>` | `127.0.0.1:3333` (on Linux not when another user holds the port), then `PUBLIC_P2POOL_NODES` | p2pool stratum nodes, repeatable, in order, used as given; replaces the whole default list |
| `--hugin-share-difficulty <n>` | 200 | app difficulty: the target of the jobs handed to apps; the p2pool login asks for `max(1000, n)`; other relays drop messages easier than 200 |
| `--hugin-max-clients <n>` | 200 | app connections (public + private) |
| `--hugin-sync-slots <n>` | 8 | history syncs (`some` answers) of apps prepared and sent at once; more wait in arrival order |
| `--hugin-queue-budget-mb <n>` | 256 | send budget of all app connections together, in MiB; new apps are paused at 80 % and resumed below 60 % |
| `--hugin-max-clients-per-ip <n>` | 4 | app connections from one IP address or IPv6 /64, with 8 per /56 and 16 per /48 (0: none of these limits) |
| `--hugin-private` | off | do not join the public-node topic |
| `--private` | off | xkrd private node: node mesh client-only, no public-node and no address-based private topic; apps connect through the node's invite (below) |

### xkrd private node (`--private`)

With `--private` (`docs/hyper-transport.md`, "Private node") the relay serves Hugin apps only
through the node's Hugin app invite. The node joins the node mesh as a client only and does not
join the public-node topic or the address-based private topic above.

The Hugin app invite has the form of the private node invite above with one field more:
`<relay address, 99><Hugin topic key, 64 hex><Hugin identity key, 64 hex>` (227 characters).
The node logs it once the Hugin identity is announced (`Hugin app invite of the private node`)
and the console command `invite` prints it next to the `xkrinv2:` invite. The topic key is
`cn_fast_hash("xkr/private/hugin/v1" || secret)`; an app uses its 32 bytes as the Hugin topic
key (base key pair, topic, `mid`). As with the 163-character invite, the app pays the relay at
the address in the first 99 characters and admits only the key in the last 64; hugin-network's
`NodeClient` takes the topic key from the 64 characters between them. The invite reaches the
Hugin identity only, not block sync; the relay treats these apps as apps (`Role::Client`, same
limits). Without `--private` the address-based private topic and its invite work as before.
`invite new` makes a new secret: earlier invites stop working.

## 12. Tests

- `cargo test -p xkr-hugin`: codec vectors (bare-rpc frames, split/coalesced input, limits),
  PoW vectors (nonce offset, `prev_id`, the target table, a real CN-Turtle share, case
  sensitivity, zero target), `pow.auth` vectors (our ed25519 signer reproduces libsodium's
  signatures), message shape rules, pool TTL/dedupe/bounds/persistence/compaction, identity
  round trip, `feeAddress`/`feeAsked` and the hugin-node `config.json` import
  (`identity::tests`), a stratum session against a fake pool, the p2pool source
  (`p2pool::tests`: logins, no job before the node is synchronized, a new fee address logs in
  again), and `tests/relay_loopback.rs`: two and three in-process relays over
  `tokio::io::duplex` (post, pool submit, push, gossip, `some` sync, duplicate, share reuse,
  xkrd sync, bans for bad JSON, oversized packets, bad requests, missing shares, bad
  signatures, invalid gossip), `relays_forward_only_the_signed_share`,
  `gossip_checks_follow_hugin_node` (the signed share decides, difficulty floor, zero target,
  waiting for a `prev_id`), `gossip_with_a_zero_prev_id_needs_no_job` and
  `stale_jobs_and_posts_without_submits`; the app difficulty in `relay::tests::{apps_get_the_app_target,
  only_shares_that_meet_the_pool_target_are_submitted, jobs_easier_than_the_app_target_are_refused,
  gossip_floor_is_the_same_for_every_app_difficulty}` and
  `pow::tests::verified_results_against_other_targets`; gossip and sharing rules in
  `relay::tests::{gossip_pays_with_the_signed_share, one_share_one_post_under_concurrency,
  posts_of_one_connection_take_turns, gossip_bucket_refills, gossip_rate_per_connection,
  only_recent_messages_are_regossiped}`; the local p2pool check in
  `p2pool::tests::local_pool_owner_check`.
- `cargo test -p xkr-stratum`: `fails_over_and_returns_to_the_first_pool` (the first pool down:
  the second is used; the first up again: the client moves back; a new login reconnects with
  it; a share is sent only for the blob its job id was issued with) and
  `no_reachable_pool_is_reported`.
- Resource limits: `relay::tests::{outbox_is_bounded_by_bytes, memory_bound_of_the_default_limits,
  app_syncs_take_slots_in_arrival_order, a_newer_sync_request_replaces_a_waiting_one,
  sync_allowance_cuts_a_large_answer_and_refills, repeat_sync_requests_get_only_newer_messages,
  used_up_allowances_wait_behind_others_for_a_slot,
  a_full_sync_then_new_messages_stays_within_the_allowance,
  the_sync_allowance_belongs_to_the_address,
  budget_holds_a_large_answer_until_the_queue_drains, admission_pauses_and_resumes_with_hysteresis,
  notifications_go_first_while_syncs_fill_the_budget, two_syncs_progress_in_turns,
  large_syncs_wait_for_the_queue, share_hashing_waits_for_a_permit,
  global_job_window_counts_only_admitted_requests, xkrd_sync_is_served_once_per_key_per_cooldown,
  incoming_syncs_are_capped, large_node_packets_only_during_our_sync,
  ttl_map_reinsert_keeps_one_queue_entry, repost_with_new_auth_keys_is_a_duplicate,
  share_reuse_is_answered_without_a_strike, only_the_signed_share_is_kept,
  synced_messages_keep_only_known_fields, eviction_counts_paid_activity_of_apps,
  app_connections_are_capped_per_address, address_caps_per_prefix}`,
  `store::tests::eviction_is_exact_within_a_millisecond`, and in `tests/prop_protocol.rs`
  `silent_connections_hit_the_hello_timeout`, `idle_client_is_evicted_when_full` and
  `pow_tag_packets_do_not_keep_a_slot`.
- `cargo test -p xkrd`: flags (`cli::tests::hugin_flags`, `hyper::tests::config_from_the_command_line`),
  topics and roles on both networks, the jobs following the sync state, private seed =
  hugin-node's (vector), the fee address (`fee::tests`: validation, flag > saved > default,
  saving, the question), the console (`fee-address`, the question answered with Enter, other
  commands dropping it).
- `cargo test -p xkr-p2p-hyper topic`: the topic derivation of §2 for all three node keys,
  checked against `cn_fast_hash("hugin/topic" || base public key)` computed in the test, that
  the earlier hex-fill topic is not produced, and the testnet topics of §14
  (`hugin_topics_hash_the_domain_and_base_key`,
  `hugin_topics_are_not_the_legacy_hex_fill_topics`, `hugin_domain_constants`,
  `hugin_mid_message_is_domain_then_key`, `hugin_testnet_topics_differ_from_mainnet`).
- `bridge` `npm test` (`test/mid.test.js`): Keccak known answers, topic derivation (mainnet and
  testnet), the `hugin_derivation` vectors checked with libsodium, `mid` sign/verify, an
  old-scheme `mid` refused by `verifyMid` and in a handshake, and over a local testnet a client
  with an old-scheme `mid` refused by our identity and the earlier topic refused by the bridge.
  `test/hugin-interop.test.js` (needs the old forks, see `bridge/README.md`) checks the
  separation against real hyperswarm-hugin peers in both directions.
- Vectors: `HUGIN_NODE_MODULES=<hugin-desktop>/node_modules node bridge/test/hugin-vectors.js
  --write` regenerates `docs/test-vectors/hugin-relay.json` with bare-rpc, hugin-utils,
  kryptokrona-utils and keypear. Its `hugin_derivation` section (the three node topics with
  topic key, base public key and topic, and the `mid` of each for the DHT key pair with seed
  32 × `0x07`) is written by `XKR_WRITE_VECTORS=1 cargo test -p xkr-p2p-hyper --test
  hugin_derivation_vectors` (only that key is replaced); `hugin-vectors.js` carries it over
  after checking it against kryptokrona-utils and libsodium.

### Local test (`testnet/hugin-relay-test.sh <work dir>`)

Now `testnet/scenarios/40-hugin-relay.sh` in the regression suite (`docs/testnet-suite.md`).
Local HyperDHT; jobs from p2pool when the image `xkr-p2pool:testnet` is there (A mines 10
blocks first, p2pool runs on A's RPC with A's fee address as its wallet), else from the fake
stratum pool `bridge/test/fake-stratum-pool.js` (difficulty 100); node A (relay by default,
`--hugin-payout <address>`) and node B (`--hugin-private`, the default fee address), both testnet with
no legacy peers; and `bridge/test/hugin-client-sim.js --testnet`, an app that does what
hugin-desktop does, with bare-rpc, keypear, hugin-utils and kryptokrona-utils and the bridge's
own `mid` layer on the testnet topics. Steps: A's relay logs in to the pool with its fee
address; B says it donates its fee; public topic post + sync + push on A; A's `/status` counts
the accepted share and p2pool logs it under the fee address; B has the message (mesh); post
through A's private invite; A restarted still has it.

```
HUGIN_NODE_MODULES=<hugin-desktop checkout>/node_modules \
testnet/hugin-relay-test.sh /tmp/xkr-hugin                  # p2pool if the image is there
HUGIN_JOBS=fake HUGIN_NODE_MODULES=<hugin-desktop checkout>/node_modules \
testnet/hugin-relay-test.sh /tmp/xkr-hugin-fake             # fake stratum pool
```

## 13. The fee address

The node operator's fee address is the address the relay's shares are submitted under. It is
kept by xkrd's fee address handle (`crates/xkrd/src/fee.rs`, shared with later services of the
node) and saved in `<data dir>/hugin/identity.json` (`feeAddress`, and `feeAsked` once the
operator answered; mode 0600, written atomically). A hugin-node `config.json` imported as
`identity.json` keeps its `payoutAddress` as the fee address.

* Precedence: `--hugin-payout <address>` (saved at once), then the saved address, then the default:
  a donation to the xkrd developer, nilsjr.
* Valid: a standard address with either prefix (`SEKR…` or `Xkr…`; the testnet uses the same
  prefixes); integrated addresses are refused. A bad `--hugin-payout` stops the start with an error.
* While the default donation is in effect, the start-up shows under the "Hugin relay" step
  `You are donating your hugin node fee to nilsjr`, and, until the operator has answered once,
  asks `Change it? Type fee-address <address>, or Enter to keep.` The console stays free:
  Enter keeps the donation (saved, not asked again), `fee-address <address>` sets an address,
  any other command drops the question (asked again at the next start). Once answered, the
  notice is followed by `Change it: fee-address <address>, or --hugin-payout <address>`.
  Without a terminal (`--no-console`, `--log-json`, a service) there is no question, only the
  notice and that line as one warning line.
* Console: `fee-address` (also `fee_address`, `hugin-address`, `hugin_address`) shows the
  address in effect (and "donation to nilsjr" for the default); `fee-address <address>` checks
  it, saves it and applies it at once: the p2pool client logs in again with the new address
  (on a new connection, since p2pool takes one login per connection). Jobs handed out before
  the change belong to the old connection: a post on one is answered `stale_job` with the new
  job, and the app mines again.
* `status` shows the fee address, the p2pool node with its connection state and the shares
  accepted, submitted to p2pool and rejected by it.

## 14. Testnet topics

A testnet relay derives its three topics with the domain `hugin/testnet/topic` instead of
`hugin/topic` (`topic = cn_fast_hash("hugin/testnet/topic" || base.publicKey)`,
`HUGIN_TESTNET_TOPIC_DOMAIN`, `HuginTopic::on_network`); keys, base key pairs and `mid`s are
the same on both networks. Testnet relays and apps therefore never meet mainnet ones on the
DHT; the mainnet derivation is unchanged. The bridge (`bridge/src/mid.js`,
`huginTopicFromKey(key, 'testnet')`) accepts both forms, and the app simulator joins the testnet
topics with `--testnet`. An app that wants to use testnet relays needs the same option.

## 15. Not done yet

- The push-notification cluster (`push` packets are forwarded like hugin-node does, nothing
  more).
- The public p2pool nodes (`PUBLIC_P2POOL_NODES`) are still to be chosen; until then the relay
  uses the local p2pool only (or the nodes given with `--hugin-p2pool`), and without the node
  also the stratum service of remote nodes.
