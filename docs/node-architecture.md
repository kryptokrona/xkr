# xkrd node architecture

Status: written 2026-11-13, including the full legacy peer, block templates, the built-in miner,
wallet RPC and `xkr-wallet-core`. Crates: `xkr-chain`, `xkr-txpool`, `xkr-rpc`,
`xkrd`, `xkr-wallet-core`, plus
additive changes to `xkr-p2p-legacy` (managed download mode, transport-generic connections) and
`xkr-consensus` (`check_proof_of_work_with_hash`). The design borrows Cuprate's split
(`binaries/cuprated`, `p2p/p2p/src/block_downloader`, `consensus/`): components are crates with
narrow, data-oriented interfaces; state lives in `xkr-chain` (storage + a small in-memory window)
and in the single-owner tasks of `xkrd`.

## Crates and who owns what

| crate | owns | interface |
|---|---|---|
| `xkr-storage` | the redb file | `Store`, `Reader` (MVCC snapshot), `ops::{push_blocks, pop_blocks, put_alt_block, ...}` |
| `xkr-chain` | `Store`, header window (last 4096 `BlockInfo`s), tip, alternative blocks, the writer lock | `Chain::{add_blocks, add_block, submit_transaction, reader, tip, ...}`, `NodeView` (legacy `ChainView`), `ChainSnapshot` (pool view) |
| `xkr-txpool` | pool transactions, key-image index, seen/deleted filters | `TxPool::{add, remove_for_block, readd, clean, ...}` over a `PoolChainState` trait |
| `xkr-p2p-legacy` | nothing global: per-connection tasks and a pure per-peer sync machine | `LegacyPeer`, `PeerHandle`, `PeerEvent`, `sync::step` |
| `xkr-rpc` | nothing: reads `Chain`, `watch`es of `NetStatus` and `MiningStatus`, sends `RpcCommand`s | `serve(config, state, shutdown)` |
| `xkr-wallet-core` | nothing: keys, addresses, scanning, transaction construction as pure functions | `Address`, `WalletKeys`, `scan_outputs`, `build_transaction` |
| `xkrd` | the P2P manager (peer table, peer list, bans, downloader), the chain thread, the miner, the runtime | CLI, `P2pHandle` |

`xkr-chain` depends on `xkr-txpool` (it keeps the pool consistent with every commit) and on
`xkr-p2p-legacy` only to implement its `ChainView` trait. Nothing depends on `xkrd`.

## Threads, tasks and channels

```
                         +------------------------- tokio runtime (xkr-io-*, min(cores,4)) ----------------------------+
 TCP peers  <-------->   | connection task x N  --PeerEvent (mpsc 4096)-->  P2P manager task  --ChainJob (unbounded)-->  |---+
 (Levin)                 |   (LegacyPeer::spawn)  <--notify (mpsc 1024/peer)--  (peer table, PeerList,                    |   |
                         |                                                    PeerSync per peer, Downloader)             |   |
                         | accept loop / dial tasks --Internal (unbounded)-->                                            |   |
                         | spawn_blocking: serve GET_OBJECTS, pool admission --Internal-->                               |   |
                         | RPC (axum) --RpcCommand (unbounded)--> manager;  manager --NetStatus (watch)--> RPC          |   |
                         +---------------------------------------------------------------------------------------------+   |
                                    ^ ChainEvent (unbounded)                                                                |
                                    |                                                                                       v
                         chain thread (xkr-chain, one OS thread) -- Chain::add_blocks / add_block --> rayon pool (xkr-verify-*, cores-1)

 miner controller (xkr-miner) -- Chain::block_template --> job --> hashing threads (xkr-mine-*, --mine-threads)
        |  found block: RpcCommand::SubmitBlock --> manager --> ChainJob::Block{local} --> chain thread --> relay (hop 0)
```

* **Connection tasks** (`xkr-p2p-legacy::peer`): one per peer; answer timed sync and ping
  themselves, forward protocol notifications as `PeerEvent`s, read our `CORE_SYNC_DATA` and peer
  list head from `watch` channels the manager publishes. A full outbound queue (1024 messages
  or 32 MiB, the C++ write-buffer limit) drops the peer. Incoming messages pass per-peer token
  buckets and a per-connection read limit.
* **P2P manager** (`xkrd::p2p`): the only owner of the peer table. Runs every `PeerEvent` through
  the peer's `sync::step` (with a fresh `NodeView` = one read snapshot) and carries out the
  resulting `SyncAction`s. Never blocks: storage-heavy answers (`NOTIFY_REQUEST_GET_OBJECTS`,
  `NOTIFY_MISSING_TXS`, pool admission with ring signatures) run on `spawn_blocking` behind
  semaphores sized from `--verify-threads`, one job per peer at a time.
* **Chain thread** (`xkrd::chain_worker`): the single writer. Takes `ChainJob::Batch` (downloader)
  and `ChainJob::Block` (relayed blocks) in order, calls `Chain::add_blocks` / `Chain::add_block`,
  replies with `ChainEvent`s. CPU work inside fans out to the global rayon pool; tokio worker
  threads never run verification.
* **RPC** (`xkr-rpc`): axum handlers; every storage read runs on `spawn_blocking` with its own
  snapshot, so it never waits for the writer (redb MVCC).
* **Miner** (`xkrd::miner`, only with `--mine`): plain OS threads, see `docs/mining.md`.
* **Shutdown**: SIGINT/SIGTERM cancels a `CancellationToken`; the manager closes peers, writes
  `p2pstate.json` and returns, dropping the job sender; the miner threads stop; the chain thread
  finishes its current job and does a durable commit; `main` joins it and flushes again.

## The peer manager (full legacy peer)

`xkrd::p2p::P2pManager` is a complete C++-compatible node on the Levin network, so C++ nodes can
sync from xkrd, not only the other way round.

**Inbound path.** `accept_loop` (one task per listener) closes connections from banned
addresses at once (the manager publishes the ban set in a `watch`), and, before reading
anything, connections beyond `--in-peers` / `--max-conns-per-ip` (addresses we dial are exempt
so their back-pings pass); it runs at most 64 handshakes concurrently, 3 per address (IPv6:
per /64), and per connection calls `LegacyPeer::accept` (first message within 10 s): a C++ back-ping (a fresh connection
carrying one `COMMAND_PING`) is answered and closed; a `COMMAND_HANDSHAKE` is checked (network
id, `P2P_MINIMUM_VERSION`, not ourselves) and answered with our node data, `CORE_SYNC_DATA` and
peer-list head within `connection_timeout * 3` = 15 s. The manager then applies its limits
(`--in-peers`, `--max-conns-per-ip` with loopback exempt, bans, duplicate peer ids) and starts the
connection task. Unlike C++, which back-pings *before* answering the handshake, we answer first
and back-ping the announced `my_port` in the background (`peer::ping`, 10 s budget); success
whitelists `ip:my_port` (`append_with_peer_white`), failure graylists it. Private addresses are
only pinged with `--allow-local-ip` (always on for testnet).

**Serving.** Timed syncs are answered by the connection task (our sync data + peer-list head).
`NOTIFY_REQUEST_CHAIN` (sparse chain -> `findBlockchainSupplement`, up to 10 000 ids),
`NOTIFY_REQUEST_TX_POOL` and `NOTIFY_MISSING_TXS` go through the per-peer sync machine;
`NOTIFY_REQUEST_GET_OBJECTS` (up to 100 blocks / 31 MiB, like C++ requests) is served on
`spawn_blocking`, with only the blocks held in full ("Pruned peers" below). New main-chain
blocks are relayed to every other peer in the normal state, `NOTIFY_NEW_LITE_BLOCK` to P2P
version >= 4 and `NOTIFY_NEW_BLOCK` to older ones, with `hop + 1` (hop 0 for blocks mined here,
like C++ `on_submitblock`); accepted pool transactions with `NOTIFY_NEW_TRANSACTIONS`.

**Outbound.** Every 2 s `maintain_connections` keeps `--add-peer` / `--add-exclusive-node`
connected and fills `--out-peers` slots: 70 % from the white list, then gray, then seeds (seeds
only while the lists hold fewer than 16 addresses or nothing is connected). A failed address
backs off exponentially (5 s doubling, at most 1 h; 5 min for explicit peers); a failing white
entry is demoted to gray, a failing gray entry removed. Outbound handshakes merge the remote
peer list (`fix_time_delta` + `merge_peerlist`) and whitelist the peer (`set_peer_just_seen`).

**Bans.** `--ban-time` (default 86 400 s, Monero's `P2P_IP_BLOCKTIME`; the C++ node has no ban
list) for: invalid blocks (relayed or in a sync batch), sync-protocol violations (the sync
machine's `Disconnect`), downloader violations, and *malformed or oversized Levin messages*: the
connection task reports `PeerEventKind::Misbehaved` when it ends with
`PeerError::is_protocol_violation` (bad signature/flags, a bucket above the per-command limit, a
body that does not decode, an unsolicited response, a second handshake). A banned address is
removed from both peer lists. Explicit peers are never banned, except briefly when they are on
an older fork (below).

**Timeouts.** TCP connect 5 s (`P2P_DEFAULT_CONNECTION_TIMEOUT`), handshake 15 s, request/response
2 min (`P2P_DEFAULT_INVOKE_TIMEOUT`), and an idle timeout of 4 min without any received message
(`P2P_IDLE_TIMEOUT`; both sides send a timed sync every 60 s, so only dead peers hit it).

**Persistence.** `<data-dir>/p2pstate.json` (our own versioned JSON, not the C++
`p2pstate.bin`): peer id (kept stable like C++), white and gray lists (with the pruning seed of
stripe-pruned entries), unexpired bans. Written
every 5 minutes (temp file + rename, on `spawn_blocking`) and on shutdown, read once at start;
the saved white list drives outbound attempts before the seeds.

**Peers at a fork.** The rule applies only after a *block-version* fork (a fork-table entry with
`Rules::MAJOR_VERSION`, today fork 13 = major version 6, `docs/forks.md`) that our main chain has
passed, i.e. our tip index `T > F` where `F` is the fork height (block `F + 1` is the first v6
block). Before that, peering is unchanged. After it, a peer is **on an older fork** and is
dropped with the warning `peer is on an older fork` when

1. its top index (`CORE_SYNC_DATA.current_height - 1`, from the handshake and every timed sync,
   in both directions every 60 s) is `<= F` and has not grown for `OLDER_FORK_STALL` = 150 s
   (more than two timed-sync intervals, so a node that is still syncing up to the fork is not
   hit; the observation is kept per IP and peer id for an hour, so reconnects do not reset it
   and an up-to-date node behind the same IP does not reset it either), checked on
   every handshake and timed sync and every 10 s for all peers; or
2. it sends a block (relayed or in a sync batch) that is refused with `WRONG_VERSION` because its
   major version is **older** than the height requires (`RejectReason::older_block_version`). A
   *newer* version before the fork is an ordinary invalid block (24 h ban).

The ban is short (`OLDER_FORK_BAN` = 10 min, capped by `--ban-time`), applies to explicit
peers too (so `--add-peer` does not redial an old node every 2 s; the ban list blocks dialling),
and is skipped for loopback addresses (dropped only). An unpatched C++ v1.2.2 node behaves the
other way round on its own: it cannot deserialise a v6 block (`serializeBlockHeader` throws
"Wrong major version" for major > 5), logs `failed to parse and validate block` (sync) or
`Deserialization of Block Template failed` (lite block relay), closes the connection and stays at
height `F`; it never bans.

**Block-rate monitor.** Every 60 s, when synced, `Chain::block_rate` counts main-chain blocks
with a timestamp in the last hour; below a third or above three times the expected
`3600 / DIFFICULTY_TARGET` = 40 the manager warns (at most every 10 min). Fixed-difficulty
networks (testnet) report `fixed-difficulty` and never warn. `/status.block_rate` shows the
same numbers. Every 10 min an upcoming scheduled block-version fork is logged with its height,
blocks left and readiness, and entering one is logged once.

**Per-origin accounting.** Every peer carries a `PeerOrigin`; `NetStatus` counts legacy outgoing,
legacy incoming and Hyperswarm peers separately (`/status` `peers.{legacy_outgoing,
legacy_incoming,hyper,total}`, `/getinfo` `hyper_connections_count` next to the C++ counters).

## The sync pipeline

1. **Need**: a peer's handshake or timed sync carries a top id we do not know. Its `PeerSync`
   (in `managed_download` mode) goes to `Synchronizing` and emits `SyncEvent::SyncRequired`
   instead of requesting the chain itself.
2. **Chain tracker**: the downloader sends `NOTIFY_REQUEST_CHAIN` to one peer, ranked
   (`Downloader::request_chain`) by the fewest *chain strikes* (a strike when a peer's entry
   added nothing while it announced more; after `CHAIN_STRIKE_LIMIT` = 2 the peer is dropped
   without a ban), then the tracker's current source (continuations), then the most verified
   blocks delivered, then the highest announced height counted only up to the **estimated
   network height** (`network_height_estimate`: the highest height at least two peers announce;
   with one peer, its height), then the oldest connection. The `NOTIFY_RESPONSE_CHAIN_ENTRY`
   (up to 10 000 ids) is forwarded by the sync machine as `SyncEvent::ChainEntry`. The tracker
   keeps the ids from the first unknown one. When fewer than 4 000 unrequested ids remain and
   the entry did not reach the peer's top, the next request carries the tracker's last id in
   front of our sparse chain, so the peer continues where the tracker ends although those blocks
   are not stored yet (Cuprate does the same). Tracker ids count as **confirmed** below the
   height where a batch verified, or where a chain entry of another peer (a continuation, or a
   second entry asked for after 10 s when the next range has no holder) carries the same ids;
   stripe rotation and the stall reset act on confirmed ranges only, and a stall in an
   unconfirmed range gives the tracker's source a chain strike. The same estimate is the
   `network_height` of `/status` and `/getinfo` and decides when the node counts as synced.
3. **Download**: every idle usable peer whose height covers the next range gets one
   `NOTIFY_REQUEST_GET_OBJECTS` of up to 100 ids (`BLOCKS_SYNCHRONIZING_DEFAULT_COUNT`). One
   request per peer in flight; with 8 peers, 8 batches download in parallel. A response with
   missing blocks marks that peer as unable to serve from that height and re-queues the batch
   for another peer; a request not answered in 60 s is re-queued and the peer dropped. Batches
   never cross a 4 096-block stripe span and go only to peers that hold them in full ("Pruned
   peers").
4. **Reorder buffer + backpressure**: responses go into a `BTreeMap` keyed by start height. New
   ranges are requested only while the buffer holds less than 96 MiB and the range starts less
   than 20 000 blocks above the next block to verify. The batch at the head of the queue is
   always requested, so one slow peer cannot stall the window (it times out and is re-queued).
5. **Verification queue**: consecutive buffered batches are merged into jobs of up to 1 000
   blocks; at most 2 jobs are queued at the chain thread. While the chain thread verifies job
   *n*, job *n+1* is waiting and the network keeps filling the buffer.
6. **`Chain::add_blocks`** (one job):
   1. *prepare*, parallel over blocks (rayon): decode, block id, coinbase hash, PoW slow hash
      (`block_long_hash`), every transaction decoded, hashed and `validate_semantic`-checked
      (amounts, key-image subgroup). The id must equal the requested id.
   2. *context*, sequential: leading blocks already stored are skipped. For each block, the
      context (timestamps incl. genesis, LWMA/Zawy difficulty window, last 100 sizes, generated
      coins) is sliced from the in-memory header window, which already contains the blocks
      accepted earlier in the same job. Checks in C++ `Core::addBlock` order: cumulative size,
      `validateBlock` (version, parent block, timestamps, coinbase), difficulty, transaction list
      consistency (1 750 000 rule), mixins (with the unlock-window grace), reward, checkpoint,
      `check_hash(long_hash, difficulty)` + merge-mining proof. Then inputs: key images unique
      in the block, unspent in storage and in the job's overlay; ring members resolved through
      `Reader::output_keys` or the overlay of outputs created earlier in the job (global indexes
      per amount, miner transaction first, exactly like `push_blocks` assigns them); unlock
      times; signature counts. Each input becomes a signature job.
   3. *signatures*, parallel: every ring signature of the job (`check_ring_signature`) on rayon.
   4. *store*: the accepted prefix (everything before the first failure) becomes
      `VerifiedBlock`s and is written with one `push_blocks` in one redb write transaction.
      During sync, commits are non-durable (`Durability::None`, no fsync) with a durable commit
      at least every 5 s; live blocks and reorgs always commit durably. A crash loses at most the
      last seconds of sync and leaves a consistent shorter chain.
7. **Result**: an invalid block bans the peer that sent it (`sources` maps heights to
   connections) and resets the downloader (new generation, stale responses and results are
   ignored). When the tracker is complete and everything is verified, synchronizing peers whose
   top we now know get `SyncInput::DownloadFinished` (normal state, pool request); if a peer
   still leads, a new round starts.

`--fast-sync` skips only the slow hash (and the PoW target check), only for blocks that arrive
in a **requested sync batch** (`Chain::add_blocks` with the expected ids) and only for heights up
to the last hard-coded checkpoint; a single relayed block (`NOTIFY_NEW_BLOCK`, lite blocks, a
mined block) always has its proof of work checked, as does every alternative block. Signatures,
amounts and double spends are always verified. Checkpoints (genesis plus the mainnet fixture
blocks up to 2 617 400) are always enforced: a block with another hash at a checkpoint height is
rejected, and an alternative block or reorg is refused when the main chain has already reached a
checkpoint at or above its height (Monero's `is_alternative_block_allowed`,
`Checkpoints::reached_at_or_above`); below the next checkpoint ahead of the tip the heavier
branch wins as usual. Between two checkpoints a fast-synced block's proof of work is only
implied by the checkpoint that follows; Monero's per-chunk "hash of hashes" is the next
step for fast sync.

Mainnet performance notes: the slow hash (CN-Turtle-Lite v2) and ring signatures are the CPU
cost and run on all cores; storage writes are one transaction per job without fsync; the header
window avoids storage reads for consensus windows; `Reader` lookups for ring members are B-tree
point reads.

## Pruned peers

Stripe pruning (storage side in `docs/storage.md`, "Stripe pruning") makes a node
keep the ring signatures of one *stripe* of the old chain: height `h` is in stripe
`(h / 4096) % stripes + 1`, the seed is `stripes << 8 | stripe` (0 = full node, `0x0100` =
`PruningSeed::RECENT_ONLY`, a node pruned completely that keeps only its newest blocks whole).
Such a node cannot serve the blocks of the other stripes, so the network side has to know who
holds what. Code: `xkr_p2p_legacy::wire::CoreSyncData::pruning_seed`,
`sync::serve_get_objects_held`, `xkrd::downloader` (`PeerSlot::holds` / `claims`,
`Downloader::unserved`, `stripe_holders_for`, `Downloader::rotation_victim`,
`Downloader::may_rotate`), `xkrd::p2p` (`PeerEntry::pruning_seed`, `rotate_for_stripe`,
`drop_for_stripe`), `xkrd::peerstore::SeedBook`, `xkrd::hyper` (stripe topics,
`stripe_lookup_topics`).

**Advertisement (Levin).** `CORE_SYNC_DATA` gets an optional third field `pruning_seed` (u32,
the key Monero uses in the same struct), in the handshake request and response and in both
timed-sync messages, so both ends learn the other's seed at connect time and again every 60 s.
It is written last and only when non-zero: a full xkrd node's messages are byte-identical to
the C++ ones (`wire/tests.rs` checks the C++ bytes unchanged and the field present/absent), and
a missing key decodes as 0. C++ compatibility, checked in the reference source
(`src/serialization/kv_binary_input_stream_serializer.cpp`, `json_input_value_serializer.cpp`,
`p2p_protocol_definitions.h`): `parseBinary` loads the whole section into a `JsonValue` tree
(`loadSection` inserts every key; only an unknown *type tag* throws, and UINT32 = 6 is a known
one), then `CORE_SYNC_DATA::serialize` looks up `current_height` and `top_id` by name. Nothing
enumerates the keys or rejects extra ones, so a C++ node ignores `pruning_seed`; our own epee
reader skips unknown keys the same way. C++ nodes never send the key, so they count as full,
which they are. The seed is kept per connection (`PeerEntry::pruning_seed`, updated by every
timed sync), and for TCP peers under their peer-list address (`ip:my_port` of an inbound peer)
in a `SeedBook` of at most 4 096 addresses (the one seen longest ago is forgotten first; connected
peers refresh theirs at every timed sync) and in `p2pstate.json` (`"pruning_seed"` on white/gray
entries), so a restarted node knows which addresses are pruned.

A database pruned completely (`xkrd db prune --all`) stores and announces the reserved seed
`0x0100` (`PruningSeed::RECENT_ONLY`: stripe count 1, stripe 0), never 0: it holds no stripe,
`/status.pruning` says `full: false, recent_only: true`, and it announces no stripe topic on
Hyperswarm. Older xkrd nodes fail to decode the value (and, before this fix, treated invalid
seeds as full); C++ nodes ignore the key. On the
receiving side every invalid non-zero seed (a count outside 2..=8, stripe 0 or above the count,
bits above 16) is read as `RECENT_ONLY` too (`downloader::peer_seed`), never as full: such a peer
is only asked for its recent blocks, and `/status.pruning.pruned_peers` counts it.

**Serving.** `NOTIFY_REQUEST_GET_OBJECTS` is answered with the blocks for which
`Reader::has_full_block` is true; every other requested id goes to `missed_ids` (the same place
unknown ids go), never a block without its signatures. Choice (a), answer partially, over (b),
drop the requester, because:

* the C++ requester treats both the same. `handle_response_get_objects` never reads
  `missed_ids`: it checks that every returned block was requested, and when
  `m_requested_objects` is not empty afterwards it logs "returned not all requested objects"
  and sets `state_shutdown`, without processing the blocks it did get. It closes the
  connection; it has no ban list, reconnects later and syncs from its other peers (each C++
  connection syncs on its own, so one unhelpful peer does not stall it);
* (a) is what the C++ server itself does for blocks it does not have (`Core::getBlocks` fills
  `missed_ids`), and it keeps the connection for everything the pruned node *can* do: chain
  entries, pool sync, block and transaction relay, and xkrd requesters, whose downloader
  re-queues missed ids to another peer (a miss is never a protocol violation).

The pruned node never claims more than it has: its `CORE_SYNC_DATA` height and top are its real
chain, `NOTIFY_REQUEST_CHAIN` is answered normally (block ids and headers are complete on a
pruned node, and xkrd downloaders use pruned peers as chain sources), and no block is served
that it does not hold. The C++ protocol has no capability field a C++ node would read, so a C++
node may still pick a pruned peer for an initial sync; it then gets one partial answer and
moves on. Rejected alternatives: truncating chain entries or lowering `total_height` for
requesters without a seed (it would lie about the chain, cannot tell a C++ node from a full
xkrd node since both send 0, and a C++ node that gets a chain entry without new ids asks for
the chain again in a loop). Pruning never touches the newest 8 192–12 287 blocks (nor anything
above the last checkpoint), so live relay, lite blocks, `NOTIFY_MISSING_TXS` and the catch-up
of nodes that are less than ~8 000 blocks behind are served in full by every node; only long
initial syncs meet missed blocks.

**Downloader.** Every `PeerSlot` carries the peer's seed. `PeerSlot::holds(start, end)` is true
for full peers, for a range inside the peer's stripe (`PruningSeed::holds_range`; together the
peer's *claim*, `PeerSlot::claims`), and for ranges within the peer's last `PRUNED_TIP_BLOCKS` =
2 × 4 096 − 512 blocks (an assumption from its height: the storage bound keeps at least 8 192,
minus slack for a top that moved since the last timed sync). Then:

1. batches end at the next multiple of `STRIPE_SPAN` (4 096), so a batch belongs to one stripe
   (every 41st batch is 96 blocks instead of 100);
2. a batch, re-queued or new, is only sent to a peer that holds it; idle peers that delivered
   the most verified blocks on this connection are served first, then full peers, then by
   height (a claimed seed buys no priority before the peer delivered);
3. when a pruned peer cannot serve the next tracker batch, that batch is set aside in the
   re-queue (no failure counted) and the next one is tried while the download window (20 000
   blocks, 96 MiB) allows, so the peer works on a later range of its own stripe while full peers
   or holders of the other stripes fill the gap in order;
4. when connected peers reach the lowest needed range but none that holds it can be asked
   (only pruned peers of other stripes, or every holder answered it as missed this round),
   `Downloader::unserved` names the range, `Downloader::missing_stripe` its stripe for the
   default count (4), a warning is logged at most once a minute, and the round stays open, so it
   resumes as soon as a holder connects. Nothing is ever requested from a peer that cannot
   serve it;
5. a missed answer re-queues the batch for another peer and never counts as a branch failure
   (only time-outs and closed peers do, three per batch reset the download). If the peer's seed
   claims the range, the miss is its fault: the per-connection record (it survives resets)
   counts it, and the second such miss drops the peer without a ban. A miss in the assumed
   recent window does not count. When every connected holder of the next range has missed it
   for 30 s (`STALL_RESET`), the download starts over (the tracked branch is likely gone).

`holds` works for any stripe count, since every count uses the same 4 096-block spans: a
6-stripe peer holds span `n` if its stripe is `n % 6 + 1`, a 4-stripe peer if its stripe is
`n % 4 + 1`, so the same stripe number means different spans under different counts (stripe 1
of 4 holds span 4, stripe 1 of 6 does not; both hold span 12). Looking for holders therefore
names one seed per count, `stripe_holders_for(h)` = `PruningSeed::holding(n, h)` for every `n`
in `LOOKUP_STRIPES` = [4, 6].

While a range is missing, and once the tracker's ids for it are confirmed (a verified batch, or
a second peer's chain entry with the same ids; `Downloader::rotation_range`), the P2P manager:

* publishes its first height for the Hyperswarm transport (`P2pHandle::wanted_height`), which
  looks up the stripe topics of its 4- and 6-stripe holders (`docs/hyper-transport.md`, "Stripe
  topics");
* skips peer-list addresses whose remembered seed lacks the range when it dials (addresses with
  a matching seed or none remembered are dialed);
* **rotates** peers that do not serve the range out, within a budget (`Downloader::may_rotate`,
  shared by both transports): at most one rotation per 30 s, and for the same 4 096-block span
  30 s, 60 s, 120 s, ... up to 10 minutes after the previous one, until a peer delivers blocks
  of that span (then the span's budget starts over). It drops, without a ban, the outbound peer
  `Downloader::rotation_victim` picks, so the slot goes to a holder. TCP: when every outbound
  slot (`--out-peers`) is taken and the peer lists hold an address that may have the range
  (`rotate_for_stripe`, every 2 s on the tick); `maintain_connections` then dials such an
  address. Hyperswarm: when a stripe lookup found holders, `--hyper-max-peers` is reached and
  our own *established outbound* Hyperswarm connections fill the outbound target (inbound
  connections and dials in flight never trigger it), the transport asks for it
  (`P2pHandle::rotate_hyper_for_stripe`) and dials a random sample of the holders found as soon
  as a connection ends.

`Downloader::rotation_victim` (unit-tested, from the downloader's view: usable peers, this
round's misses and the per-connection records) only picks an outbound, non-explicit peer of the
transport that is full (never inbound or `--add-peer`/`--add-exclusive-node` peers):

1. first a peer that holds the range (by its seed, or its recent blocks) but answered it as
   missed, whatever its seed (fewest delivered blocks, then newest connection);
2. otherwise a stripe-pruned or recent-only peer that does not hold the range and could be
   asked for it, only while another usable peer that has not missed the range reaches it (a
   peer that missed it never counts), and never
   one of the two candidates that delivered the most verified blocks (anchors). Among the
   candidates it prefers a peer whose seed another *outbound* peer also has (inbound peers
   cannot steer the choice), then the fewest delivered blocks, the lowest chain and the newest
   connection.

**RPC.** `/getinfo` has `pruning_seed` (raw, Monero's key; 256 for a completely pruned node).
`/status.pruning`: `{ seed, full, recent_only, stripes, stripe, pruned_peers, missing_stripe }`
(`stripes` and `stripe` are `null` unless the seed keeps a stripe; `missing_stripe` is the raw
seed of the stripe the downloader waits for, `null` otherwise). Blocks and transactions whose
signatures were pruned go to `missed_tx` in `/gettransactions` and fail details and explorer
requests with a "block N is pruned on this node" status, never an internal error.

Limits: the `/status` report (`missing_stripe`) uses the default count; lookups cover 4 and
6 stripes, so a node with another count (2, 3, 5, 7, 8) is used fully over Levin and announces
its own topic but is not looked up. Stripe dials may exceed the Hyperswarm outbound target, up
to `--hyper-max-peers`. A slot filled by inbound or explicit peers is never freed. On a pruning
or pruned database `--max-reorg-depth` is clamped to 7 168 at start. Testnet
coverage: `testnet/scenarios/33-pruned-peers.sh` (`docs/testnet-suite.md`); the rotation and the
Hyperswarm lookups are covered by unit tests only.

## Live mode

* `NOTIFY_NEW_BLOCK` and completed `NOTIFY_NEW_LITE_BLOCK`s (missing transactions are fetched
  with `NOTIFY_MISSING_TXS` by the sync machine, from our pool or chain otherwise) become
  `ChainJob::Block`. `Chain::add_block` returns `AddedToMain`, `AddedToAlternative`,
  `AddedAndSwitched`, `AlreadyExists`, `Orphan` or `Invalid(reason)`; the outcome is fed back
  with `SyncInput::BlockProcessed` (orphan -> the peer hands over to the downloader again,
  invalid -> drop). Blocks that changed the main chain are relayed with `hop + 1`
  (`relay_block_messages`: lite blocks to version >= 4 peers, full blocks to older ones) to every
  other peer in the normal state.
* `NOTIFY_NEW_TRANSACTIONS` -> pool admission on `spawn_blocking` -> accepted ones are relayed to
  every other normal peer. `/sendrawtransaction` does the same through `RpcCommand`.
* We serve C++ peers that sync from us: `NOTIFY_REQUEST_CHAIN` (sparse chain lookup,
  `findBlockchainSupplement`), `NOTIFY_REQUEST_GET_OBJECTS` (raw blocks from storage, on
  `spawn_blocking`), `NOTIFY_REQUEST_TX_POOL`, `NOTIFY_MISSING_TXS`.

## Alternative blocks and reorgs

* A block whose parent is a main-chain block below the tip, or another alternative block, is an
  alternative block. Its context is built along its own branch (main-chain headers up to the
  split point, alternative headers above it, Monero's `handle_alternative_block`), and everything
  that does not depend on the branch's UTXO state is checked: header, coinbase, timestamps,
  difficulty, reward (fees from `validate_semantic`), checkpoint, PoW. It is stored with
  `put_alt_block` and indexed in memory (hash -> height, parent, `BlockInfo` with cumulative
  difficulty and generated coins). Alternative blocks do not survive a restart (the C++ node
  keeps them in memory only).
* When an alternative branch's cumulative difficulty exceeds the main chain's (strictly, as C++),
  `switch_to`:
  1. refuses if the split is deeper than `--max-reorg-depth` (default 1 000, warnings above 60;
     node policy, not consensus) or below a checkpoint the main chain has reached
     (`Checkpoints::reached_at_or_above`; an alternative block at such a height is already
     refused with `RejectReason::BelowCheckpoint` when it arrives, Monero's rule), or when it
     would pop a pruned block with transactions (`docs/storage.md`);
  2. reads the main blocks above the split back as `VerifiedBlock`s and pops them
     (`pop_blocks`, durable commit);
  3. pushes the branch through the full main-chain pipeline (all transaction and signature
     checks against the post-pop state);
  4. on success, the old main blocks become alternative blocks and their transactions go back to
     the pool if still valid; on failure the pushed part is popped, the old blocks are re-pushed,
     and the invalid alternative blocks are dropped.
  The two commits are not one transaction; a crash in between leaves a valid shorter chain that
  resyncs.

## Pool consistency

C++ guards the core with one big shared mutex. Here the writer and the readers never block each
other, except for one small gate: a block commit and the matching pool update
(`remove_for_block`: included transactions and key-image conflicts) hold `pool_gate` for writing;
`Chain::submit_transaction` holds it for reading while it validates against a snapshot and
inserts. So a transaction can never be admitted against a state that a concurrent commit just
invalidated.

## Plugging in `xkr-p2p-hyper`: `P2pHandle::add_stream`

Phase 1 of the Hyperswarm transport (`docs/hyper-transport.md`) runs the **same Levin protocol** over Hyperswarm
streams. `xkr-p2p-legacy` accepts any stream (`PeerStream` = `AsyncRead + AsyncWrite + Unpin +
Send + 'static`, boxed as `BoxedPeerStream`), and the manager exposes one entry point for other
transports:

```rust
let p2p: P2pHandle = manager.handle();          // before manager.run(); Clone + Send + Sync
let conn_id: u64 = p2p
    .add_stream(stream /* BoxedPeerStream */, PeerOrigin::Hyper { remote_public_key, topic: Some(topic) },
                /* outbound = */ initiator)
    .await?;                                    // Result<u64, AddStreamError>
```

* `add_stream` runs the Levin handshake over the stream — `LegacyPeer::connect_stream` (we send
  `COMMAND_HANDSHAKE`) when `outbound`, `LegacyPeer::accept_stream` otherwise — with the same
  timeouts and checks as TCP, then hands the `Handshaked` connection to the manager, which
  registers it exactly like a TCP peer: same sync machine, downloader slots, relay and bans.
  It returns the connection id once the peer is registered.
* `PeerOrigin` is stored per peer: `LegacyTcp { addr }` or `Hyper { remote_public_key: [u8; 32],
  topic: Option<[u8; 32]> }`. Hyperswarm peers get a synthetic `PeerInfo::addr`
  (`hyper_synthetic_addr`: `fd` + the first 15 key bytes as IPv6, port from key bytes 15-16), so
  bans and logs are keyed by the public key; logs print `hyper:<first 8 key bytes>`.
* Differences for Hyperswarm peers: their timed-sync peer lists are not merged (discovery is the
  DHT's job), they are never back-pinged, never enter the white/gray lists, do not count against
  `--in-peers`/`--out-peers`/`--max-conns-per-ip`, and are counted in `NetStatus::hyper`. Their
  admission is the transport's (`hyper::Tracker::admit`, `docs/hyper-transport.md`
  "Admission"): one live connection per remote key, `--hyper-max-peers` counted from admission
  on (pending Levin handshakes included), at most 16 outbound and 8 inbound handshakes pending
  at once (a ninth inbound one closes the one pending longest), at most 4 inbound connections
  and 2 pending ones per IP address (IPv6: per /64), and a reserve of outbound slots
  (`min(8, --hyper-max-peers / 2)`) that inbound connections cannot take.
* Errors (`AddStreamError`, `std::error::Error` with `source()` = the `PeerError` for handshake
  failures): `Handshake(PeerError)` (wrong network, old version, self-connection, timeout, I/O;
  `PeerError::is_protocol_violation` tells misbehaviour apart), `PingOnly` (the remote only
  back-pinged), `Refused(reason)` (banned, duplicate peer id, limits), `Stopped` (manager gone).
* The peer is closed when the stream ends, on a protocol violation, or on shutdown.

Phase 2 (a native protocol over Protomux) replaces only the framing; the manager keeps the same
`PeerEvent`/`SyncAction` interface.

## RPC surface (`xkr-rpc`)

C++-compatible: `/getinfo` `/info` `/getheight` `/height` `/fee` `/feeinfo` `/peers` `/getpeers`
`/getwalletsyncdata` `/get_global_indexes_for_range` `/get_transactions_status` `/get_o_indexes`
`/gettransactions` `/sendrawtransaction` `/getrandom_outs` `/get_pool_changes_lite`
`/get_pool_changes` `/get_pool` `/get_block_details_by_height` `/get_blocks_details_by_heights`
`/get_blocks_details_by_hashes` `/get_transaction_details_by_hashes`
`/get_transaction_hashes_by_payment_id` `/queryblockslite` `/get_blocks_hashes_by_timestamps`
and `/json_rpc` (`getblockcount`, `on_getblockhash`,
`getlastblockheader`, `getblockheaderbyhash`, `getblockheaderbyheight`,
`get_block_headers_range`, `get_info`, `getcurrencyid`, `getblocktemplate`, `get_miner_data`,
`submitblock`, `f_blocks_list_json`, `f_block_json`, `f_transaction_json`,
`f_on_transactions_pool_json`). Every
path accepts GET and POST like the C++ server. CORS like `--enable-cors` (one origin per flag, `*`
for any; the matching origin is echoed). `/status` is xkr's own JSON for a desktop app: height,
network height, sync progress, peers per transport, download rates, pool, alternative blocks,
database size, uptime, the built-in miner (`mining`), the Hugin relay counters (`hugin`:
clients, nodes, messages and bytes in the message pool, posts, gossip, shares submitted /
accepted / rejected, `pool_ready` = a job can be handed out, `fee_address`, `fee_donation`,
`p2pool { host, connected }`; `null` with the relay off), the fork state (`fork`: active and next index, next height, readiness) and
the block-rate monitor (`block_rate`), and stripe pruning (`pruning`, "Pruned peers"; `/getinfo`
has `pruning_seed`). `/getinfo`
`upgrade_heights`/`supported_height` come from the fork table (unchanged until fork 13 is
scheduled) and `fork_readiness` is xkr's addition (`docs/forks.md`). Default bind `127.0.0.1:11898` (testnet
`11899`).

Wallet-facing behaviour (what `kryptokrona-wallet-backend-js` `Daemon.ts` calls):

* `/getrandom_outs { amounts, outs_count }` -> `{ outs: [{ amount, outs: [{ global_amount_index,
  out_key }] }], status }`: C++ `getRandomOutputs` — uniform sampling without replacement among
  outputs of the amount that are unlocked at the top and at least one unlock window deep, sorted
  by index; `"Failed"` (with the outs so far) when an amount has none or the chain is shorter
  than two unlock windows.
* `/get_pool_changes_lite { tailBlockId, knownTxsIds }` -> `{ isTailBlockActual, addedTxs:
  [{ "transactionPrefixInfo.txHash", "transactionPrefixInfo.txPrefix" }], deletedTxsIds,
  status }` with the literal dotted C++ keys (wallet-backend-js rewrites them) and prefixes in the
  C++ JSON form (`vin` `{type: "ff"|"02", value}`, `vout` target `{type: "02", data: {key}}`,
  `extra` hex).
* `/sendrawtransaction { tx_as_hex }`: full pool validation (`Chain::submit_transaction`:
  semantic, policy, key images, ring members, ring signatures), then relay to every synchronized
  peer; `{ status: "OK", transactionHash }` or `{ status: "Failed", error }` (C++ sends only
  `status`; wallet-backend-js shows `error` when present).
* Explorer and pool routes (`crates/xkr-rpc/src/details.rs`, `handlers.rs`): the C++
  `BlockDetails` / `TransactionDetails` JSON (`blockchain_explorer_data_serialization.cpp`:
  inputs `{ type: "ff"|"02", data: { input, amount | mixin, output: { transactionHash, number } } }`,
  outputs `{ output: { amount, target }, globalIndex }`, `signatures: [{ first, second }]`,
  `extra { publicKey, nonce, raw }`, no `penalty`). Additions: `extra.raw` is filled (C++ writes
  `""`), version-2 transactions carry `clsag_signatures` and tagged targets (`docs/tx-v2.md`
  §9). Only the main chain and the pool are searched (C++ also finds alternative blocks). Caps:
  100 blocks per details call, 1 000 transactions per `/get_transaction_details_by_hashes` (a
  repeated hash or height is answered at every position, like C++, and built once; at most
  16 MiB of JSON per answer), `/get_pool` 10 000 entries oldest first, `/get_pool_changes` 2 000
  blobs and the pool listings 8 MiB of JSON per answer (blobs as arrays of byte values, as C++
  serializes a `BinaryArray`; the rest follows on the next poll).
* p2pool (`mining.rs`): `get_miner_data` (C++ fields; `difficulty` hex without `0x`, `seed_hash`
  zeros, `tx_backlog` sorted by fee per byte and capped at 10 000, no transaction cloned) and
  `get_block_headers_range` (inclusive, at most 1 000, error -2 like C++).
* C++ `NodeRpcProxy` (kryptokrona-service / walletd and xkrwallet, `node_rpc_proxy.cpp`;
  `crates/xkr-rpc/src/node_proxy.rs`). Every call it makes is routed: `getlastblockheader`,
  `/getinfo`, `/feeinfo`, `/get_pool_changes_lite`, `/queryblockslite`, `/get_o_indexes`,
  `/getrandom_outs`, `/sendrawtransaction`, `/get_block_details_by_height` (scan height to
  timestamp), `/getwalletsyncdata`, `/get_global_indexes_for_range`, `/get_transactions_status`,
  the explorer routes and `/get_blocks_hashes_by_timestamps`. All are JSON (`jsonCommand`); the
  proxy sends `Host: 127.0.0.1` and needs a `Content-Length` on every answer, which every route
  sends.
  * `/queryblockslite { blockIds, timestamp }` -> `{ status, startHeight, currentHeight,
    fullOffset, items }` (`Core::queryBlocksLite`): `startHeight` is the height of the first
    `blockIds` entry on the main chain (0 for an empty list; none gives `"Failed to perform
    query"` with zeros and no items), `currentHeight` the top block's height. A `timestamp` of
    0 with `startHeight > 0` becomes that block's timestamp. `fullOffset` is the first block (in
    chain order) of the timestamp's UTC day, or of the latest earlier day that has blocks (0
    when none after day 0 has), raised to `startHeight`; a timestamp above `i64::MAX` fails
    like C++. `items` are `{ "blockShortInfo.blockId", "blockShortInfo.block",
    "blockShortInfo.txPrefixes" }` with the literal dotted keys: hash-only entries (`block`
    and `txPrefixes` empty) from `startHeight` up to `fullOffset`, at most 10 000; only when
    they reach `fullOffset`, full entries from it, at most 100: `block` is the block blob as a
    JSON array of byte values (C++ `BinaryArray`) and `txPrefixes` the
    `{ "transactionPrefixInfo.txHash", "transactionPrefixInfo.txPrefix" }` entries of its
    transactions (miner transaction excluded; prefixes in the `/get_pool_changes_lite` form;
    no signatures are needed, so a pruned node answers too). An answer stops adding full blocks after 16 MiB
    of JSON (the wallet continues from the last block it got). A `blockIds` entry that is not
    a hash is `400`.
  * `/get_blocks_hashes_by_timestamps { timestampBegin, secondsCount }` -> `{ status,
    blockHashes }`: main-chain blocks with a timestamp in `[timestampBegin, timestampBegin +
    secondsCount)`, by timestamp and then height, at most 100 000; a range whose end does not
    fit in 64 bits is `"Error: Timestamp overflow"` with no hashes, as in C++.
* Not implemented (called by none of wallet-backend-js, the C++ wallets, Hugin, p2pool or the
  C++ miner): `/getrawblocks` (wallet-backend-js only uses it when its `useRawBlocks` is set,
  which it never is, and falls back to `/getwalletsyncdata` on 404; the C++ v1.2 node has no
  such route either), `/getblocks`, `/queryblocks` and `/queryblocksdetailed` (legacy wallet
  sync formats; `/queryblockslite` is the one the C++ wallets use).

## Terminal use

Like the C++ `kryptokronad`, xkrd prints the Kryptokrona header at start and is quiet otherwise:
`--log-level` takes the C++ numbers (0/1 errors, 2 warnings, the default, 3 info, 4 debug,
5 trace) or a tracing filter (`info`, `xkrd=debug,warn`); `RUST_LOG` overrides it, and
`--log-file <path>` also appends the log to a file. Independent of the level it prints one line
per new top block (`New block 2 620 300 · diff … · 2 txs`), a progress line while syncing
(height / target, percent, blocks/s, ETA; in place on a terminal every 3 s, otherwise one line
every 30 s) and, when it first reaches the network's top, "✔ Synced · height N" (plain:
"xkrd synced · height N"). Colours only on a terminal and without `NO_COLOR`.

On a colour terminal (not `NO_COLOR`, not `TERM=dumb`) the block and sync lines give way to a
live panel at the bottom (`crate::display::render_panel`): the top block height in big
box-drawing digits (normal digits below 40 columns; the width comes from `COLUMNS`, else
`stty size`, else 80) and a status line with a spinner: "waiting for block N+1 · last block 42s
ago · diff … · n peers", for 2 s "✔ new block · n txs · diff …" after a new top block or
"↺ switched chain" after a reorganisation, and while syncing a progress bar with percent,
blocks/s and ETA. Log lines and console replies print above the panel (`term::Screen` clears the
panel line by line with cursor-up + clear-line, writes, and draws it again); the panel is redrawn
in place between a saved and restored cursor, so a command being typed below it stays.

Starting shows each part of the node as it comes up, in the order `main` brings them up
(`crate::stopping`, a start-up reporter): verification threads, storage (database size and
pruning), transaction pool, chain (height and tip), RPC server, miner (with `--mine`), legacy
P2P, Hyperswarm and Hugin relay (on by default; with its fee address), each with a spinner and its elapsed time
while it runs (opening a large database can take a while) and a ✔ with detail and time after.
On a colour terminal the whole start-up block stays at the bottom while it runs (log lines print
above it) and is printed for good under the title "Started" (or "Start stopped" when the node
is stopped meanwhile), then "Node running · started in …" and the live panel. The console and Ctrl-C / SIGTERM work
from the header on: until the chain is open `status`, `height` and `print_pl` answer "the node
is starting: <step>", and `exit` stops the start-up and closes what was opened. After an unclean stop
(crash, kill) redb checks the whole file while it opens; the Storage step says why once
("The node did not stop cleanly last time, so the database is being checked." and the file
size), shows it live (`Store::open_observed`; redb reports its progress only at its phase boundaries,
so the status is how much of the file the process has read, from `/proc/self/io` on Linux:
"checking the database · ~38 % read", without a figure elsewhere) and ends with
"checked after an unclean stop · 3 min 12 s"; a stop requested meanwhile ends the check (it runs again at
the next start). A resumed interrupted repack shows as "finishing an interrupted repack of …".

Every way the node ends shows the "Stopping the node" title and its steps: `exit`, Ctrl-C,
SIGTERM, SIGHUP, an error during start-up (only what was opened is listed: the database closed
or the chain flushed) and an error at run time; an error before anything was opened still
shows the title and the final line.

Stopping (Ctrl-C, SIGTERM, SIGHUP when the terminal window is closed, `exit`) shows each part of the node as it stops (`crate::stopping`):
legacy P2P, the peer list (`p2pstate.json`), the Hyperswarm announcements, peers, Hugin relay
and Bare workers, the RPC server, the miner, the remaining tasks, the chain thread and
the database flush, each with its time, then "Node stopped at height … · uptime …". On a colour
terminal with spinners and ✔ / ✗, otherwise one plain line per step (`stopping RPC server …
done 12 ms`), with `--log-json` one `shutdown step` event per step. The log level does not
change. The first Ctrl-C, SIGTERM, SIGHUP or `exit` starts the clean stop and locks input: the
console reads no more commands, the next signal prints "stopping cleanly — please wait, the
database is being closed" once, and later ones are ignored, so no signal cuts the stop short
(only SIGKILL can). Every stop step is bounded (Hyperswarm 10 s, RPC 10 s, miner 10 s,
remaining tasks 5 s), so the chain thread and the database flush always run, last.

In a terminal xkrd reads console commands from stdin (`crate::console`, C++
`DaemonCommandsHandler`): `help`, `status` (heights, peers, difficulty and hashrate, pool,
version and uptime, database size, pruning, mining, Hugin relay with fee address and p2pool),
`height`, `print_pl`, `set_log <0-4>`, `fee-address [<address>]` (shows or sets the fee
address, `crate::fee`; while the start-up question about it is open, an empty line keeps the
donation) and `exit` (stops the node like SIGINT/SIGTERM). `help` lists them compactly
(`name  description`, names bold, descriptions dim, at most 60 columns); `help <command>` gives
a one-line detail. `--no-console` turns the console off; it is also off when stdin is not a
terminal.

## Sidecar use

No daemonising, no Unix sockets. `--log-json` prints structured logs on stdout at info level
unless `--log-level` says otherwise, without the header, block and progress lines (progress
events once per second while syncing: height, target, blocks/s, MB/s, peers, in-flight,
buffered MB). `--rpc-bind-port`, `--p2p-bind-port 0`, `--data-dir`, `--verify-threads`, `--io-threads` and
`--mine <address> --mine-threads N` (built-in miner, `/status.mining`) let a host app control
resources. SIGINT/SIGTERM shut down cleanly with a durable
commit.

## Known gaps

* The routes listed as not implemented above.
* No push channel for new blocks (p2pool polls).
* Back-pings happen after the handshake answer (C++ pings first); the outcome is the same
  white/gray decision.
* Stripe pruning (`--prune`, `docs/storage.md` "Stripe pruning") prunes storage; how pruned
  peers are found and asked for blocks is described with the P2P layer ("Pruned peers"). Gaps
  there: lookups only cover the default stripe count, and Hyperswarm peers are not rotated for
  a missing stripe.
