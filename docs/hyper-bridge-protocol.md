# Hyper-bridge frame protocol (Rust ↔ Bare), v1

Bare (JS) owns HyperDHT, holepunching, UDX and secret-stream; Rust owns everything else. The
frames travel over in-process channels between xkrd and N Bare worker threads started through
bare-kit's C API (`docs/hyper-transport.md`, "Bare workers"). Nothing in a frame depends on what
carries it.

Implementations: `bridge/src/frames.js` (JS) and `crates/xkr-p2p-hyper/src/frame.rs` (Rust).
Golden vectors (one frame of every type, hex): `docs/test-vectors/hyper-bridge-frames.json`;
both test suites build every vector by hand and compare bytes.

## 1. Framing

All integers little-endian (ports too). Every frame = 16-byte header + body.

```
offset size field
0      4    len       bytes after this field (12 + body length)
4      1    type      frame type (table below)
5      1    flags     bit0 END (last frame of a multi-frame reply), bit7 IGNORABLE
6      2    shard     Bare worker index; routes control frames
8      8    conn_id   connection id; 0 for frames that are not about one connection
16     ..   body
```

`max_frame` (from `Hello`) bounds the **whole** frame: `len + 4 <= max_frame`. Both sides
propose at least 4096; the default is 16 MiB. A frame over the limit is a fatal `BAD_FRAME`.

A receiver that sees an unknown `type` drops the frame if `IGNORABLE` is set, otherwise it sends
`Error(UNKNOWN_TYPE)` and closes the bridge. Bodies may end with **TLV extensions**
(`tag u8 | len u16 | value`) after their fixed fields; unknown tags are skipped, a truncated TLV
is `BAD_FRAME`, and so are more than 64 TLVs in one body (`MAX_TLVS_PER_FRAME`; v1 uses at
most three). This is how minor versions add fields without new frame types. Frames with a
fixed-size body and no TLV tail (`Ping`, `Pong`, `Credit`, `PeerInfo` reply) must not carry
trailing bytes. `Close` and `Error` end in free text and therefore cannot carry TLVs.

Addresses (`addr_family u8, port u16, addr [16]`): family 0 = none (all zero), 4 = IPv4 in
`addr[0..4]` with the rest zero, 6 = IPv6.

Transport: one byte stream per worker, bare-kit's IPC channel between the worker and a pump
thread, and an in-memory channel between the pump and the Rust link; frames go back to back (the
IPC may split or join writes, `len` delimits frames). Nothing outside the process can reach the
channel.

## 2. Frame types

| type | name | dir | body |
|---|---|---|---|
| 0x01 | Hello | both | `major u16, minor u16, max_frame u32, initial_credit u32, workers u16, worker u16, features u64` + TLV (`0x01` node public key 32 B; `0x02` reserved) |
| 0x02 | Ping | both | `nonce u64` |
| 0x03 | Pong | both | `nonce u64` |
| 0x10 | Open | R→B | `request_id u32, remote_public_key [32], topic [32] (zeros = none), open_flags u32` + TLV (`0x10`, `0x11`, `0x12` as in Announce) |
| 0x11 | Opened | B→R | `request_id u32 (0 for inbound), remote_public_key [32], handshake_hash [64], topic [32] (zeros = unknown), initiator u8, addr_family u8 (0/4/6), port u16, addr [16]` + TLV (`0x01` local public key used) |
| 0x12 | Close | both | `reason u8, code u32, message utf8 (rest)` |
| 0x20 | Data | both | payload: exactly one secret-stream message |
| 0x21 | Credit | both | `bytes u32` added to the sender's window for `conn_id` |
| 0x30 | Announce | R→B | `request_id u32, topic [32], mode u8 (1 server, 2 client, 3 both)` + TLV (`0x10` hugin `mid` 64 B, `0x11` hugin topic key 32 B, `0x12` identity seed 32 B) |
| 0x31 | Unannounce | R→B | `request_id u32, topic [32]` |
| 0x32 | Lookup | R→B | `request_id u32, topic [32], max_peers u16 (0 = no limit)` |
| 0x33 | LookupResult | B→R | `request_id u32, count u16, count × (public_key [32], addr_family u8, port u16, addr [16])`; `END` on the last |
| 0x34 | Ack | B→R | `request_id u32` (Announce/Unannounce done, topic fully joined) + TLV (`0x01` local public key used for the topic) |
| 0x40 | PeerInfo | B→R | `rtt_us u32, bytes_in u64, bytes_out u64, relayed u8, holepunched u8, addr_family u8, port u16, addr [16]` (reply to `PeerInfo` R→B with empty body, or unsolicited every 30 s) |
| 0x41 | DhtStatus | B→R | `state u8, firewalled u8, nat u8, udp_port u16` + TLV (none defined); a control frame with `IGNORABLE` set, see below |
| 0x7F | Error | both | `request_id u32, code u16, message utf8` |

`open_flags`: bit0 allow relay (only effective when the bridge has a blind relay configured),
bit1 no-holepunch (direct, LAN or already open routes only). `Close.reason`: 0 normal, 1 error,
2 timeout, 3 firewall-rejected (e.g. missing hugin `mid`), 4 protocol violation, 5 bridge
shutdown. `Close.code` (informative, 0 = unspecified): 1 peer not found, 2 peer connection failed,
3 holepunch failed, 4 timeout, 5 hugin `mid` rejected, 6 remote closed, 7 message too large,
8 credit exceeded. `Error.code`: 1 UNKNOWN_TYPE, 2 BAD_FRAME, 3 UNKNOWN_CONN, 4 CREDIT_EXCEEDED,
5 DHT_ERROR, 6 VERSION (7 reserved). An `Error` about a
connection carries its id in the header `conn_id`.

`Hello.features`: bit0 `HUGIN_MID` (the bridge understands TLVs `0x10..0x12`).

### DhtStatus

The state of the worker's DHT node, sent after the Rust side's `Hello` and again whenever a
field changes (the bridge sends no duplicates). It always carries the `IGNORABLE` flag, so a
receiver that does not know the type drops it.

- `state`: 0 starting (the UDP socket is bound or being bound, the DHT is bootstrapping),
  1 ready (bootstrapped), 2 failed (the DHT could not start, for example because a fixed UDP
  port given with `--port-fixed` is taken).
- `firewalled`: 1 while other DHT nodes cannot reach the node's UDP port without
  holepunching (hyperdht's `dht.firewalled`; it starts at 1 and turns 0 when the DHT's own
  reachability check passes), else 0. Only meaningful with `state = 1`.
- `nat`: what the DHT nodes saw of the node's public address: 0 unknown (no samples yet),
  1 consistent (one public port for every destination), 2 random (the NAT picks a new public
  port per destination, so holepunching towards another such node does not work; hyperdht's
  `dht.randomized`).
- `udp_port`: the local UDP port of the DHT server socket, 0 while it is not bound.

### LookupResult

`addr` of a peer is the first DHT node it announced through (a handshake relay hint), not the
peer's own address, which the DHT does not reveal; family 0 if none. A lookup that finds nobody
is answered with one `LookupResult` with `count = 0` and `END`.

### Opened

`handshake_hash` is the Noise handshake hash (64 bytes, BLAKE2b-512 in hyperdht), identical on
both ends; it binds application-level authentication to the encrypted channel. `request_id = 0`
marks every connection Rust did not ask for: server-side accepts *and* Hyperswarm auto-dials of
topics joined in client mode (those have `initiator = 1`). The topic is known for explicit
`Open`s and for Hugin identities (one topic each), and is the first topic the peer was found
under for auto-dials; it is zero for server-side accepts of the plain swarm, because the
handshake does not say which topic the remote found us under. `Opened` is sent only once the
secret stream is open; inbound handshakes that fail earlier are never announced.

`addr` is the remote address of the stream's UDP socket (`rawStream.remoteHost` /
`remotePort`): the peer's own address for direct and holepunched connections, the relaying
node's for relayed ones, family 0 when the bridge does not know it. `PeerInfo` carries the
same address. xkrd's Hugin relay limits app connections per address with it
(`--hugin-max-clients-per-ip`, `docs/hugin-relay.md` §11).

## 3. Connection ids

`conn_id` is a `u64` that is never reused within one bridge session:

```
bit 63      origin: 0 = allocated by Rust (outbound Open), 1 = allocated by Bare (inbound)
bits 48..62 shard (= header.shard of every frame of this connection)
bits 0..47  per-origin, per-shard counter, starting at 1
```

Outbound: Rust picks the id, sends `Open` with it; Bare answers `Opened` (same id) or
`Close(reason, code)`. Inbound: Bare picks the id and sends `Opened` with `request_id = 0`.
Either side ends a connection with `Close`; the other side answers with its own `Close` once it
has released the id (two-way close, so no Data can arrive for a recycled slot). Frames that
arrive for an id after its owner sent `Close` are dropped silently. Frames for an unknown id
are dropped and answered with `Error(UNKNOWN_CONN)` at most once per id.

## 4. Backpressure (credits)

Per connection and direction, the sender has a window that starts at the negotiated
`initial_credit` bytes (default 1 MiB). Each `Data` frame consumes `len(payload)`; a sender with
a window smaller than the next message waits. The receiver returns credit with `Credit(bytes)`
when the application has *consumed* data (Rust: when the reader has taken the bytes out of the
connection queue, batched until a quarter of the window is consumed or the queue is empty; Bare:
when `stream.write()` returned true or `drain` fired). Exceeding the window is a protocol
violation (`Error(CREDIT_EXCEEDED)` + `Close(4)`).

**Oversized messages.** A message larger than the whole window may be sent when the window is
full (nothing outstanding); the window then goes negative and the sender waits until credits
bring it back. The receiver therefore only flags a violation when it already has uncredited
bytes: `outstanding > 0 && outstanding + len > initial_credit`. This keeps any secret-stream
message up to `max_frame − 16` deliverable without fragmenting `Data` frames. (Rust never writes
more than its window; it splits its byte stream into chunks of at most 64 KiB.)

Control frames are not credited and must stay small, so a reader always drains the transport:
memory per connection is bounded by `max(initial_credit, max_frame)` in each direction, total
memory by `connections × that`. A slow Rust consumer stalls only its own connection (the
worker pauses that secret stream), never the link. Messages larger than
`max_frame − 16` are refused with `Close(4, code 7)`; XKR messages are far below that (lite
blocks, tx relay; full blocks are batched).

## 5. Versioning and startup

Each side sends `Hello` first and waits for the peer's (answering `Ping`s meanwhile); any other
frame before `Hello` is `BAD_FRAME`. `major` must be equal (else `Error(VERSION)` and close); the
session uses `min(minor)` and the AND of both `features` bit sets. `max_frame` and
`initial_credit` are the minimum of both proposals. v1 = major 1, minor 0. Additive changes (new
TLVs, new IGNORABLE frame types, feature bits) bump `minor`; anything else bumps `major`.
A worker drops a Rust side that sends no `Hello` within 10 s.

Liveness: either side sends `Ping` after 10 s of silence and closes the bridge if no `Pong`
arrives within 10 s more; this notices a worker whose event loop hangs. A worker that ends is
noticed at once through its IPC channel.

Worker lifetime: when the Rust side ends the IPC, the worker ends its side too, destroys every
connection, leaves every topic and destroys its swarm; the worklet then exits.

## 6. Hugin `mid` (TLVs 0x10–0x12)

Hugin peers (hugin-node, hugin-desktop, hugin-native) refuse handshakes without a valid `mid`,
carried in the Noise handshake payload (flag 128, 64 bytes after all upstream fields). The
bridge implements the topic and `mid` derivation of hugin-network 3.0 (domain-separated) on
upstream hyperdht (`bridge/src/mid.js`, `docs/hugin-relay.md` §2):

```
topic = cn_fast_hash("hugin/topic" || base_public_key)                 (Keccak-256, 43 bytes in)
mid   = ed25519_sign_detached("hugin/mid" || dht_public_key, base_secret_key)    (41 bytes in)
```

A remote `mid` is accepted only if it verifies against the base public key over exactly
`"hugin/mid" || remote DHT public key`. Clients using the earlier derivation (hyperswarm-hugin:
`topic = fill32(hex(base_public_key))`, `mid` over the bare DHT key) cannot reach a bridge
identity, nor it them: there is no legacy mode and no option for one. This section is the
frame-level contract.

- `0x11` hugin topic key: the 32-byte seed of the topic base key pair
  (`Buffer.alloc(32).fill(key)` in Hugin). The bridge derives the base key pair, checks that
  `topic == cn_fast_hash("hugin/topic" || base_public_key)` (else `Error(DHT_ERROR)`; an
  earlier hex-fill topic is refused this way), creates a **Hugin
  identity** for the topic (its own DHT key pair, because a `mid` signs one specific public
  key), signs that identity's public key as its `mid`, and from then on verifies the `mid` of
  every peer that handshakes with that identity (our rejection: `Close(3, code 5)` on our
  dials, silent drop of inbound handshakes; a remote peer rejecting *our* `mid` drops the
  handshake silently too, which shows up as `Close(1, code 2)` peer connection failed).
- `0x12` identity seed: seed of the identity's DHT key pair. Default: derived by the worker
  from its node key and the topic (stable across restarts when the node key is persisted).
- `0x10` precomputed `mid`: sent as-is instead of signing (only meaningful with `0x12`). Next
  to `0x11` it must verify for the identity's DHT key under the topic base key (i.e. be a `mid`
  of the current derivation), else `Error(DHT_ERROR)` / `Close(ERROR)`. Without `0x11` the
  remote `mid` cannot be verified, so such an identity may only dial out: every inbound
  handshake on it is refused (fail closed).

The bridge also refuses a handshake payload with bytes after the `mid` (the `mid` is the last
field), always applies its `createHandshake` after any caller options, and fails a handshake
whose identity was removed from the registry while it was in flight.

`Announce` with these TLVs joins the topic on the identity; `Open` with them (or with a topic
that already has an identity) dials from the identity. Topics without them use the node key and
plain upstream handshakes. `Ack` and `Opened` report the local public key used (TLV `0x01`).

## 7. Workers

Rust starts N Bare worker threads (bare-kit worklets, each running `bridge/src/worker.js`), each
with its own DHT node, key pair and swarms, and holds one link per worker. Worker `i` of `N`:

- **Hello**: Rust sends `workers = N`, `worker = i`; the worker answers with the same two values
  (Rust refuses a link whose worker answers otherwise, with `BridgeError::Protocol`).
- **`header.shard`**: every frame on the link of worker `i`, control frames included, carries
  `shard = i`; a worker treats a frame for another shard as `BAD_FRAME` (fatal).
- **`conn_id`**: bits 48..62 are `i` for both origins, so Rust routes connection frames by
  `conn_id >> 48 & 0x7fff` without a table and each worker only ever sees its own connections;
  a worker refuses an `Open` whose id names another shard (`Error(BAD_FRAME)`).
- **Topics**: each worker serves a disjoint set of topics. By default the topics without Hugin
  `mid` go to worker 0 and the Hugin topics to the last worker; xkrd runs a single worker, which
  serves them all. Rust decides the routing; nothing on the wire names a topic group.
- **Failure**: any link failing fails the whole bridge (every link).
- **DHT**: every worker is its own DHT node with its own UDP socket and reports it with
  `DhtStatus` on its own link. xkrd runs one worker, so one UDP port serves every topic group.

Frame encode/decode lives in one Rust module (`xkr-p2p-hyper::frame`) and one JS module
(`bridge/src/frames.js`), with shared golden test vectors checked by both test suites.

## Changelog

v1.0 is not deployed anywhere yet, so the clarifications below were folded into v1.0 instead of
bumping `minor`.

- 2026-11-13 (bridge implementation):
  - First transport: a Node.js process over `127.0.0.1` TCP (removed 2026-11-16).
  - `Opened.handshake_hash` is **64** bytes (hyperdht's Noise uses BLAKE2b-512), not 32.
  - `max_frame` bounds the whole frame (`len + 4`), minimum 4096.
  - Credits: oversized-message rule (§4) so messages larger than the window cannot deadlock;
    Rust credit batching documented.
  - TLVs: `0x01` also in `Ack` and `Opened` (local public key); `Open` accepts the Hugin TLVs;
    new `0x12` identity seed; `0x11` defined as the topic base key seed; §6 added.
  - `features` bit0 `HUGIN_MID`; `Close.code` table; `max_peers = 0` means no limit; empty
    lookup = one `LookupResult` with `count 0` + `END`; `LookupResult.addr` is a relay hint.
  - Address encoding, little-endian ports, `Opened` timing and topic semantics, `request_id 0`
    for Hyperswarm auto-dials, silent drop after own `Close`, `conn_id` in `Error` headers.
  - JS module path is `bridge/src/frames.js`; golden vectors in
    `docs/test-vectors/hyper-bridge-frames.json`.
- 2026-11-13 (bridge limits; folded into v1.0, still not deployed): `Hello` TLV `0x02` and
  `Error.code` 7 for the TCP transport (removed 2026-11-16); 10 s `Hello` deadline; at most 64 TLVs per body; mid-only
  identities refuse inbound handshakes, trailing bytes after a `mid` are refused (§6). Rust
  client limits that do not change the wire: at most 16 384 control frames queued for the
  link (the bridge fails beyond), at most 256 inbound connections waiting to be taken
  (further ones are closed), lookups keep at most `max_peers` (4096 for 0) peers.
- 2026-11-13 (xkrd integration): **no frame changes.** `--max-peers <n>` sets Hyperswarm's
  `maxPeers` on the plain swarm (xkrd passes `--hyper-max-peers`); Hugin identities keep the
  default. `bridge/test/testnet-bootstrap.js` gained `--ignore-stdin` for shell scripts. The
  Rust client's terminal-failure semantics are covered by
  `crates/xkr-p2p-hyper/tests/fake_worker.rs`.
- 2026-11-14 (Hugin derivation of hugin-network 3.0): **no frame changes**, but the meaning of the
  Hugin TLVs (§6) changed: the topic checked against `0x11` is
  `cn_fast_hash("hugin/topic" || base_public_key)` and the `mid` signs
  `"hugin/mid" || dht_public_key`; a precomputed `0x10` next to `0x11` must verify. Clients
  using the earlier derivation cannot reach the relay; no legacy mode.
- 2026-11-14 (stripe pruning, `docs/hyper-transport.md` "Stripe topics"): **no frame changes.**
  xkrd joins one more kind of XKR topic, `xkr/<network>/stripe/<n>/<k>/v1`, on the plain swarm
  with the existing `Announce` (mode server), `Lookup` and `Open` frames.
- 2026-11-15 (Bare workers in xkrd): **no frame changes.** Clarified for N workers (§1, §5,
  §7): every frame on a worker's link carries that worker in `header.shard` (control frames
  included), a worker must answer `Hello` with the `workers`/`worker` it was given, and the
  keepalive stays on. The transport is a byte stream (bare-kit IPC), not one ring slot per frame
  as first planned.
- 2026-11-16: the Bare workers are the only transport; the TCP transport, its `PORT` line, TLV
  `0x02` and `Error.code` 7 are gone (the tag and the code stay reserved). No other frame
  changes.
- 2026-11-16 (`--port`; folded into v1.0, still not deployed): new frame `DhtStatus` (0x41,
  B→R, always `IGNORABLE`) with the DHT node's state, firewall and NAT view and local UDP
  port. With `--port-fixed` the worker binds exactly that port (no fallback to another one)
  and reports `state = 2` when it cannot.
