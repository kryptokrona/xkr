# Mining with xkrd

Status: 2026-11-13, including fork-14 templates (major 7, version-2 coinbase). Code:
`crates/xkr-chain/src/template.rs` (templates),
`crates/xkr-rpc/src/mining.rs` (`getblocktemplate`, `submitblock`), `crates/xkrd/src/miner.rs`
(built-in miner), `crates/xkr-txpool/src/pool.rs` (`fill_block_template`).

## `getblocktemplate` / `submitblock` (C++-compatible)

Both are JSON-RPC methods on `POST /json_rpc`, exactly as `RpcServer::on_getblocktemplate` /
`on_submitblock` of kryptokronad, which the bundled C++ `miner` (`src/miner/miner_manager.cpp`),
pools and p2pool use.

```json
{"jsonrpc":"2.0","id":0,"method":"getblocktemplate","params":{"wallet_address":"SEKR...","reserve_size":8}}
-> {"result":{"blocktemplate_blob":"05...","difficulty":1,"height":1234,"reserved_offset":127,"status":"OK"}}

{"jsonrpc":"2.0","id":0,"method":"submitblock","params":["05..."]}
-> {"result":{"status":"OK"}}
```

| case | C++ error | xkrd |
|---|---|---|
| `reserve_size > 255` | -3 "To big reserved size, maximum 255" | same |
| address does not parse (`SEKR…` and `Xkr…` accepted, integrated refused) | -4 "Failed to parse wallet address" | same |
| template cannot be built | -5 "Internal error: failed to create block template" | same |
| `submitblock` params not one string | -1 "Wrong param" | same |
| blob not hex | -6 "Wrong block blob" | same |
| block not added (decode error, transaction not in the pool, orphan, invalid) | -7 "Block not accepted" | same |

`submitblock` succeeds for every C++ `BLOCK_ADDED` outcome: added to the main chain, added as an
alternative block (with or without a chain switch) and "already exists". Blocks that change the
main chain are relayed with hop 0 (`NOTIFY_NEW_LITE_BLOCK` to P2P v4+ peers).

### How a template is built (`Chain::block_template`)

Everything comes from one storage snapshot plus the pool, mirroring `Core::getBlockTemplate`:

1. `height = top + 1`, `difficulty = getDifficultyForNextBlock()`, major version of the height
   from the fork table (6 after fork 13, 7 after fork 14), previous hash = top. Minor version:
   the supported fork index of the schedule (`ForkSchedule::template_minor_version`: 13, or 14
   once fork 14 is scheduled, as on mainnet at 2 750 000 and on a testnet with
   `--fork14-height`; 14 signals forks 13 and 14) instead of the C++ 0, to signal fork
   readiness (`docs/forks.md`; consensus and the C++ upgrade manager ignore it, p2pool
   overwrites it with 16).
2. Timestamp = now, raised to the median of the last 11 timestamps (60 below height 620 000) —
   the jagerman timestamp patch of the C++ node.
3. `medianSize = max(median of the last 100 block sizes, full reward zone)`;
   `fillBlockTemplate`: zero-fee (fusion) transactions first, from the cheap end of the C++
   priority order, while their total stays within `FUSION_TX_MAX_SIZE` (30 000 bytes); then all
   transactions in priority order (`fee * other_size` compared in 128 bits, then smaller, then
   older), zero-fee ones within `medianSize`, paying ones within `min(125 % of medianSize,
   maxBlockCumulativeSize(height)) - 600`. Transactions with `extra` of 128 bytes or more (Hugin
   messages) or a mixin outside the limits of the height are skipped, as are transactions whose
   key images a selected one already uses.
4. Miner transaction (`constructMinerTx`): fresh tx key `R` in extra, then the extra nonce
   (`02 len 00…` with `reserve_size` zeros), the reward `getBlockReward(major, medianSize,
   blockSize, alreadyGeneratedCoins, fees)` decomposed into denominations and merged down to at
   most 11 outputs, each paid to `H_s(8·r·A ‖ i)·G + B`, unlock time `height +
   CRYPTONOTE_MINED_MONEY_UNLOCK_WINDOW` (20, testnet 1). The two-phase loop then pads `extra`
   with zero bytes until coinbase size + transactions size is exactly the size the reward was
   computed for (with the C++ one-byte varint correction).
   **From fork 14** (block `F + 1` on, major 7) the miner transaction is **version 2**: the same
   inputs, extra, amounts and unlock time, but every output is a tagged key (tag `0x03`, key,
   then the view tag `view_tag(8·r·A, i)`, one byte more per output, `docs/tx-v2.md` §3). The
   size loop accounts for the extra bytes. Pool transactions the next block no longer allows
   (version 1 after `F + grace`) are left out of the template. External template builders
   (p2pool, pools that build their own coinbase) must produce exactly this: a version-1
   coinbase or an untagged output after the fork is `WRONG_TRANSACTION_VERSION` /
   `OUTPUT_UNKNOWN_TYPE`, a v6 block is `WRONG_VERSION`.
5. Major version >= 2: a merge-mining parent block with `transaction_count = 1`, empty branches
   and a parent miner transaction whose extra is a zero merge-mining tag (`03 21 00` + 32 zero
   bytes). The parent major version is 0 because C++ assigns `BLOCK_MINOR_VERSION_0` to it (a
   copy-paste slip that is part of the wire format of every template).
6. `reserved_offset` = position of `R` in the blob + 32 + 3 (`slow_memmem` in C++); 0 when
   `reserve_size` is 0.
   **Quirk reproduced on purpose:** the pubkey tag precedes `R`, so `+3` lands on the second
   byte of the nonce (Monero uses `+2`). Pools and p2pool were written against the C++ value, so
   xkrd reports the same offset.

A miner must, like C++ `MinerManager::adjustMergeMiningTag`, replace the merge-mining tag by one
whose merkle root is the block's auxiliary header hash before hashing
(`xkr_chain::set_merge_mining_tag`); without it the proof-of-work check fails for every nonce.
Then it varies the 4-byte nonce, hashes the proof-of-work blob (the header-only parent block for
v2+) with the algorithm of the major version and checks `check_hash(hash, difficulty)`.

`submitblock` (`Chain::assemble_mined_block`) decodes the blob and attaches the listed
transactions from the pool (C++ `TRANSACTION_ABSENT_IN_POOL` if one is missing), then the block
goes through the chain thread like a relayed block.

## Built-in miner: `xkrd --mine <address> | --mine-p2pool [--mine-threads N]`

For the desktop app (node and miner run as one sidecar) and the testnet. Two modes:

* **solo** (`--mine <address>`, console `start_mining <address> [threads]`, the default): blocks
  paid to that address, as described below.
* **p2pool** (`--mine-p2pool` with the built-in p2pool running, console `start_mining p2pool [threads]`): the
  miner takes the built-in p2pool's current template in process (no stratum connection) and
  hands its results to the same share path as a stratum miner of the node: a result at the
  sidechain difficulty goes into the sidechain and to the peers ("Share found" line), one at
  the main-chain difficulty also goes to the node as a block ("Block found by p2pool"). Shares
  pay the node's p2pool payout address, the fee address the Hugin relay uses (`--hugin`,
  console `fee-address`); while that is still the donation default, the start says so and how
  to change it. The job target is the stratum template target (the easier of the two
  difficulties), the extra nonce a fixed value outside the stratum range in use, one share per
  template (the next job comes with the template p2pool builds on its new tip). While p2pool
  waits for the node to synchronize the miner waits too; `start_mining p2pool` refuses with
  `--no-p2pool` (on the testnet: without `--p2pool`) or when p2pool failed.

The console's `stop_mining` stops the miner (its threads are joined); `start_mining` refuses
while one runs. Threads default to half of the available cores in both.

* `--mine` takes a `SEKR…`/`Xkr…` address; `--mine-threads` defaults to half of the available
  cores (at least 1). The miner threads are plain OS threads (`xkr-mine-*`) outside the tokio
  runtime and the verification pool.
* The hashing threads and the miner loop are the `xkr-miner` crate's (the same code as the
  standalone `xkr-miner` binary below); xkrd adds a `ChainJobSource` (`crates/xkrd/src/miner.rs`).
* The controller (`xkr-miner`) builds templates directly with `Chain::block_template` (no HTTP),
  fixes the merge-mining tag, computes the proof-of-work blob once and finds the nonce offset;
  every hashing thread patches its own nonce sequence (random start, stride = thread count) into
  a private copy and hashes with CN-Turtle-Lite v2 (v5, v6 and v7: the proof of work did not
  change), CN-Lite v1 (v4) or CryptoNight v0 (v1-v3), chosen by `pow_variant_for(major)`. Each
  thread keeps one `xkr_crypto::cryptonight::CnHasher` (its scratchpad is reused, no allocation
  per hash) on the CPU's AES instructions when it has them.
  It needs no fork-14 code of its own: the template is already v7 with a version-2 coinbase.
* New template when the tip changes (checked every 500 ms) or every 30 s (new pool transactions,
  fresh timestamp). Mining pauses while the node is not synchronized (`/status` `synced`
  false or the downloader active), so it never mines on a stale tip during initial sync.
* A found block takes the `submitblock` path (`RpcCommand::SubmitBlock` -> P2P manager ->
  chain thread -> relay with hop 0), so mined blocks reach C++ peers like any other.
* At trivial difficulty (testnet: fixed difficulty 1, every hash wins) the miner waits 1 s
  between blocks, so the testnet gets at most about one block per second.
* Logs `mining mode=… hashrate=… height=… difficulty=… found=…` every 10 s; `/status` has
  `mining: { active, mode, address, threads, hashrate, total_hashes, height, difficulty,
  blocks_found, shares_found, blocks_rejected, last_block_hash, last_error }` (`mode`: `solo`
  or `p2pool`; in p2pool mode `address` is the payout address of the share being mined,
  `difficulty` the sidechain difficulty and `shares_found` counts the shares added to the
  sidechain). The console's `status` shows `Mining: solo, 4 threads, 1.23 KH/s, block …, 2
  blocks found` or `Mining: p2pool, 4 threads, …, 3 shares found, payout SEKR…`.

## Standalone miner: `xkr-miner`

`crates/xkr-miner`. It takes the C++ `miner`'s options unchanged (`src/miner/mining_config.cpp`),
so a copy named `miner` replaces the C++ binary:

```sh
xkr-miner --daemon-address 127.0.0.1:11898 --address SEKR… [--threads N] [--scan-time 1]
          [--limit N] [--first-block-timestamp T [--block-timestamp-interval S]]
xkr-miner --daemon-host 127.0.0.1 --daemon-rpc-port 11898 --address SEKR…
```

* **Solo** (works against xkrd and the C++ `kryptokronad`): `getblocktemplate` with
  `reserve_size` 0 -> merge-mining tag -> proof-of-work blob -> hash by the blob's major
  version -> `submitblock`; `getlastblockheader` every `--scan-time` seconds, a new template
  when the top block changes and after every submitted block (C++ `MinerManager`,
  `BlockchainMonitor`). `--limit N` stops after N accepted blocks (the C++ miner counts every
  HTTP 200 as a block). `--threads` defaults to all cores and must not exceed them, as in C++.
  Without `--address` it asks on the terminal, as C++ does.
* **Pool:** `--pool stratum+tcp://host:port` (or `host:port`), `--pool stratum+ssl://host:port`
  (TLS, system roots); repeat `--pool` for fail-over pools, tried in order. `--pool-user`
  (default `--address`; pools may read suffixes such as `.rigid`), `--pool-pass` (default `x`),
  `--rig-id` (sent as `rigid`). The protocol is the cryptonote / xmrig `login`, `job`, `submit`,
  `keepalived` set (`crates/xkr-stratum`); the job blob is the merge-mining parent's hashing
  blob (its first byte is the parent's major version, 0 from p2pool), so the algorithm comes
  from the job's `algo` (p2pool: `cryptonight_turtle`; xmrig names such as `cn-pico/trtl` work
  too), else from the block at the job's `height` on `--network`, else CN-Turtle-Lite v2; a job
  naming an algorithm the miner does not have is refused. In a pool `--limit` counts accepted
  shares.
* **Output:** C++-style text on stdout (hashrate every 60 s), or with `--log-json` one JSON
  object per line for a host app: `started`, `job`, `hashrate` (every 10 s: `hashrate`,
  `total_hashes`, `accepted`, `rejected`, `stale`, `errors`, `height`, `difficulty`, `job_id`,
  `threads`), `accepted` / `rejected` / `stale` / `error` per solution, `paused`, `limit`,
  `stopped`; each with `event` and `time`. Logs go to stderr (`RUST_LOG`).
  On a colour terminal (not `--log-json`, `NO_COLOR` or `TERM=dumb`) it looks like xkrd
  (`crates/xkr-term`): the "kryptokrona miner" header, start-up steps ("Starting the miner ·
  Mainnet": daemon or pool, threads with the AES kind, first job), a live panel with the
  hashrate in big digits and "mining block N · diff … · n accepted · n rejected · n threads ·
  solo|pool host" (a found block or an accepted share flashes green), and the stop steps on
  Ctrl-C, SIGTERM or SIGHUP (hashing threads, connection, final totals); log lines print above
  the panel. One signal stops cleanly; the next prints "stopping cleanly — please wait, the
  threads are being stopped" once and later ones are ignored (no forced quit).
* `--network testnet` switches the default daemon port to 11899; `--software-aes` forces the
  table AES rounds (for comparisons).
* Hashrate on a machine: `cargo run --release -p xkr-miner --example hashrate -- --threads N`
  (the miner's hashing threads on a fixed job) and `cargo run --release -p xkr-crypto --example
  cn_hashrate` (`cn_slow_hash` against `CnHasher` with software and hardware AES).

## Built-in p2pool

xkrd runs the Kryptokrona p2pool itself (crate `xkr-p2pool`, the same shares, sidechain rules
and P2P protocol as the C++ p2pool, so both share one sidechain). On by default on mainnet;
`--no-p2pool` turns it off, `--p2pool` on (the later flag wins). The testnet needs `--p2pool`
and its own sidechain (`--p2pool-config`).

* **When it runs.** The start-up step `p2pool` shows the sidechain, the stratum address and the
  payout (`sidechain default · stratum 127.0.0.1:3333 · payout nilsjr (donation)`). p2pool
  itself starts once the node is synchronized, prints `p2pool running · stratum …`, and stops
  in the node's stopping steps (stratum, P2P with the peer list written, sidechain).
* **Payout.** Every share pays the node's fee address, the same address the Hugin relay uses
  (`--hugin <address>`, console `fee-address <address>`, saved in
  `<data dir>/hugin/identity.json`). Until you set your own it is the donation address of
  nilsjr; the node says so at start-up and asks once (one question for the relay and p2pool).
  A new address applies to the next block template at once. Miners log in with any user name;
  the login only names the worker (and `+N` asks for a fixed difficulty, at least 1000).
* **Stratum.** `--p2pool-stratum <ip:port>` (default `127.0.0.1:3333`: local miners only; use
  `0.0.0.0:3333` for miners on the network), `--p2pool-stratum-ban-time <s>` (600),
  `--p2pool-no-autodiff`. xmrig, xkr-miner and the Hugin relay mine on it unchanged; with
  `--p2pool` the relay uses this server directly.
* **Main chain.** Block templates come from the node's own chain: a new template on every new
  main-chain tip (pushed by the chain, no polling), on a new sidechain tip, on a payout change
  and every 20 s for new pool transactions. The transactions are those the node's own
  `getblocktemplate` would take. A share that also meets the network difficulty is submitted
  like `submitblock` (P2P manager, chain thread, relay); the terminal then shows
  `Block found by p2pool at height N`, and `Share found · height … · diff …` (sidechain
  height and difficulty) for each share of this node's miners (shares of other p2pool miners
  only go to the log).
* **Sidechain P2P.** Outgoing TCP to the sidechain's seed peers and the peers they name
  (`--p2pool-out-peers`, 10), plus the sidechain topic on the Hyperswarm network
  (`docs/hyper-transport.md`, "The built-in p2pool"). xkrd accepts TCP peers only with
  `--p2pool-p2p <ip:port>` (37889, mini 37888, nano 37890; its port is the one announced to
  peers; `--p2pool-in-peers`, 450). `--p2pool-addpeers <host:port,…>` adds peers,
  `--p2pool-allow-same-ip` allows several connections per address. The peer list is kept in
  `<data dir>/p2pool/p2pool_peers.txt`. `--no-p2pool-tcp` uses Hyperswarm only (no seeds, dialing
  or listener); TCP is on by default for now so that every node also reaches the C++ p2pool
  nodes (`--p2pool-tcp` turns it on explicitly).
* **Sidechains.** The default sidechain (the live Kryptokrona one), `--p2pool-mini`,
  `--p2pool-nano`, or a custom one with `--p2pool-config <file>` (JSON with `name`, `password`,
  `block_time`, `min_diff`, `pplns_window`, `uncle_penalty`; the testnet suite uses
  `p2pool/sidechain-testnet.json`). `--p2pool-uncapped-pplns` (testnet only, or
  `XKR_TESTNET_UNCAPPED_PPLNS=1`) lifts the PPLNS weight cap; every node of that sidechain must
  set it alike.
* **Status.** Console `status` (`P2pool node`, `Pool peers`, `Pool miners`, `Payout`) and
  `/status` `p2pool` (`state`, `sidechain`, `consensus_id`, `height`, `difficulty`,
  `hashrate`, `miners`, `shares`, `synced`, `peers_out` / `peers_in` / `peers_hyper`,
  `stratum_miners`, `stratum_hashrate`, `shares_found`, `blocks_found`, `payout_donation`;
  `payout_address` and `stratum` only for local clients). With `--log-level info` the log has
  the `p2pool: …` lines (`consensus id`, `new tip`, `share added`, `share found`,
  `block found`, `peer … rejected`, `synchronized`).

## External p2pool

Patches: `p2pool/README.md`; testnet recipe: `docs/testnet-suite.md` (32-p2pool). p2pool calls
`get_info`, `get_miner_data`,
`getblockheaderbyheight`, `get_block_headers_range` and `submitblock` over JSON-RPC (it polls
`get_miner_data` every 2 s; it does not use `getblocktemplate`, so the `reserved_offset` quirk
does not affect it), all served by xkrd now. `get_miner_data` has the C++ v1.2 fields
(`major_version`, `height`, `prev_id`, `seed_hash` = 64 zeros, `difficulty` as hex without
`0x`, `median_weight`, `already_generated_coins`, `median_timestamp`, `unlock_window`, `tx_backlog: [{ id, weight, fee }]`), read from one storage
snapshot; the backlog is exactly what `getblocktemplate` would select for the next block
(template rules: no Hugin messages, mixin and version rules of the next block; priority order,
deterministic, so the answer is byte-identical while the pool is unchanged) and capped at
10 000. `get_block_headers_range` serves at most 1 000 headers like C++.

Forks 13 and 14 need the p2pool patches in `p2pool/patches/`: upstream p2pool's sidechain fork
table stops at major 5, and p2pool builds the coinbase itself, which must be version 2 with
tagged outputs from `F14 + 1`. p2pool never parses transaction blobs, so version-2 pool
transactions need nothing from it. No push channel is needed (it polls).

### The Hugin relay as a p2pool miner

The Hugin relay (on by default, `docs/hugin-relay.md` §11) is a stratum client of p2pool like
any miner: it logs in with `<fee address>+1000` (p2pool's least fixed difficulty, `+<n>` when
the app difficulty `n` is higher; p2pool reads the part before `+` as the user name), hands
p2pool's jobs to Hugin apps and submits the shares that pay for accepted messages and also meet
p2pool's target. It uses the local p2pool
at `127.0.0.1:3333` first, then the public p2pool nodes built into xkrd (`--hugin-p2pool`
replaces the list), and moves back to the local node when it is reachable again.

p2pool builds every block with the wallet it was started with (`--wallet`) and pays its
sidechain shares to that wallet; the stratum login only names the user. The Hugin shares
therefore pay the wallet of the p2pool node that receives them: to have them paid to your fee
address, run the built-in p2pool (on by default, which pays the fee address and which the relay
then uses directly) or your local C++ p2pool with `--wallet <fee address>`. A submitted share is a sidechain
share only when it meets the sidechain difficulty (rarely, at difficulty 1000) and a main-chain
block when it meets the network difficulty (every share on the testnet).

#### App difficulty and p2pool difficulty

Apps mine at the relay's app difficulty (`--hugin-share-difficulty`, default 200: about one
second per message on a multi-core phone), lower than p2pool's least fixed difficulty of 1000.
The relay hands each p2pool job to the apps with the target replaced by the app target
(`floor(0xFFFFFFFF / 200)` = `14ae4701`, the same 4-byte encoding as hugin-pow
`difficultyToTarget`), keeps p2pool's `job_id` and blob, checks every share against the app
target, and submits a share to p2pool only when its hash also meets p2pool's target of that job.
At 200 against 1000 that is about one share in five; every share still pays for its message.
`/status` `hugin` counts `shares_accepted`, `shares_submitted` and `shares_rejected_by_pool`.
With `--hugin-share-difficulty 1000` (or more; the login then asks for the same `+n`) every app
share is a p2pool share, as the testnet scenario 40 runs it.

A p2pool job's blob is the merge-mining parent header: `algo`
`cryptonight_turtle`, the 4-byte target of the asked difficulty, a `job_id` that counts per
connection, and a zero parent `prev_id`. Because job ids start again with every connection, the
relay sends a share only for the job id and blob the current connection issued.

## End-to-end testnet test

Automated by the regression suite (`docs/testnet-suite.md`): `10-sync-from-cpp` (step 1),
`11-serve-to-cpp` (step 2), `12-mining` (steps 3 and the built-in miner), `13-transactions`
(step 4, mixin 0 and 1), `31-fork14` (step 5), on a private chain mined from genesis instead of
the compose testnet. The manual steps:

Prerequisites: `testnet/up.sh --build` (three C++ nodes and the C++ miner on
`172.31.98.0/24`, RPC of node1 on `127.0.0.1:11899`, P2P on `127.0.0.1:11898/12898/13898`),
release builds of `xkrd` and `xkr-testwallet`. Ports below avoid the compose ones. Containers
that must reach xkrd on the host use `--network host` (no firewall between bridge and host).

1. **xkrd on the testnet, listening:**

   ```sh
   target/release/xkrd --network testnet --data-dir /tmp/xkrd-tn \
       --p2p-bind-ip 0.0.0.0 --p2p-bind-port 14898 --rpc-bind-port 14899 \
       --add-peer 127.0.0.1:11898 --add-peer 127.0.0.1:12898 --add-peer 127.0.0.1:13898
   curl -s 127.0.0.1:14899/status | jq '.height, .peers, .synced'
   ```

2. **A fresh C++ node syncs from xkrd only:**

   ```sh
   docker run -d --rm --name xkr-cpp-fresh --network host xkr/kryptokrona-testnet:master \
       /usr/src/kryptokrona/kryptokronad --data-dir=/tmp/cpp-fresh --p2p-bind-port=15898 \
       --rpc-bind-ip=127.0.0.1 --rpc-bind-port=15899 --add-exclusive-node=127.0.0.1:14898 \
       --allow-local-ip --no-console --log-level=2
   curl -s 127.0.0.1:15899/getinfo | jq '.height'     # reaches the xkrd height
   curl -s -d '{"jsonrpc":"2.0","method":"getlastblockheader"}' 127.0.0.1:15899/json_rpc | jq .result.block_header.hash
   ```

   xkrd logs `peer connected … inbound=true`; the C++ node requests chain entries and objects
   from it. The fresh node's top hash must equal xkrd's and node1's.

3. **The C++ miner mines against xkrd's RPC** (stop the compose miner first so xkrd's blocks are
   the only new ones: `docker compose -f testnet/docker-compose.yml stop miner`):

   ```sh
   docker run --rm --network host xkr/kryptokrona-testnet:master /usr/src/kryptokrona/miner \
       --daemon-address=127.0.0.1:14899 --address=<SEKR address> --threads=1
   ```

   xkrd logs `submitted block accepted` and `new block … mined=true`; node1..3 and the fresh node
   follow (relay with hop 0).

4. **The built-in miner and a transaction through xkrd:**

   ```sh
   W=target/release/xkr-testwallet
   A=$($W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 address)
   B=$($W --wallet-dir /tmp/tw-b --daemon 127.0.0.1:14899 address)
   # restart xkrd from step 1 with:  --mine "$A" --mine-threads 2
   $W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 balance     # unlocked rewards (unlock window 1)
   $W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 send "$B" 100 --fee 0.01
   #   prints "transaction <hash>" and the daemon answer {"status":"OK",...}
   curl -s -d '{"transactionHashes":["<hash>"]}' 127.0.0.1:11899/get_transactions_status
   #   C++ node1: first transactionsInPool (relayed by xkrd), then transactionsInBlock
   $W --wallet-dir /tmp/tw-b --daemon 127.0.0.1:11899 balance      # scan via the C++ node: 100 XKR
   ```

   Below height 440 000 the minimum mixin is 0, so the default transfer has ring size 1;
   `--mixin 1` exercises `/getrandom_outs` (needs a second output of the same amount, which
   coinbase denominations provide after a few blocks). To see the C++ node (not xkrd) include it,
   stop xkrd's miner (`--mine` off) and let the compose miner run.

5. **Fork 14 on the testnet** (`testnet/fork14-scenario.sh`, `docs/forks.md` §5): with
   `--fork13-height F13 --fork14-height F14 [--fork14-grace N]` the built-in miner and
   `getblocktemplate` produce major 7 and a version-2 coinbase from `F14 + 1` (the `f_*`
   methods need a node started with `--explorer`):

   ```sh
   curl -s -d '{"jsonrpc":"2.0","id":0,"method":"f_block_json","params":{"hash":"<F14+1>"}}' \
       127.0.0.1:14899/json_rpc | jq '.result.block.major_version, .result.block.transactions[0].hash'
   curl -s -d '{"jsonrpc":"2.0","id":0,"method":"f_transaction_json","params":{"hash":"<coinbase hash>"}}' \
       127.0.0.1:14899/json_rpc | jq '.result.tx.version, .result.tx.vout[0].target'
   #   7, 2, {"type":"03","data":{"key":"…","view_tag":"5e"}}
   ```
