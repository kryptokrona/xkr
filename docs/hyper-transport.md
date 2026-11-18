# Hyper transport: how xkrd uses Hyperswarm/HyperDHT

Wire format: [`hyper-bridge-protocol.md`](hyper-bridge-protocol.md).
Code: `bridge/` (JS, run on Bare worker threads inside xkrd) and `crates/xkr-p2p-hyper` (Rust
side).

## Pieces

```
xkrd ──(Levin, hugin-node protocol, …)── HyperConn (AsyncRead+AsyncWrite)
                                            │  xkr-p2p-hyper: Bridge (one link per worker)
                                            ▼  frames over in-memory channels
                 bridge/src/worker.js (a Bare thread in xkrd: bare-kit worklet + IPC)
                 bridge/src/session.js: the frame protocol
                         HyperDHT + Hyperswarm + Hugin mid layer
                                            │  UDX / holepunching / secret-stream
                                            ▼
                         other xkrd nodes, hugin-node, Hugin apps
```

The JavaScript side runs on Bare worker threads inside xkrd, started through bare-kit's C API
("Bare workers" below); xkrd needs no Node.js.

- `Bridge::start_in_process(config, &BareConfig)` reads or creates the key pair seeds, starts
  the workers, does the `Hello` exchange with each and keeps every link alive with Ping/Pong.
  The workers leave the DHT when xkrd closes the bridge, and end with the process.
- `announce(topic, mode, hugin)` joins a topic; `lookup(topic, max)` lists peers; `open(key,
  topic, flags)` dials; `incoming()` yields everything else (server accepts and Hyperswarm
  auto-dials).
- `HyperConn` is a plain tokio byte stream with credit-based backpressure underneath, so the
  existing Levin stack in `xkr-p2p-legacy` runs over it unchanged (it is a
  `PeerStream`/`BoxedPeerStream`; `Send + Sync + Unpin + 'static`).
- `HyperConn::close_handle()` returns a `CloseHandle` (weak references, cheap to clone) that
  closes the connection after the `HyperConn` itself was moved into a peer task.
- Failure is terminal per bridge: `incoming()` yields one `Err` and ends, `wait_closed()`
  resolves, every `HyperConn` errors, and `announce`/`lookup`/`open` fail immediately
  (`tests/fake_worker.rs`). Reconnecting means a new `Bridge`.

Limits worth knowing: messages up to `max_frame − 16` (16 MiB default) per secret-stream message
(HyperConn splits writes into ≤ 64 KiB `Data` frames, so this only matters for what peers send
us); per connection at most `initial_credit` (1 MiB) of unread data is buffered on each side
of the bridge.

## Topics

| topic | derivation | mode | purpose |
|---|---|---|---|
| `xkr/mainnet/blocks/v1` | `cn_fast_hash(name)` (`topic::xkr_blocks_topic`) | both (`client` with `--hyper-client-only` or `--private`) | XKR peers find each other; Levin over HyperConn |
| `xkr/testnet/blocks/v1` | same | both | testnet |
| `xkr/<network>/stripe/<n>/<k>/v1` | `cn_fast_hash(name)` (`topic::xkr_stripe_topic`) | server (lookup to find) | holders of stripe `k` of `n` (stripe pruning; full nodes announce all of `n` = 4), see "Stripe topics" |
| Hugin node mesh | `HuginTopic::node_mesh()` | both (`client` with `--private`) | hugin-node ↔ hugin-node relay mesh |
| Hugin public nodes | `HuginTopic::public_nodes()` | server (not with `--hugin-private` or `--private`) | Hugin apps find a public node |
| Hugin private node | `HuginTopic::private_node(address)` | server (not with `--private`) | invited Hugin apps |
| p2pool sidechain | `p2pool_topic(network, consensus id)` in `crates/xkrd/src/hyper.rs` | both (`client` with `--hyper-client-only` or `--private`) | the built-in p2pool's sidechain P2P (on by default on mainnet, `--no-p2pool`, `--node --p2pool`), see "The built-in p2pool" |
| private sync | `PrivateTopics::of(secret).sync` (`xkr_p2p_hyper::private`) | server (`--private` only) | peers with the private node's invite; Levin, see "Private node" |
| private Hugin | `PrivateTopics::of(secret).hugin` | server (`--private` only) | Hugin apps with the private node's invite, see "Private node" |
| `xkr/<network>/clients/v1` | topic key `cn_fast_hash(name)`, keyed like p2pool (`topic::xkr_clients_topic`) | server, once synchronized (node on; not with `--hyper-client-only` or `--private`) | chainless xkrd modes (and later tools) use a node's chain, see "Clients topic" |
| private clients | `PrivateTopics::of(secret).clients` | server (`--private` only) | the clients topic for holders of the private node's invite |

Further XKR topics (e.g. separate `txs`/`sync` topics) are added by name with
`topic::xkr_topic`, versioned (`/v1`) so a protocol change can move to a new topic without a
flag day. The Hugin topics must be announced with `Some(topic.mid())`: the bridge then gives each its own DHT identity and adds/verifies the `mid` that hugin-node and the Hugin
apps require, with the topic and `mid` derivation of hugin-network 3.0 (domain-separated; clients
using the earlier derivation cannot reach the relay, `docs/hugin-relay.md` §2).

## Integration in xkrd

Code: `crates/xkrd/src/hyper.rs`. Phase 1 runs the **unchanged Levin protocol** over `HyperConn`,
so there is no new wire protocol: a Hyperswarm peer is an ordinary Levin peer in the P2P manager
(same sync state machine, downloader slots, block/tx relay), only its byte stream differs.

```
xkrd main ── hyper::run(cfg, P2pManager::handle(), shutdown)
               │ Supervisor: start Bare workers, restart with back-off
               │ Session:    announce, lookup, dial, dedup, bans
               ▼
   Bridge::incoming() / Bridge::open() ── HyperConn ── TrackedConn (reports its drop)
               │
               ▼  P2pHandle::add_stream(Box<TrackedConn>, PeerOrigin::Hyper { remote_public_key, topic },
               │                        outbound = info().initiator)
   P2P manager: Levin handshake (connect_stream / accept_stream), then the same peer table
```

### Command line

| flag | default | meaning |
|---|---|---|
| `--no-hyper` | off | legacy Levin/TCP only, no bridge |
| `--hyper-bridge-dir <dir>` | `$XKR_BRIDGE_DIR`, then `bridge/` next to the executable or in its parent directories (`../../bridge` for `target/<profile>/xkrd`) | bridge sources (`src/worker.js`, `node_modules`) |
| `--port <port>` (alias `--hyper-udp-port`) | a free port | the one UDP port of the Hyperswarm network (node sync, Hugin, later p2pool); forward it only behind a very strict firewall ("Ports and firewalls"). Not the legacy TCP port (`--p2p-bind-port`) |
| `--hyper-bootstrap <host:port>` | public HyperDHT bootstrap nodes | repeatable; a local DHT for tests |
| `--hyper-max-peers <n>` | 32 | hard cap on Hyperswarm peers; also the plain swarm's Hyperswarm `maxPeers` |
| `--hyper-client-only` | off | announce mode `client`: look up and dial, never announce (no inbound Hyperswarm peers) |
| `--private` | off | private node: public topics client-only, peers and Hugin apps only through the invite ("Private node") |
| `--private-peer <invite>` | none | repeatable; stay connected to a private node ("Private node") |
| `--node`, `--hugin`, `--p2pool` | none: node, Hugin relay and p2pool (testnet: no p2pool) | mode flags: only the ones named run (`--node --hugin`, `--node --p2pool`, ...); `--p2pool` alone runs p2pool on remote nodes ("Clients topic", client role); `--hugin` needs `--node` for now and the Hyperswarm transport |
| `--no-hugin` | relay on | do not run the Hugin relay on the three Hugin topics ([`hugin-relay.md`](hugin-relay.md)); `--no-hyper` turns it off too; not with mode flags |
| `--hugin-payout <address>` | saved, else the donation address | fee address of the relay's p2pool shares, saved in `<data dir>/hugin/identity.json` |
| `--hugin-p2pool <host:port>` | `127.0.0.1:3333`, then the public p2pool nodes | p2pool stratum nodes for jobs and shares, repeatable, in order |
| `--hugin-share-difficulty <n>` | 200 | app difficulty of the jobs handed to Hugin apps; the p2pool login asks for `max(1000, n)` |
| `--hugin-max-clients <n>` | 200 | Hugin app connections |
| `--hugin-sync-slots <n>` | 8 | Hugin history syncs sent at once |
| `--hugin-queue-budget-mb <n>` | 256 | send budget of all Hugin app connections, in MiB |
| `--hugin-max-clients-per-ip <n>` | 4 | Hugin app connections from one IP address (0: no limit) |
| `--hugin-private` | off | no public-node topic |

The DHT key pair is persisted in `<data dir>/hyper-keypair.json` (JSON `{ seed }`, mode 0600;
"Identity" below), so the node keeps its DHT identity, and with it bans held by
other nodes and its Hugin identities, across restarts. No seed nodes: the DHT bootstrap nodes
are the only fixed entry points. `/status` reports the Hyperswarm peers under `peers.hyper`
(counted by the P2P manager from `PeerOrigin::Hyper`) and the DHT node under `hyper`.

### Ports and firewalls

Holepunching needs no open port, and that stays the default: the DHT node binds a free UDP port
(hyperdht tries 49737 to 49741, then any) and connects through NATs. xkrd runs one Bare worker
whose single HyperDHT node (one UDP socket) carries every topic group: the XKR topics on the
plain swarm with the node key, each Hugin topic on a swarm of its own identity, and later
p2pool. So one port serves the whole Hyperswarm network.

Holepunching fails between two nodes whose NATs pick a new public port for every destination
(a "random" NAT in hyperdht's terms). For a node behind such a NAT, or behind a firewall that
lets no unsolicited UDP in, `--port <port>` binds the DHT to exactly that UDP port (no fallback
to another one) so it can be forwarded; once the DHT's reachability check passes, other nodes
connect to it directly. Forward that one UDP port; nothing else is needed.

| situation | what to do |
|---|---|
| home router, ordinary NAT | nothing (default) |
| xkrd says "Behind a strict firewall: forward a UDP port and start with --port <port>" | forward one UDP port to the machine, start with `--port <that port>` |
| public server | optional: `--port <port>` for a stable, known port |

xkrd says the hint once (in yellow on a terminal, as a log warning with `--log-json`) when the
DHT is ready, reports the node firewalled, has seen the NAT change the public port per
destination, and no `--port` was given. A firewalled node behind an ordinary NAT gets no hint.

The worker reports its DHT node in `DhtStatus` frames (`docs/hyper-bridge-protocol.md`), which
xkrd shows in the console's `status`:

```
Hyperswarm:  firewalled yes, UDP port 49737
Hyperswarm:  firewalled no, UDP port 21890 (--port)
Hyperswarm:  firewalled yes, strict NAT, UDP port 49737
```

and in `/status`:

```json
"hyper": { "state": "ready", "udp_port": 21890, "port_fixed": true,
           "firewalled": false, "nat": "consistent" }
```

`state` is `starting`, `ready` or `failed`; `udp_port` is `null` until the socket is bound,
`firewalled` `null` until the DHT is ready, `nat` `unknown`, `consistent` or `random`; `hyper`
is `null` with `--no-hyper`. `udp_port`, `firewalled` and `nat` are in the answer only for a
client on a loopback address; other clients get `state` and `port_fixed`. A `--port` that is taken stops the start of the bridge with "UDP
port N (--port) is in use", `state` `failed`, and the supervisor retries with its back-off.
`--port` is UDP; the legacy Levin port (`--p2p-bind-port`, TCP 11897) is separate.

### Topic and discovery

- Topic: `xkr/<mainnet|testnet>/blocks/v1` (`topic::xkr_blocks_topic`), announced in mode
  `both` (`client` with `--hyper-client-only`) once per bridge session; announce failures are
  retried every 10 s.
- The bridge's Hyperswarm auto-dials what it finds on the topic (up to `maxPeers`); those
  connections arrive on `incoming()` with `initiator = true`. Server-side accepts arrive with
  `initiator = false` and no topic (the handshake does not carry it); xkrd tags both with the XKR
  topic, since the plain swarm joins only XKR topics (the blocks topic and the stripe topics
  below, which all carry the same Levin protocol).
- A lookup runs every 10 s while there is no Hyperswarm peer, every 30 s below the outbound
  target (`min(8, --hyper-max-peers)`), and not at all above it. Explicit `open`s go only to peers
  that were in the **previous** lookup too and are still not connected (Hyperswarm had a full
  round to auto-dial them), at most 4 in flight, never to ourselves, banned or backing-off keys.
  Failed dials back off per key: 5 s, 10 s, … 10 min.
- The Levin direction follows the Noise direction: the Hyperswarm initiator sends the Levin
  handshake (`add_stream(.., outbound = info().initiator)`), the other side answers it.

### Stripe topics

Stripe pruning (`docs/node-architecture.md`, "Pruned peers"; `docs/storage.md`, "Stripe
pruning") splits the old chain into stripes; a pruned node holds one, a full node all. So that a
syncing node can find the holders of a stripe with one DHT lookup, every stripe has its own
topic:

```
topic(network, n, k) = cn_fast_hash("xkr/<mainnet|testnet>/stripe/<n>/<k>/v1")
```

with `n` = stripe count and `k` = stripe (`1..=n`) as ASCII decimals, the two bytes of the
pruning seed `n << 8 | k` (`topic::xkr_stripe_topic`, `topic::xkr_stripe_topics`; derivation
and golden vectors like the blocks topic, e.g. `xkr/mainnet/stripe/4/1/v1` =
`140a9b48…f5cee3e0`).

- **Announcing**: in every bridge session, next to the blocks topic, the node joins the topics
  of the stripes it holds in **server** mode (findable, no Hyperswarm auto-dials on them): a
  full node all four stripes of the default count (`DEFAULT_STRIPES` = 4), a stripe-pruned node
  its own stripe under its own count, `(seed.stripes(), seed.stripe())`, so a 6-stripe node is
  on `stripe/6/k` (`hyper::held_stripe_topics`). A completely pruned node (`xkrd db prune
  --all`, seed `0x0100` = `PruningSeed::RECENT_ONLY`) holds no old stripe and announces none.
  With `--hyper-client-only` nothing is announced. They are unannounced on shutdown like the
  blocks topic.
- **Looking up**: when the downloader needs a range that no connected peer serves (none holds
  it, or every holder answered it as missed), the P2P
  manager publishes its first height `h` (`P2pHandle::wanted_height`) and the session calls
  `Session::find_stripe_peers(h)`. The stripe of `h` depends on the count (span `n = h / 4096`
  is stripe `n % 4 + 1` of 4 but stripe `n % 6 + 1` of 6), so it looks up one topic per count
  in `LOOKUP_STRIPES` = [4, 6] (`hyper::stripe_lookup_topics`, from
  `downloader::stripe_holders_for`): full nodes and 4-stripe holders answer on the first,
  6-stripe holders on the second. Each lookup asks for at most 16 peers and is followed by
  explicit `open`s with its topic to up to 2 of the keys found, picked at random from the
  answer (`Tracker::sample_stripe_keys`; the DHT's order is whatever a topic flood makes it),
  at once (the two-sightings rule
  of the blocks-topic lookups does not apply, since nobody auto-dials stripe topics), never to
  ourselves, connected, banned or backing-off keys, and beyond the outbound target if needed,
  up to `--hyper-max-peers`. One round of lookups (both counts) runs at a time, each topic at
  most every 15 s, repeated every 15 s while the range stays missing. Full nodes announce only
  the default count: every lookup includes it, so they are always found.
- **Rotation**: only for a **confirmed** range (`Downloader::rotation_range`: the tracker's ids
  there verified in a batch, or a second peer's chain entry carries the same ids; an unconfirmed
  range is downloaded and looked up but rotates nobody out). If a lookup found keys to dial but
  `--hyper-max-peers` is reached (connections plus dials in flight) **and** our own established
  outbound Hyperswarm connections fill the outbound target (`min(8, --hyper-max-peers)`;
  inbound connections and pending dials, which others control, never trigger it), the session
  keeps up to 2 of them (a random sample) and
  asks the P2P manager to drop one outbound Hyperswarm peer that does not serve the range
  (`P2pHandle::rotate_hyper_for_stripe`; the manager picks it with
  `Downloader::rotation_victim`: first a peer that claimed the range and missed it, otherwise a
  pruned peer without it, never an inbound peer, a holder, one of the two peers that delivered
  the most verified blocks or the last peer that reaches the range; within the rotation budget
  shared with TCP, one per 30 s and per span 30 s, 60 s, ... up to 10 min until the span is
  served: `docs/node-architecture.md`, "Pruned peers"). When a connection
  ends, the kept keys are dialed into the freed slot; they are forgotten once nothing is
  missing. A Hyperswarm auto-dial on the blocks topic can still take the slot first; the next
  lookup round tries again.
- **Connections** on a stripe topic (our dials, or accepts on the plain swarm) are ordinary
  XKR peers: `Session::on_conn` treats the blocks topic, our stripe topics and every stripe
  topic we looked up as XKR topics (Levin, P2P manager); `PeerOrigin::Hyper.topic` is the stripe
  topic when known.

No frame or bridge change: stripe topics use the existing `Announce` (mode server), `Lookup`
and `Open` frames on the plain swarm (`docs/hyper-bridge-protocol.md` is unchanged).

### Admission

Every connection the bridge reports goes through `hyper::Tracker::admit` before its Levin
handshake starts, and counts against `--hyper-max-peers` from then on (`Admit::Accept`,
`AcceptEvicting`, `Refuse*`):

- **Per direction.** Outbound connections (our dials, Hyperswarm auto-dials included) are
  admitted up to `--hyper-max-peers` with at most `MAX_PENDING_HANDSHAKES` = 16 Levin handshakes
  pending at once. Inbound connections are admitted up to the **inbound cap**: `--hyper-max-peers`
  minus a reserve of outbound slots, `min(8, --hyper-max-peers / 2)`, that only connections we
  initiate may take, so a full inbound side never stops our own dials. Pending inbound
  connections never refuse an outbound one and vice versa.
- **Pending inbound.** At most `MAX_PENDING_INBOUND` = 8 inbound handshakes are pending at once;
  a further inbound connection is accepted and the one pending longest is closed
  (`AcceptEvicting`; a connection that replaced an earlier one of the same key keeps the earlier
  admission time). The closed connection's later Levin failure counts for nothing.
- **Per address.** From one IP address (IPv6: one /64) at most `MAX_INBOUND_PER_IP` = 4 inbound
  connections (pending and established) and `MAX_PENDING_INBOUND_PER_IP` = 2 pending ones;
  further ones are refused and closed.
- **One connection per key**, resolved as below; a replaced established connection counts as a
  new pending one.
- **Private node.** With `--private` an inbound connection is admitted only if it came in on the
  node's private sync identity (`RejectReason::NotInvited` otherwise); outbound connections are
  admitted as above.

The bridge's own Hyperswarm `maxPeers` is set high (65 535), so these rules, not the bridge,
decide what xkrd keeps.

### One connection per remote key

The remote DHT public key (authenticated by Noise) is the peer identity. When a second
connection to a key arrives, both ends must keep the same one, and the bridge's Hyperswarm
already deduplicates its own connections, so xkrd uses the same rule (`hyper::prefer_new`):

1. if the existing connection is established (its Levin handshake succeeded), the new one wins:
   the remote lost the old one and reconnected, since nobody dials a key it is connected to
   (Hyperswarm's `existingIsOutdated`);
2. otherwise the connection initiated by the peer with the **higher** public key wins
   (Hyperswarm's `expectedInitiator`);
3. if both or neither were initiated that way, the lower Noise handshake hash wins (identical on
   both ends).

The loser is closed through its `CloseHandle`; its Levin failure does not count against the
remote. Explicit dials are rare (rule above), so races with Hyperswarm's auto-dials are rare; when
the ends still disagree (an established/new race), both connections close and the next dial
settles it.

### Bans

Bans are keyed by the remote public key, not by an address, and survive bridge restarts. A
failed `add_stream` is classified by the `PeerError` in its error chain:

| Levin failure | ban |
|---|---|
| `WrongNetwork`, `VersionTooOld` | 1 h |
| `SelfConnection` | 24 h |
| protocol violation (`PeerError::is_protocol_violation`: bad Levin bucket, unexpected message or response, bad ping) | 10 min |
| I/O, timeouts, `Closed`, queue full, anything else | none (dial back-off only) |

Banned keys are closed on arrival and never dialed. Invalid blocks are handled by the P2P manager
like for TCP peers.

### Supervision and shutdown

The supervisor holds the JS side as the `BareWorkers` of the session.

- The session ends when `Bridge::wait_closed()` resolves, `incoming()` yields its `Err`, or a
  worker thread ends. Every `HyperConn` then fails, the P2P manager drops those peers, and
  `open`/`lookup`/`announce` on the dead bridge fail at once.
- The supervisor logs, waits (1 s, doubling to 60 s; reset after a session that lived 2 min),
  starts fresh workers and re-announces. Remote peers find the node again through the DHT.
- A start that fails logs its error once; the same error again is not logged again. After 5
  starts in a row that fail with the same error the supervisor stops trying: the status says
  failed and xkrd says once `Hyperswarm off: <error> (5 tries in a row)`. A start that works
  resets the count. Without the libraries the addons need (see "Requirements") no start is
  tried at all.
- Shutdown (SIGINT/SIGTERM): unannounce (3 s budget), close every Hyperswarm connection, close
  the bridge; each worker's pump sees its link close and ends its worklet, which leaves the DHT
  (the threads are joined for at most 5 s; details under "Failure, liveness and shutdown").
  `main` waits for this at most 10 s. If xkrd dies, the workers end with the process.

### The Hugin relay

Every xkrd with the Hyperswarm transport is also a hugin-node relay unless `--no-hugin` is given
or mode flags without `--hugin` (protocol and design in [`hugin-relay.md`](hugin-relay.md)). `hyper.rs` loads the Hugin identity
(`<data dir>/hugin/identity.json`), starts an `xkr_hugin::Relay` (message pool in
`<data dir>/hugin/messages.jsonl`, p2pool client) once, and in every bridge session announces
the node-mesh (both), public-node (server, unless `--hugin-private`; only once the node is
synchronized) and private-node (server) topics with `Some(topic.mid())`. On the testnet the
three topics are the testnet ones (`HuginTopic::on_network`), so testnet relays never meet
mainnet apps. The bridge gives each Hugin topic its own DHT
identity (the private one with hugin-node's view-key-derived seed, so the invite printed in the
log, `<address><key>`, is stable), and every connection on such an identity arrives with its
topic set. `Session::on_conn` routes by topic: the XKR topic (or no topic, i.e. a plain-swarm
accept) goes to the Levin P2P manager as before; a Hugin topic goes to `Relay::serve` as a node
(mesh) or client (public/private) connection. Hugin connections do not go through the `Tracker` (they are
not XKR peers and do not count against `--hyper-max-peers`); the relay enforces its own limits
(`--hugin-max-clients`, new apps paused while many apps sync, 32 nodes) and bans by DHT key. The relay outlives bridge restarts; on
shutdown the Hugin topics are unannounced with the XKR topic and their connections closed.
The Hugin topics share the worker's DHT node with the XKR topics, each on its own Hyperswarm
instance and identity ("Sharding" below), so they use the same UDP port.

### The built-in p2pool

With p2pool on (plain `xkrd` on mainnet, or `--node --p2pool`; `docs/mining.md`, "Built-in
p2pool") the node also carries the p2pool
sidechain's P2P protocol on the Hyperswarm network, next to its outgoing TCP connections to
other p2pool nodes; a share received on either transport is broadcast on both, so
Hyperswarm-only xkrd nodes and TCP-only C++ p2pool nodes share one sidechain through any node
that has both.

- **Topic key**: `cn_fast_hash("xkr/<network>/p2pool/v1/" || hex(cn_fast_hash("p2pool-topic" ||
  consensus id)))` (`xkr_p2pool::p2p::hyperswarm_topic_key`; `<network>` is `mainnet` or
  `testnet`, the consensus id is the sidechain's: default, mini, nano or a `--p2pool-config`
  sidechain). The id is hashed first, so nodes of a private sidechain only meet each other and
  the topic does not show the id.
- **Swarm**: the topic key is used like a Hugin topic key (`hyper::p2pool_topic`): the DHT topic
  is the Hugin derivation of its base key (`hugin_topic_on`, the testnet domain on the testnet)
  and the topic is announced with `Some(topic.mid())`. The bridge therefore gives the sidechain
  its own DHT identity, on the same worker and UDP port: its connections arrive tagged with the
  topic (server-side accepts of the plain swarm carry none), it keeps its own connection per
  peer next to the Levin one (Hyperswarm keeps one connection per remote key and swarm), and a
  peer's `mid` shows that it knows the topic key.
- **When**: the topic is announced once p2pool runs (after the node is synchronized), in mode
  `both` (`client` with `--hyper-client-only`), and unannounced with the other topics at
  shutdown. The bridge's Hyperswarm auto-dials the peers it finds on it.
- **Connections**: `Session::on_conn` hands every connection on the topic to `crate::p2pool`,
  which attaches it to its P2P server (`P2pServer::attach`, outgoing when we are the Noise
  initiator). The p2pool handshake runs unchanged on it; these peers never appear in peer
  lists, are banned by DHT key, and do not go through the `Tracker` or count against
  `--hyper-max-peers`. Without a running p2pool a connection on the topic is closed.

### Private node

A private node (`--private`) still syncs from and relays to the public network, but takes
Hyperswarm connections only from peers that hold its invite.

- **What is client-only.** The XKR topics (blocks; no stripe topics are announced, as with
  `--hyper-client-only`), the p2pool sidechain topic and the Hugin node mesh are joined in
  mode `client`: the node looks up and dials, its node key never listens. The Hugin
  public-node topic and the address-based Hugin private topic are not joined.
- **What listens.** Three identities derived from the node's secret, each announced in mode
  `server` like a Hugin topic (topic key + identity seed, so the bridge gives it its own DHT key
  pair, signs that key as its `mid` and refuses every handshake whose `mid` does not verify):
  the sync identity (connections go to the Levin P2P manager, like plain Hyperswarm peers) and
  the Hugin identity (connections go to the Hugin relay as apps; announced only with the relay
  on) and the clients identity (connections go to the clients server, "Clients topic"; with the
  node on). `Session::on_conn` routes by the topic the connection arrived on; `Tracker::admit` admits
  inbound Levin connections only on the sync identity.
- **Secret.** `<data dir>/hyper-private.json`, `{ "secret": "<64 hex>", "identity": "<64
  hex>" }`, mode 0600, created with random bytes from the operating system on the first
  `--private` start. `secret` goes into the invite; `identity` stays on the node.
- **Derivation** (`xkr_p2p_hyper::private`; `cn_fast_hash` of an ASCII domain and 32 bytes):

  | value | derivation |
  |---|---|
  | sync topic key | `cn_fast_hash("xkr/private/sync/v1" \|\| secret)` |
  | Hugin topic key | `cn_fast_hash("xkr/private/hugin/v1" \|\| secret)` |
  | clients topic key | `cn_fast_hash("xkr/private/clients/v1" \|\| secret)` |
  | sync identity seed | `cn_fast_hash("xkr/private/sync-identity/v1" \|\| identity)` |
  | Hugin identity seed | `cn_fast_hash("xkr/private/hugin-identity/v1" \|\| identity)` |
  | clients identity seed | `cn_fast_hash("xkr/private/clients-identity/v1" \|\| identity)` |

  Each topic key is used like a Hugin topic key: base key pair
  `crypto_sign_seed_keypair(topic key)`, DHT topic `hugin_topic_on(network, base public key)`
  (the testnet domain on the testnet), `mid` = signature by the base key over
  `"hugin/mid" || DHT public key`. The topics have separate base keys because the bridge
  ties one topic to one base key. The identity seeds give the DHT key pairs the node listens
  with.
- **Invite.** One line, `xkrinv2:` followed by 256 hex characters: the public keys of the sync,
  Hugin and clients identities and the secret, 32 bytes each. Invites of the first version
  (`xkrinv1:` + 192 hex: sync key, Hugin key, secret; no clients key) are still read. The node logs it
  once the sync identity is announced (`private node invite (console: invite)`, info level), and
  the console command `invite` prints it.
  With the Hugin relay on, the node also logs and prints a Hugin app invite
  (`docs/hugin-relay.md`, "xkrd private node"): relay address, Hugin topic key and Hugin
  identity key, which reaches the Hugin identity only.
- **Rotation.** `invite new` in the console writes a new secret and identity to the file and
  restarts the bridge at once (no back-off): every connection closes, the workers leave the DHT
  and the new identities are announced. Invites of the old secret no longer connect; peers that
  hold the new invite reconnect.
- **`--private-peer <invite>`** (repeatable, with or without `--private`): the node dials the
  sync identity of each invite directly with the invite's sync topic key (its own `mid`, signed
  by that key, on an identity the bridge derives from the node key and the topic), at the start
  of every bridge session, every 15 s and whenever a connection ends, unless it is connected,
  being dialed, banned or backing off (5 s doubling to 10 min after a failed dial or
  handshake). These are Levin peers like any other and count against `--hyper-max-peers`.
- **Status.** `/status` `hyper.private` is `true`; the console's `Hyperswarm:` line ends in
  `, private node`.

No frame or bridge change: the identities use the existing `Announce` and `Open` frames with
the Hugin TLVs `0x11` (topic key) and `0x12` (identity seed), and `Opened` carries the topic
(`docs/hyper-bridge-protocol.md` is unchanged).

### Clients topic

Programs without the chain (the chainless xkrd modes `--p2pool`, later `--hugin` without
`--node`; later wallet tools on a `wallets` topic of their own with the same framing) use a node
with the chain over Hyperswarm. Code: `crates/xkr-hyper-rpc` (protocol, server driver, client),
`crates/xkrd/src/clients.rs` (the node's server), `crates/xkrd/src/remote.rs` (xkrd as a
client, "Client role" below).

- **Topic.** Name `xkr/<mainnet|testnet>/clients/v1`; topic key `cn_fast_hash(name)`
  (`topic::xkr_topic`); used like a Hugin topic key (base key pair from it, DHT topic
  `hugin_topic_on(network, base public key)`, `mid` signed by the base key;
  `topic::xkr_clients_topic`), so the topic gets its own DHT identity and its connections arrive
  tagged. A private node announces `PrivateTopics::of(secret).clients` with its clients identity
  instead (key in the `xkrinv2:` invite).
- **Who announces.** Every xkrd with the node and Hyperswarm, in mode `server`, once the node is
  synchronized (`synced` and not downloading); not with `--hyper-client-only` or `--private`
  (then only the private clients topic, announced at once). Clients look up and dial; they
  never announce.
- **Limits.** `--max-clients <n>` (default 200, as `--hugin-max-clients`) connections, at most 8
  per IP address (IPv6: /64); per connection 8 calls in flight (more: error `-32000`), 20 calls/s
  with a burst of 40 (more: `-32001`), 64 refused calls in a row close the connection; 10 s for
  the hello; frames of at most 1 MiB. A client that sends a bad frame or keeps calling past its limits is closed and its
  DHT key refused for 10 min. These connections do not go through the `Tracker` and do not count
  against `--hyper-max-peers`.
- **Status.** `/status` `clients: { connections, served_calls }` (`null` when not served); the
  console's `status` has a `Clients:` line.

#### Wire format (version 1)

Every message is one **frame**: a `u32` little-endian length `n` (1 ≤ n ≤ 1 048 576), then `n`
bytes of UTF-8 JSON, one object. A longer or empty frame, or JSON that is not a message below,
closes the connection. Unknown object keys are ignored.

1. **Hello** (client, first frame):

   ```json
   {"v":1,"network":"mainnet","service":"node","client":"xkrd/1.0.0"}
   ```

   `service` is `node`, `stratum` or `wallet` (the last only on the wallets topic); `client` is
   for logs.

2. **Hello reply** (server), or a refusal after which the server closes:

   ```json
   {"v":1,"height":2641203,"hash":"<64 hex>","synced":true,"services":["node","stratum"],
    "state_span":644,"state_hash":"<64 hex>","pruning_seed":0}
   {"v":1,"error":"service","message":"this node serves node"}
   ```

   `height` is the index of the top block and `hash` its hash; `synced` is the node's sync
   state; `services` lists what this node serves now (`stratum` only while its built-in p2pool
   runs); `state_span`/`state_hash` are the newest stable span with a state hash (`getstatehash`,
   `null` before the first); `pruning_seed` is the raw seed (`stripes << 8 | stripe`, 0 = full).
   Refusal `error`: `version`, `network`, `service`, `busy`, `hello` (first frame not a hello).

3. **Calls**, any number in flight, answered in the order they finish (match by `id`, an
   unsigned integer below 2^53 for JavaScript; a non-integer `id` closes the connection):

   ```json
   {"id":1,"method":"getblockheaderbyheight","params":{"height":5}}
   {"id":1,"result":{"block_header":{...},"status":"OK"}}
   {"id":2,"error":{"code":-32601,"message":"getblocktemplate is not served on the clients topic"}}
   ```

   `params` is what the method takes in `/json_rpc` (object or array; absent = `null`). Errors
   of the methods keep their JSON-RPC codes (`-1` wrong parameter, `-2` too big height, ...);
   the protocol's own: `-32601` method not served, `-32603` internal, `-32000` busy (calls in
   flight), `-32001` rate limited, `-32002` answer larger than a frame, `-32003` service not
   available yet.

4. **Pushes** (server, no `id`):

   ```json
   {"method":"tip","params":{"height":2641204,"hash":"<64 hex>","synced":true}}
   ```

   sent on every new main-chain tip (service `node`). A push that finds the connection's send
   queue full is dropped; the next one carries the newer tip.

Service `node` answers these `/json_rpc` methods, through the node's dispatcher in-process:
`get_miner_data`, `get_block_headers_range`, `getblockheaderbyheight`, `getblockheaderbyhash`,
`submitblock` (`["<block blob hex>"]`), `getinfo` (the `/getinfo` answer) and `getstatehash`
(`{"span":k}` or `{"height":h}`), and one method of its own, `get_block_blob`
(`{"height":h}` -> `{"height":h,"hash":"<64 hex>","blob":"<hex>"}`: the main-chain block at
`h` as stored, header, parent block, miner transaction and transaction hashes, without the
transactions), from which a client computes the block's id and proof of work. Service
`stratum` negotiates, but its calls answer `-32003` for now.

#### Client role: `xkrd --p2pool` without the node

Code: `crates/xkrd/src/remote.rs` (`RemoteNodes`, `run_hyper`, `BridgeDialer`). Without the
node xkrd starts the Bare workers without the blocks, stripe and Hugin topics and without the
Levin P2P manager. It announces only the built-in p2pool's sidechain topic (once p2pool runs;
client mode with `--hyper-client-only`), and uses the clients topic as a client:

- **Finding nodes.** A lookup of `xkr/<network>/clients/v1` every 30 s while fewer than three
  nodes are connected (every 10 s with none); each found key is dialed with
  `Bridge::open_hugin(key, topic, mid)` on the topic's derived identity, never announced. Hello
  `{"v":1,"network":…,"service":"node","client":"xkrd/<version>"}`. At most three nodes, with
  different keys. A failed dial or a closed connection backs off 5 s doubling to 10 min.
- **Keeping them.** The `tip` pushes move the tip; a node that sent nothing for a minute is asked
  `getinfo`, one that answers nothing for three minutes is dropped.
- **State hashes.** A hello whose `state_span`/`state_hash` differs from the list built into
  xkrd (`xkr_chain::state`), or, for a span above that list, from what more than half of the
  connected nodes naming that span say (at least two), drops that node; it is not dialed for an
  hour.
- **Tip.** The highest tip that at least two synchronized nodes have or build on (a node above
  it is asked `getblockheaderbyheight` at that height); with one node, its tip. Before a new tip
  is used: `get_block_headers_range` from the previous checked block (at most the newest four
  blocks) to the tip must link by `prev_hash`, and every new block (`get_block_blob`) must hash
  to its header's `hash`, carry its `prev_hash` and `timestamp`, and meet its header's
  `difficulty` with its proof of work (merge-mining tag included). The nodes on a tip that
  fails are dropped and not dialed for an hour.
- **Use.** `get_miner_data` and the PPLNS headers from a node at the tip; a block found by
  p2pool goes to every connected node with `submitblock`. Synchronized: a node answers and is
  synchronized, and the tip was confirmed (push, answer) less than three minutes ago.
- **Status.** `/status` `remote` and the start-up step `Remote nodes` (`3 nodes, height …`;
  `docs/mining.md`, "p2pool without the node").

Rust clients use `xkr_hyper_rpc::Client::connect(stream, Hello::new(network, service, agent))`,
then `call(method, params)`, `tips()` (a watch channel with the tip of the hello, then every
push) and `closed()`.

### What comes later

- **Phase 2 (native protocol over Protomux)** replaces the Levin framing on Hyperswarm
  connections; `add_stream` and the `PeerEvent`/`SyncAction` interface of the manager stay, and
  so does everything in `hyper.rs` (discovery, dedup, bans, supervision).
- **A pure-Rust HyperDHT** ("Later" at the end) is local to `xkr-p2p-hyper`.

## Two-node test (no open ports, no seed nodes)

`testnet/scenarios/20-hyper-two-nodes.sh <work dir>` in the regression suite
(`docs/testnet-suite.md`; needs a built `xkrd` and libbare-kit, it never builds):

1. starts a local HyperDHT (`bridge/test/testnet-bootstrap.js --ignore-stdin`);
2. starts **A**: `--network testnet`, peered with the suite's own C++ seed node (127.0.0.1:16898,
   mined from genesis) or `XKR_CPP_PEER`, `--hyper-bootstrap <local>`;
3. starts **B**: `--network testnet --p2p-bind-port 0 --out-peers 0 --hyper-bootstrap <local>`
   and a `--seed-node` on a dead port, so B has no legacy peer at all (a legacy connection on
   B is a FAIL);
4. waits until B's height and hash equal A's or `TIMEOUT` (default 900 s), then prints the
   Hyperswarm lines of both logs.

State goes to `<work dir>/20-hyper-two-nodes/` (data directories `a`, `b`, logs `a.log`,
`b.log`, `bootstrap.log`). Everything runs under `nice -n 19`. `KEEP=1` keeps the nodes running
after success (e.g. to watch live blocks relayed A → B), `XKRD=` picks the binary.
`testnet/hyper-two-nodes.sh <work dir>` is a wrapper for the same scenario.

**Public-DHT variant** (both nodes find each other through the public HyperDHT bootstrap nodes,
no `--hyper-bootstrap`):

```
PUBLIC_DHT=1 testnet/scenarios/20-hyper-two-nodes.sh /tmp/xkr-hyper-public
```

The testnet topic `xkr/testnet/blocks/v1` keeps it apart from mainnet nodes. Expect the first
connection after a few seconds to a minute (announce flush plus Hyperswarm's or our lookup).

## Desktop app packaging (Electron / Tauri)

`xkrd` is meant to run as a sidecar of a desktop app (a child process the app starts), so the
bridge must travel with it: `xkrd`, `libbare-kit` next to it (found through the
`$ORIGIN`/`@executable_path` search path; on Windows `bare-kit.dll` next to `xkrd.exe` or on
`PATH`) or on the loader's search path, and the `bridge/` bundle (sources and `node_modules`,
`packaging/scripts/bundle-bridge.sh`). `BridgeConfig` finds `bridge/` next to the executable,
through `--hyper-bridge-dir` or `$XKR_BRIDGE_DIR`. The native addons (`sodium-native`,
`udx-native`) ship as prebuilds for linux/win32/darwin, so no compiler is needed on the user's
machine, and no Node.js. `packaging/tauri-sidecar/` is a Tauri sample.

## Bare workers

xkrd starts one Bare worker (`HyperConfig::bare_config`). The bridge supports N workers, one per
topic group, each with its own DHT node; earlier versions put the Hugin topics on a second
worker. One worker is enough: Hyperswarm needs one swarm per key pair, not one DHT node per key
pair (`new Hyperswarm({ dht, keyPair })`), the Hugin traffic is small next to block sync, Hugin's
share hashing runs in Rust, and one DHT node means one UDP port to forward and half the DHT
traffic.

The frame protocol does not care what carries it: frame types, body layouts, TLVs, credit rules
and golden vectors are those of `docs/hyper-bridge-protocol.md`, and the JS modules
`session.js`, `frames.js`, `mid.js` and `swarm.js` implement them. Each Bare worker runs them
over bare-kit's IPC channel; the Rust `Bridge`/`HyperConn`/`CloseHandle` API is what xkrd sees.
(An earlier version ran the same modules in a Node.js child process over a local TCP socket;
that transport is gone.)

### How it works

- `Bridge::start_in_process(config, &BareConfig)` (`crates/xkr-p2p-hyper/src/bare/`) reads or
  creates each worker's key pair seed and starts N threads `xkr-bare-<i>` (xkrd: N = 1). Each
  thread creates a bare-kit worklet and the host end of its IPC channel (`bare_worklet_create`,
  `bare_ipc_create`), starts it running `bridge/src/worker.js` with its arguments
  (`bare_worklet_start`), and then pumps bytes between that IPC and an in-memory channel
  (`xkr_p2p_hyper::transport::mem_channel`, 1 MiB per direction).
- The other end of each in-memory channel is an ordinary bridge link: `Bridge::connect_streams`
  does the `Hello` exchange with every worker (`workers = N`, `worker = i`; a worker that answers
  with another index is refused) and runs a reader, a writer and a keepalive task per link.
- In the worklet, `worker.js` builds a `SwarmManager` from its seed and one `Session` over
  `Bare.IPC`, the duplex stream bare-kit sets up in the worklet for the host's IPC.
- bare-kit's IPC is a byte stream (`bare_ipc_write` takes a whole buffer or nothing,
  `bare_ipc_read` copies up to the given length and a message may arrive in several reads), so
  frames go back to back and `len` delimits them.
- The pump sleeps on the in-memory channel and is woken by the Rust side (bytes to send, room
  freed), by bare-kit's IPC poll callback (`bare_ipc_poll_start`, readable/writable; it runs on
  a thread owned by the IPC) and by the worklet's exit callback. If bare-kit refuses the poll,
  the pump checks the IPC every 1 to 8 ms while idle (`BareConfig::ipc_poll = false` forces
  that).

### Sharding: topic groups and workers

`Bridge` routes every request to one worker:

| topic | worker |
|---|---|
| announced or opened with Hugin `mid` TLVs | the last worker (`Bridge::hugin_worker`); remembered, so later opens, lookups and unannounces of that topic go there too |
| assigned with `Bridge::assign_topic(topic, i)` (call before the first announce) | `i` |
| everything else (blocks topic, stripe topics, `open` without topic) | 0 |

Control frames carry their worker in `header.shard`, connections in `conn_id` bits 48..62, so
each `HyperConn` talks to the link of its own worker, and `incoming()` merges all workers into
one stream. xkrd runs one worker, so the last worker is worker 0 and every topic group (XKR
topics on the plain swarm, the Hugin topics on their identities, later p2pool) shares its DHT
node and UDP port. All XKR topics stay on worker 0 so XKR peers see one DHT identity: the
duplicate rule, self-detection and bans use `Bridge::node_public_key`, which is worker 0's key.

### Identity

Every worker is its own DHT node (own UDP socket) with its own key pair. Worker 0 uses
`<data dir>/hyper-keypair.json` (the node key); worker `i` of several uses
`hyper-keypair.<i>.json` next to it (xkrd, with one worker, uses only the first; a
`hyper-keypair.1.json` from an earlier version is not read). xkrd reads the `seed` from the file (or creates `{ "seed": "<hex>" }`, mode 0600; files of
earlier versions also carry `publicKey`, which is not read) and passes it in the worker's arguments, which
stay in memory (a worklet has no process command line). The fixed UDP port (`--port`) goes to
worker `i` as port + `i` (so xkrd's one worker binds exactly `--port`); `--hyper-max-peers`
applies to worker 0 only. The Hugin identities derive from the worker's key, so with one worker
they derive from the node key (the private-node identity still comes from the view key).

### Failure, liveness and shutdown

- Failure is terminal for the whole bridge: when one link dies (its
  worklet ended, an IPC error, keepalive timeout, protocol error) every link is failed,
  `incoming()` yields one `Err` and ends, and the supervisor restarts all workers with the
  usual back-off.
- The 10 s + 10 s Ping/Pong keepalive stays on: it is how a worker whose event loop hangs is
  noticed. A worklet that exits (or ends or closes its side of the IPC) shows up in its pump as
  end-of-stream on read or a closed IPC on write, which closes the link at once; the exit code
  from bare-kit's exit callback (`bare_worklet_on_exit`) goes into the log line.
- An uncaught exception or unhandled rejection in a worklet would abort the whole process
  (bare-kit's default); `worker-guard.js` (loaded first by `worker.js`) catches both, logs
  them to the system log, sends the error's name and message on one line as one `Error` frame
  (`request_id` 0, code `DHT_ERROR`), ends its IPC and exits that worklet with code 1 (after at
  most 1 s), which the pump then sees as above.
- `worker.js` has no static imports: Bare resolves those before any module runs, so a package
  missing from `node_modules` would abort xkrd before the guard is in place. It loads
  `worker-guard.js` and then `worker-main.js` with `import()`, whose failure is an ordinary
  rejection. Before it starts a worker, xkrd also checks that every package in the bridge's
  `package.json` is in `node_modules` and names the missing ones. `xkrd bridge-check` starts
  one worker without joining the DHT and stops it again (the release smoke test).
- A worker whose modules do not load (a native addon that cannot load) fails before its
  `Hello`, so that `Error` frame is the first frame it sends. The pump looks at each worker's
  frames until the `Hello`; an `Error` before it becomes the reason `start_in_process` returns
  (`cannot start the bridge: Bare worker 0: AddonError: ...`) instead of "closed during Hello".
  After the `Hello` the bridge logs such a frame as a worker error.
- Shutdown: `Bridge::close()` shuts every link; each pump sees its link end, closes it, and
  half-closes the IPC (`bare_ipc_end`). `worker-main.js` ends its side in turn, its session closes and
  the swarm leaves the DHT, and the worklet exits on its own. A worklet that has not exited after
  2 s is terminated (`bare_worklet_terminate`) and gets 2 s more; then the poll, the IPC and the
  worklet are destroyed (`bare_worklet_destroy` never blocks). `BareWorkers::stop` joins the
  threads for at most 5 s. There is no "leave the DHT" frame: xkrd unannounces its topics
  before closing.
- At exit, after the runtime stopped, xkrd calls `xkr_p2p_hyper::bare::teardown`
  (`bare_worklet_teardown`, once, waiting at most 2 s), which frees the JS platform the worklets
  share; it does nothing if no worker was started. The supervisor's restarts happen before that,
  so they are not affected.

### The C API used (`src/bare/ffi.rs`)

The only unsafe code of the crate, in one module with `#![allow(unsafe_code)]` (the rest of the
crate is `deny(unsafe_code)`). Every
declaration comes from `include/bare-kit.h` of bare-kit 3.1.0 (tag `v3.1.0`), and every name is
in its `src/exports.json`, the list of symbols `libbare-kit` exports. The call order follows the
usage example in bare-kit's README and its C tests (`test/worklet-ipc-host-end.c`,
`worklet-exit-code.c`, `worklet-teardown.c`).

| function | used for |
|---|---|
| `bare_worklet_create(&options, &worklet)` (`version = 0`, `memory_limit`, platform fields null), `bare_worklet_destroy` | worklet handle; destroy terminates a running worklet and returns at once |
| `bare_worklet_on_exit(worklet, cb, data)` | exit code, called once on the worklet thread after it tore down; `data` is a reference-counted state the callback releases |
| `bare_worklet_start(worklet, filename, NULL, 0, argc, argv)` | run `worker.js` from disk with arguments (`Bare.argv`) |
| `bare_worklet_terminate` | stop a worklet that did not exit within 2 s of the IPC end |
| `bare_worklet_teardown` | once at process end (`bare::teardown`) |
| `bare_ipc_create(worklet, &ipc)`, `bare_ipc_end`, `bare_ipc_destroy` | host end of the IPC (created before start), half-close on shutdown |
| `bare_ipc_read(ipc, buf, len)` → bytes / 0 = end-of-stream / `bare_ipc_would_block` (-1) | bytes from the worker into a 64 KiB buffer (`BARE_IPC_READ_BUFFER_SIZE`) |
| `bare_ipc_write(ipc, data, len)` → bytes / -1 / `bare_ipc_error` (-2, closed) | bytes to the worker |
| `bare_ipc_poll_create`, `_destroy`, `_set_data`, `_get_data`, `_start(poll, events, cb)`, `_stop` | wake the pump when the IPC is readable or writable; destroy waits for a running callback, so the callback's data is freed after it |

## Building

### libbare-kit

Nothing is downloaded or built by cargo: `build.rs` of xkr-p2p-hyper only links a library that
is already there, from `LIBBARE_DIR` (and stops with the steps below when the variable is
missing or the directory has no `libbare-kit.so`, `libbare-kit.dylib` or `bare-kit.lib`;
`LIBBARE_NAME` overrides the name). Build it once per checkout:

```sh
scripts/build-bare-kit.sh            # --docker to build in a container; --force to rebuild
```

The script clones bare-kit at commit `64ca2a9` (tag `v3.1.0`) into `target/bare-kit/src`,
installs its npm packages and bare-make 2.1.2 (into `target/bare-kit/tools`; through Socket
Firewall, `$(npm prefix -g)/bin/sfw`, when it is installed), runs `bare-make generate`, `build`
and `install`, and copies the shared library to `target/bare-kit/lib`. On NixOS (and with
`--docker`) the three bare-make steps run in a `node:24-bookworm` container (3 CPUs, 4 GiB, as
your user), because the clang that bare-make brings cannot link there. The build uses at most
3 parallel jobs. `cargo clean` removes `target/bare-kit` as well; run the script again after it.

`.cargo/config.toml` sets `LIBBARE_DIR` to `target/bare-kit/lib` (relative to the repository
root) for every cargo command in this repository, so no variable has to be exported. A
`LIBBARE_DIR` set in the environment wins (the entry has no `force`), for a library built
elsewhere; give it as an absolute path.

bare-kit builds for desktop as well as mobile: its CI builds and tests Linux (x64, arm64),
macOS (x64, arm64) and Windows (x64, arm64) next to Android and iOS, and Pear uses it. The
build makes one shared library, `bare-kit` (`libbare-kit.so`, `libbare-kit.dylib`, or
`bare-kit.dll` with the import library `bare-kit.lib`), plus a static `libbare-kit.a`. The shared
library is self-contained: Bare (pinned by bare-kit, 1.37.1 for 3.1.0), V8, libuv and the
builtin Bare modules are linked into it, and it exports Bare's symbols for native addons.

bare-make is Holepunch's CMake wrapper. Installed with npm, it brings its own CMake (4.x;
bare-kit's `CMakeLists.txt` needs CMake 4.0 or later), Ninja and LLVM (clang, lld) through npm
packages, so the machine needs git, Node.js with npm, and network access for the first
`generate` (CMake fetches Bare and its dependencies). By hand, the script does roughly this:

```sh
git clone https://github.com/holepunchto/bare-kit target/bare-kit/src
git -C target/bare-kit/src checkout --detach 64ca2a9479db82fb3ce4d9e877c34e22cc492d72
(cd target/bare-kit/src && sfw npm install)
mkdir -p target/bare-kit/tools target/bare-kit/lib
(cd target/bare-kit/tools && sfw npm install --save-exact bare-make@2.1.2)
cd target/bare-kit/src
../tools/node_modules/.bin/bare-make generate   # Release build for this machine
../tools/node_modules/.bin/bare-make build
../tools/node_modules/.bin/bare-make install    # into ./prebuilds (lib/, bin/ on Windows, include/)
cp -a prebuilds/lib/libbare-kit.so* ../lib/
```

On macOS the script checks that `libbare-kit.dylib` has an install name starting with `@rpath/`
(`otool -D`) and sets it with `install_name_tool -id @rpath/libbare-kit.dylib` otherwise. On
Windows (Git Bash) it copies `bare-kit.dll` and `bare-kit.lib`; Windows finds the DLL next to
`xkrd.exe` or on `PATH`, so copy it to `target/<profile>/` (the script prints this).

### xkrd

```sh
scripts/build-bare-kit.sh                                  # once
cargo build -j 3 -p xkrd                                   # debug: runs as built
XKR_DEV_RPATH=1 cargo build -j 3 --release -p xkrd         # release that runs from target/
target/release/xkrd ...                                    # Hyperswarm on Bare workers
```

Every build of xkr-p2p-hyper (and so of xkrd, also for `--no-hyper` use) needs libbare-kit.
The executable links the library dynamically. xkrd's `build.rs` gives it the run-time search
paths of the executable's own directory and its `lib/` (`$ORIGIN` and `$ORIGIN/lib` on Linux,
`@executable_path` and `@executable_path/lib` on macOS), so a `libbare-kit` shipped next to
`xkrd` or in `lib/` is found. Debug builds, and release builds with `XKR_DEV_RPATH=1`, also get
the absolute `LIBBARE_DIR` after those, so `target/<profile>/xkrd` runs as built without
`LD_LIBRARY_PATH`; a plain release build (the one that is packaged) searches only next to the
executable, and `packaging/scripts/package-release.sh` sets its run path to
`$ORIGIN:$ORIGIN/lib` with `patchelf` when that is installed. A release built without
`XKR_DEV_RPATH=1` runs from `target/release` with `LD_LIBRARY_PATH=target/bare-kit/lib`.
xkr-p2p-hyper's `build.rs` does the same for its own tests and examples. Windows has no search
path in the binary: `bare-kit.dll` goes next to `xkrd.exe` or on `PATH`. Native addons that a
worklet loads resolve Bare's symbols from the loaded `libbare-kit`.

The bridge directory is found as before (`--hyper-bridge-dir`, `$XKR_BRIDGE_DIR`, next to the
executable) and must contain `src/worker.js` and `node_modules`; Bare loads the `.bare`
prebuilds of `sodium-native` and `udx-native` that are already in `node_modules`. No Node.js is
needed.

### Requirements

- **Linux: `libstdc++.so.6`.** The `.bare` prebuilds of `udx-native` and `sodium-native` are
  C++ libraries that need it. Normal Linux systems have it on the default library path (it
  comes with gcc's runtime package, e.g. `libstdc++6` on Debian and Ubuntu).
- Before the first worker starts, xkrd loads it once for the whole process
  (`xkr_p2p_hyper::bare::check_runtime`, `dlopen` with `RTLD_NOW | RTLD_GLOBAL`). It tries, in
  this order, each file by its absolute path:
  1. `libstdc++.so.6` in each absolute directory of `NIX_LD_LIBRARY_PATH`;
  2. `/run/current-system/sw/lib`;
  3. `lib/` next to the executable, then the executable's own directory (a release archive can
     ship the library there);
  4. last, the soname through the loader's own search (default directories,
     `LD_LIBRARY_PATH`, the executable's run-time path).

  The loader then uses that copy for the addons, found by its soname, so no `LD_LIBRARY_PATH`
  is needed.
- If none of these works, xkrd does not start the bridge. The Hyperswarm status says failed and
  xkrd says once (on the terminal, or as a warning with `--log-json`):
  `Hyperswarm off: libstdc++.so.6 not found (needed by the Hyperswarm addons). Install it, or on
  NixOS add gcc's lib directory to LD_LIBRARY_PATH.` The rest of the node keeps running.
- **NixOS** has no default library path. When neither `NIX_LD_LIBRARY_PATH` (set by
  `programs.nix-ld`) nor the system profile has the library, run xkrd with gcc's lib directory
  on `LD_LIBRARY_PATH`:

  ```sh
  LD_LIBRARY_PATH="$(dirname "$(gcc -print-file-name=libstdc++.so.6)")${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
      target/release/xkrd ...
  ```

  (`testnet/lib.sh` does this for the xkrd processes of the suite.)
- macOS and Windows: nothing is checked; the addons use the system's C++ runtime.

## Tests

- `cargo test -p xkr-p2p-hyper` (needs `scripts/build-bare-kit.sh` once, like every build):
  `src/transport.rs` (the in-memory channel), `tests/mem_transport.rs` (the frame layer over it
  with fake workers on plain threads: Hello with the worker index, requests, an inbound
  connection, data and credits through a 7-byte channel, two-way close; topic groups on two
  workers, `header.shard` and `conn_id` naming the worker, merged `incoming()`; one worker ending
  fails the bridge; a wrong worker index is refused; `DhtStatus` frames reaching
  `Bridge::dht_status`), `tests/fake_worker.rs` (bridge failure as
  the supervisor sees it, the lookup cap), the unit tests of `src/bare/mod.rs` (key pair files,
  worker arguments incl. `--port-fixed` per worker, fixed ports past 65535 refused), the golden
  vectors and property tests of `src/frame.rs` / `tests/prop_frame.rs` (`DhtStatus` included),
  `src/private.rs` (invite round trip and refusals, derivation, the secret file).
  `tests/loopback.rs` runs two real bridges on a local DHT (including a private node's
  identities with and without the invite's `mid`) and is ignored by
  default (it needs `node` for the DHT bootstrap script and `bridge/node_modules`):
  `cargo test -p xkr-p2p-hyper --test loopback -- --ignored --test-threads=1`.
- `cargo test -p xkrd`: the Hyperswarm configuration (`one_bare_worker_for_every_topic_group`,
  `port_reaches_the_worker`, the `DhtStatus` view and the `--port` hint, the private node:
  `private_config_from_the_command_line`, `the_private_node_keeps_and_renews_its_secret`,
  `a_private_node_admits_inbound_only_on_its_identity`,
  `private_peers_are_redialed_with_back_off`) and the command line (`port_flag`,
  `private_flags`), the console's `Hyperswarm:` line, `invite` and the display's hint; `cargo test -p xkr-rpc
  --test endpoints -- status_hyper_object status_fields_for_loopback` for `/status` `hyper`.
- `cd bridge && npm test` (Node.js 22 or later runs the same modules as Bare):
  `session.test.js` (two workers on a local DHT over in-memory streams; `--port-fixed` on the
  shared DHT and a taken port reported as failed; a plain topic and a Hugin identity on one
  worker's DHT), `worker.test.js` (`worker.js` arguments, the worker index in `Session`,
  `DhtStatus` after `Hello`), `frames.test.js`, `mid.test.js`,
  `lifecycle.test.js`.
- End to end: scenarios 20 and 40 of the testnet suite.

## Limits of the Bare workers

- One DHT node and UDP socket per worker: two workers would mean two nodes' worth of DHT traffic
  and two ports; xkrd runs one.
- One JS thread carries block sync and the Hugin relay; secret-stream encryption of every
  Hyperswarm connection runs on it.
- Worker logs go to the system log (`console.error` through bare-kit's `bare-system-logger`),
  not to xkrd's `tracing`.
- No process isolation: a crash in a native addon ends xkrd; a worklet that does not end on
  terminate cannot be killed (its thread is left running).
- The `.bare` prebuilds of `sodium-native` and `udx-native` in `bridge/node_modules` must be
  built for a Bare ABI that the Bare inside `libbare-kit` (1.37.1) loads.

## Later

A pure-Rust HyperDHT once one proves interop (including the Hugin `mid`); the swap stays local
to `xkr-p2p-hyper` because the rest of xkrd only sees `HyperConn`.
