# The testnet regression suite (the standard before a real fork)

Status: written 2026-11-13.

## 1. What the standard is

`testnet/suite.sh <work dir>` is the one command that must be green before a fork height is
announced, before a release, and after any change to consensus, P2P, RPC, storage or the
Hyperswarm/Hugin code. It runs a fixed, ordered list of scenarios on private testnets that it
builds itself from genesis (testnet difficulty 1, nothing from the public network, no
dependency on any long-running container such as the old `xkr-tn2`). Every scenario:

* runs real binaries: `target/release/xkrd`, `target/release/xkr-testwallet`, the C++ daemon and
  miner from the images `xkr-cpp-testnet:master` (C++ v1.2.2, fork index 12, the **old node**)
  and `xkr-cpp-testnet:fork13` (`testnet/patches/0001-fork13-testnet.patch`, fork index 13);
* uses the C++ node as the reference (C++ is the judge up to fork 13, xkrd after it);
* prints one line per check in a stable format and writes machine-readable results;
* never builds anything: a missing binary or image is reported with the exact build command and
  the scenario exits 2;
* runs every process under `nice -n 19`, keeps all state under the work dir and cleans up after
  itself (idempotently; `--keep` leaves the nodes running).

What a green run proves, in one sentence: xkrd syncs from and serves to unmodified C++ nodes
with bit-identical headers, produces blocks and transactions that C++ accepts, survives
restarts and crashes with a consistent database, carries Levin over Hyperswarm and Hugin
traffic, and performs fork 13 and fork 14 exactly as specified, including the behaviour of the
nodes that do not upgrade.

What it does not cover: the mainnet regression (full mainnet sync compared header by header against a C++ node, every ring signature verified), pools against v6/v7
templates (other than p2pool, which the optional `32-p2pool` and `34-p2pool-mixed` cover once
its image is built),
the wallet libraries (`kryptokrona-wallet-backend-js`, Hugin apps) against tx v2, and
load. Those are items of the pre-fork checklist (§7).

## 2. Running it

```sh
testnet/suite.sh /tmp/xkr-suite                 # everything, in order (about 77 min; 60-64 alone about 38)
testnet/suite.sh /tmp/xkr-suite 13              # one scenario (by number)
testnet/suite.sh /tmp/xkr-suite 13-transactions # by name
testnet/suite.sh /tmp/xkr-suite 10-14 50        # a number range and another one
testnet/suite.sh /tmp/xkr-suite 3               # a name prefix: 30-fork13 and 31-fork14
testnet/suite.sh --list                         # scenarios, expected durations, limits
testnet/suite.sh /tmp/xkr-suite --keep 31       # leave the nodes running afterwards
testnet/suite.sh /tmp/xkr-suite 90              # clean up (after --keep or a direct run)
```

On a shared laptop run it under a memory cap like any heavy test, for example
`systemd-run --user --scope -p MemoryMax=5G testnet/suite.sh /tmp/xkr-suite` (never a CPU quota,
see the README; the C++ containers have their own 1 GiB limits).

Options: `--keep` (nodes stay up, no final cleanup), `--fail-fast` (stop at the first scenario
that does not pass), `--reuse` (do not clean up an earlier run in the work dir first, so the seed
chain of 10 is reused). Without `--reuse` the suite starts by stopping whatever an earlier run
in the same work dir left behind, and it always ends with a cleanup unless `--keep` is given.

A scenario can also run on its own: `testnet/scenarios/13-transactions.sh /tmp/xkr-suite`. It
then sets up what it needs (the seed chain, xkrd S) but leaves the shared seed container
running; `testnet/suite.sh /tmp/xkr-suite 90` removes it. The old entry points still work as
thin wrappers: `testnet/fork13-scenario.sh`, `fork14-scenario.sh`, `hyper-two-nodes.sh`,
`hugin-relay-test.sh` (same environment variables; their state moves to
`<work dir>/<scenario>/`).

### Prerequisites (built once, never by the suite)

```sh
scripts/build-bare-kit.sh                         # once: libbare-kit, which xkrd links
nice -n 19 cargo build --release -j 3 -p xkrd     # xkrd + xkr-testwallet
nice -n 19 cargo build --release -j 3 -p xkr-miner  # 65 (standalone miner)
nice -n 19 cargo build --release -j 3 -p xkr-wallet-api -p xkr-walletd -p xkr-wallet-cli  # 60-64
cd testnet
XKR_CPP_IMAGE=xkr-cpp-testnet:master XKR_CPP_PATCHES=none ./build-cpp.sh      # old node
XKR_CPP_IMAGE=xkr-cpp-testnet:fork13 XKR_CPP_PATCHES=0001-fork13-testnet.patch ./build-cpp.sh
XKR_CPP_IMAGE=xkr-cpp-wallets:master XKR_CPP_PATCHES=none ./build-cpp.sh      # C++ wallet tools, 60-64
cd ../bridge && sfw npm ci                                                    # 20, 40
cd .. && p2pool/build.sh            # optional: xkr-p2pool:testnet for 32 and 34 (long, alone)
```

plus `node` >= 22, `curl`, `docker` (daemon reachable), GNU coreutils (`timeout`, `nice`) and,
for 40, a `node_modules` with `bare-rpc`, `keypear`, `hugin-utils`, `kryptokrona-utils`
(default `../hugin-desktop/node_modules` next to this repo) plus `bridge/node_modules`: the app simulator
uses the bridge's own `mid` layer, not `hyperswarm-hugin`/`hyperdht-hugin` (clients using the
earlier Hugin derivation cannot reach the relay, `docs/hugin-relay.md` §2). The image
builds are heavy (C++ and RocksDB, `make -j3`): one at a time. `00-preflight` checks all of it
and also prints a NOTE when a `crates/**/*.rs` file is newer than `target/release/xkrd`.

20 and 40 run xkrd's Hyperswarm transport, Bare workers inside xkrd (xkrd built after
`scripts/build-bare-kit.sh`; `docs/hyper-transport.md`, "Building"). Node.js is needed for the
local DHT and the simulators, not by xkrd.
On NixOS, `lib.sh` puts gcc's lib directory on `LD_LIBRARY_PATH` of every started xkrd, since
the `.bare` addons link `libstdc++.so.6`.

Resources: at most three C++ containers (2 CPUs, 1 GiB each, data on a 512 MiB tmpfs) and two
or three xkrd processes (`--db-cache-mb 64`) at a time; `00-preflight` wants 4 GiB of available
memory and 2 GiB of disk (`MIN_MEM_MB`, `MIN_DISK_MB`).

### Results

Each check prints one line:

```
PASS    13-transactions/tx-b: mined via xkrd S at height 118; S and the C++ seed agree on block 9c1f…
FAIL    31-fork14/4b: v1 send after F14 + GRACE was accepted: …
SKIP    50-storage/3: db convert (set XKR_V1_DB to a schema v1 chain.redb to run it)
MISSING 30-fork13/prereq: docker image xkr-cpp-testnet:fork13 missing; get it with: XKR_CPP_IMAGE=…
```

i.e. `^(PASS|FAIL|SKIP|MISSING) +<scenario>/<check>: <text>$`. Each scenario ends with
`=== <scenario>: PASS|FAIL|MISSING (n passed, n failed, n skipped, n s; logs in …)`. Exit
status of a scenario: 0 all passed (SKIPs allowed), 1 a check failed, 2 a prerequisite is
missing; a scenario killed by its time limit counts as FAIL (`TIMEOUT` in the table).

In the work dir:

| file | content |
|---|---|
| `results.tsv` | `scenario status exit seconds expected passed failed skipped log`, one row per scenario; status `PASS`, `FAIL`, `MISSING`, `TIMEOUT`, `SKIP` (an optional scenario without its prerequisite) |
| `results.json` | `{suite, work_dir, started, finished, xkrd, xkrd_path, status, scenarios: [{name, status, exit, seconds, expected_seconds, passed, failed, skipped, log, checks: [{id, status, time, message}]}]}` |
| `checks.tsv` | every result line of the run: `scenario check status unix-time message` |
| `logs/<scenario>.log` | the scenario's full output |
| `<scenario>/` | its state: xkrd data dirs and logs, C++ container logs (`<container>.log`), wallet outputs (`send-*.out`, `balance-*.out`), `db` tool outputs |
| `chain/` | the shared seed chain: `seed.id`, xkrd S's data dir `chain/xkrd` |
| `wallets/w1`, `wallets/w2` | the suite wallets (W1 receives the seed chain's coinbase) |
| `pids/` | pid files of running processes (what 90-cleanup stops) |

The suite exits 0 only if every selected scenario is `PASS`, or `SKIP` for an optional scenario
(`optional=1` in its `# suite:` header: `32-p2pool`, `34-p2pool-mixed`) whose prerequisite is
missing: its `MISSING` lines become `SKIP` lines ("optional scenario skipped: ...") in
`checks.tsv`. Run directly, such a scenario still exits 2.

## 3. Ports

Everything binds `127.0.0.1`; C++ containers run with `--network host` (the host firewall
blocks docker bridge -> host) and carry the label `xkr-suite=1`. The table is also the top of
`testnet/lib.sh` (the 22xxx ports of 32 and 34 are at the top of `testnet/lib-p2pool.sh`).
The p2pool and xmrig containers also carry `xkr-suite-scenario=<scenario>`, so the ones of one
scenario can be listed or removed on their own (`docker ps -aq --filter
label=xkr-suite-scenario=34-p2pool-mixed`).

| port | what | scenarios |
|---|---|---|
| 16898 / 16899 | C++ seed node (old image), container `xkr-suite-seed`, P2P / RPC | 10-14, 20, 50 |
| 17898 / 17899 | xkrd **S** on the seed chain, P2P / RPC, data `<wd>/chain/xkrd` | 10-14, 50, 65 |
| 18898 / 18899 | fresh C++ node (old image), `xkr-suite-fresh`, P2P / RPC | 11 |
| 19898 / 19899 | old C++ node (old image), `xkr-suite-f13-old`, P2P / RPC | 30 |
| 14898 / 14899 | xkrd A, P2P / RPC | 30, 31 |
| 14889 | xkrd B, RPC (no P2P listener) | 31 |
| 15898 / 15899 | C++ fork-13 image (`xkr-suite-f13-patched` in 30, `xkr-suite-f14-cpp` in 31), P2P / RPC | 30, 31 |
| 21897 / 21898 | xkrd A (Levin + Hyperswarm), P2P / RPC | 20 |
| 21888 | xkrd B (Hyperswarm only), RPC | 20 |
| 22898 / 22899 | xkrd A (phase-0 miner, then p2pool's daemon), P2P / RPC | 32, 65 |
| 22889 | xkrd B (follower), RPC (no P2P listener) | 32 |
| 22890 / 22333 | p2pool A (`xkr-suite-p2pool-a`), sidechain P2P / stratum (65 without the image: the stratum of xkrd A's built-in p2pool) | 32, 65 |
| 22891 / 22334 | p2pool B (`xkr-suite-p2pool-b`), sidechain P2P / stratum | 32 |
| 22892 / 22335 | optional unpatched p2pool C (`xkr-suite-p2pool-c`) | 32 |
| 22868 / 22869 | optional C++ fork-13 node (`xkr-suite-p2pool-cpp`), P2P / RPC | 32 |
| 22798 / 22799 | xkrd X (phase-0 miner, then C's daemon and R1's Levin peer), P2P / RPC | 34 |
| 22789 | xkrd R1 (built-in p2pool, Levin to X, Hyperswarm), RPC (no P2P listener) | 34 |
| 22779 | xkrd R2 (built-in p2pool, Hyperswarm only), RPC (no P2P listener) | 34 |
| 22790 / 22343 | C++ p2pool C (`xkr-suite-p2pool-mixed-c`), sidechain P2P / stratum | 34 |
| 22791 / 22344 | R1's p2pool, sidechain TCP listener / stratum | 34 |
| 22345 | R2's p2pool stratum (no sidechain TCP listener) | 34 |
| 23898 / 23899 | xkrd A (full, the long chain), P2P / RPC | 33 |
| 23888 / 23889 | xkrd P (stripe-pruned copy of A), P2P / RPC | 33 |
| 23879 | xkrd F (fresh), RPC (no P2P listener) | 33 |
| 23868 / 23869 | fresh C++ node (old image), `xkr-suite-pruned-cpp`, P2P / RPC | 33 |
| 24898 / 24899 | xkrd **X**, the wallet fixture chain (own genesis), P2P / RPC | fixtures |
| 24888 / 24889 | C++ node following X (`xkr-suite-fx-cpp`), P2P / RPC | fixtures |
| 24878 / 24879 | the wallet image's own node, follow check (`xkr-suite-fx-ghcr`), P2P / RPC | fixtures |
| 24070 / 24071 | C++ `wallet-api` (`xkr-suite-fx-api`, `-api-cors` with `--enable-cors`) | fixtures |
| 24080 / 24081 | C++ `kryptokrona-service` (`xkr-suite-fx-walletd`, `-walletd-alt` legacy security + CORS) | fixtures |
| 24090 | stub daemon (python3) for the C++ miner benchmark | fixtures |
| 26098 / 26099 | xkrd X of 60, P2P / RPC | 60 |
| 26070 / 26071 | C++ `wallet-api` (`xkr-wl-60-api`) / `xkr-wallet-api` | 60 |
| 26198 / 26199 | xkrd X of 61, P2P / RPC | 61 |
| 26180 / 26181 | C++ `kryptokrona-service` (`xkr-wl-61-walletd`) / `xkr-walletd` | 61 |
| 26298 / 26299 | xkrd X of 62, P2P / RPC | 62 |
| 26270 / 26271 | C++ `wallet-api` (`xkr-wl-62-api`) / `xkr-wallet-api` | 62 |
| 26280 / 26281 | C++ `kryptokrona-service` (`xkr-wl-62-walletd`) / `xkr-walletd` | 62 |
| 26398 / 26399 | xkrd X of 63 (fork 13 and fork 14 scheduled), P2P / RPC | 63 |
| 26370 / 26371 | C++ `wallet-api` (`xkr-wl-63-api`) / `xkr-wallet-api` | 63 |
| 26498 / 26499 | xkrd X of 64, P2P / RPC | 64 |
| ephemeral | local HyperDHT (UDP), fake stratum pool, Hugin relays (P2P and RPC port 0) | 20, 40 |
| 127.0.0.1:9 | "nobody": `--add-exclusive-node` / `--seed-node` placeholder so a node dials no one | all |

"fixtures" are the wallet fixture captures in `testnet/fixtures/` (not suite scenarios; one
command: `testnet/fixtures/capture-all.sh <work dir>`). They use the same lib, label and cleanup
(`testnet/suite.sh <wd> 90` also removes their containers) and their own chain X, mined only
with `xkr-testwallet mine --to`, so heights and amounts repeat run to run. They run the C++
wallet tools from `ghcr.io/kryptokrona/kryptokrona-testnet:latest` (`FX_IMAGE`; one-off
containers `xkr-suite-fx-once` have no port) against xkrd X directly; details and the output
paths are in the header of `testnet/fixtures/lib-wallet.sh` and of each `capture-*.sh`.

The wallet scenarios 60-64 each start their own xkrd X (mined only with `xkr-testwallet mine`,
like the fixture chain) and run the C++ wallet tools from `xkr-cpp-wallets:master` (`WL_IMAGE`)
next to the Rust ones. Their containers (`xkr-wl-NN-*`, one-off `xkr-wl-<scenario>-once`) carry
the label `xkr-wallets` (`WALLET_LABEL`), not `xkr-suite`: a suite or fixture run cleaning up
its own containers leaves a wallet scenario that runs next to it alone. Each scenario removes
its containers by name when it ends; after a hard kill,
`docker rm -f $(docker ps -aq --filter label=xkr-wallets)` removes what is left.

The C++ miner (`xkr-suite-miner`, `xkr-suite-f13-miner`) has no port. The compose testnet
(`testnet/docker-compose.yml`, 11898-13899) is not used by the suite and never peered with: the
C++ nodes always get `--add-exclusive-node`, xkrd gets `--add-exclusive-node` or a dead
`--seed-node` (both compiled-in testnet seeds are `127.0.0.1:11898`).

## 4. The scenarios

Expected durations are for an idle 8-core laptop; each scenario has a hard limit
(`# suite: expected=… timeout=…` in its header).

| scenario | expected | limit | needs |
|---|---|---|---|
| `00-preflight` | 10 s | 3 min | - |
| `10-sync-from-cpp` | 1.5 min | 15 min | xkrd, testwallet, old image |
| `11-serve-to-cpp` | 1 min | 15 min | same |
| `12-mining` | 1.5 min | 15 min | same |
| `13-transactions` | 2 min | 20 min | same |
| `14-restart-persistence` | 1 min | 15 min | same |
| `20-hyper-two-nodes` | 2 min | 25 min | xkrd (with libbare-kit), node, bridge, old image (seed) |
| `30-fork13` | 5.5 min | 30 min | xkrd, testwallet, old + fork13 images |
| `31-fork14` | 8 min | 30 min | xkrd, testwallet, fork13 image |
| `32-p2pool` (optional) | 10 min | 30 min | xkrd, testwallet, image `xkr-p2pool:testnet` (SKIP without it); fork13 image with `OPTIONAL_CPP=1` |
| `33-pruned-peers` | 2.5 min | 20 min | xkrd, testwallet, old image (seed + one fresh C++ node) |
| `34-p2pool-mixed` (optional) | 15 min | 45 min | xkrd (with libbare-kit), testwallet, xkr-miner, node, bridge, image `xkr-p2pool:testnet` (SKIP without it) |
| `40-hugin-relay` | 3 min | 25 min | xkrd (with libbare-kit), node, bridge, Hugin modules |
| `50-storage` | 0.5 min | 15 min | xkrd (+ the suite chain) |
| `60-wallet-api` | 7 min | 30 min | xkrd, testwallet, xkr-wallet-api, image `xkr-cpp-wallets:master` |
| `61-walletd` | 7 min | 30 min | xkrd, testwallet, xkr-walletd, same image |
| `62-wallet-migration` | 13 min | 45 min | xkrd, testwallet, xkr-wallet-api, xkr-walletd, xkrwallet, same image (all four C++ wallet tools) |
| `63-wallet-fork14` | 6 min | 30 min | xkrd, testwallet, xkr-wallet-api, same image |
| `64-wallet-cli` | 5 min | 25 min | xkrd, testwallet, xkrwallet; same image for the C++ files (MISSING without it, the Rust part still runs) |
| `65-miner` | 1.5 min | 15 min | xkrd, testwallet, xkr-miner, old image; check 4 uses image `xkr-p2pool:testnet` when present, else xkrd's built-in p2pool |
| `90-cleanup` | 10 s | 3 min | - |

Most of 30 and 31 is the 150 s older-fork stall rule (`docs/node-architecture.md`, "Peers at a
fork") and one block per second up to the fork heights.

### The shared seed chain (10-14, 20, 50)

`ensure_seed` (lib) starts the C++ seed node from the old image with no peers of its own and
mines `SEED_BLOCKS` (100) blocks from genesis with the bundled C++ `miner` to wallet W1, then
stops the miner. `<wd>/chain/seed.id` (hash of block 1 + W1) ties xkrd S's data dir to that
seed: a new seed wipes `<wd>/chain`. Scenarios 11-14 and 50 set the chain up themselves when
run alone (`ensure_suite_chain`: seed, S at the seed's top), so they do not depend on 10 having
run, only on its outcome being reproducible. S always runs with `--add-exclusive-node
127.0.0.1:16898 --no-hyper`.

### 00-preflight

* `cmd`, `docker`: commands present, docker daemon reachable.
* `source`: git commit and number of changed files, or "not a git checkout".
* `xkrd`, `testwallet`: binaries present; version, sha256 prefix and build time printed.
* `bare-kit`: xkrd starts, i.e. finds libbare-kit; MISSING otherwise, with the build steps.
* `node` >= 22, `bridge` (`hyperswarm`, `hyperdht` in `bridge/node_modules`), `hugin` (the six
  Hugin app packages).
* `image` x2: `xkr-cpp-testnet:master` and `:fork13` exist and contain `kryptokronad` and
  `miner`; id and creation time printed.
* `ports`: every port of §3 free. `disk` >= `MIN_DISK_MB`, `memory` >= `MIN_MEM_MB`.

### 10-sync-from-cpp: xkrd syncs a chain mined by an old C++ node

1. The seed mined its chain (or is reused); its top block is major version 5 (testnet:
   `UPGRADE_HEIGHT_V2..V5 = 0`).
2. A fresh xkrd S (empty data dir) reaches the seed's top **height and hash**.
3. Every header `0..top` is identical on both: hash, `major_version`, `minor_version`,
   `timestamp`, `difficulty`, `reward`, `num_txes`, `block_size` (`CMP_FIELDS`).
4. S `/status`: `network` testnet, `synced` true, `peers.legacy_outgoing` >= 1.
5. `/getinfo` `upgrade_heights` and `supported_height` equal on C++ and xkrd.
6. 3 blocks mined on the C++ seed afterwards reach S live (relayed lite blocks), same top.

### 11-serve-to-cpp: an old C++ node syncs from xkrd only

1. A fresh C++ node (old image) starts with `--add-exclusive-node` = S.
2. It reaches S's top height and hash (it cannot have got the chain from anyone else).
3. Every header `0..top` identical on the fresh node and S (`CMP_FIELDS`).
4. S logged `peer connected … inbound=true` (the C++ node's connection).
5. The fresh node's `/getinfo` shows an outgoing connection.

### 12-mining: getblocktemplate/submitblock and the built-in miner

1. `getblocktemplate` (W1, `reserve_size` 8) on S and on the C++ seed agree on `height`,
   `difficulty` and `reserved_offset` (the C++ +3 quirk, `docs/mining.md`).
2. The bundled C++ `miner` against **S's** RPC mines `CPP_MINE_BLOCKS` (10) blocks: S's height
   rises by 10, S logged >= 10 `submitted block accepted` and 0 `submitted block refused`, the
   miner exits 0, and the C++ seed has the same top with identical headers for those heights
   (it accepted blocks built from xkrd templates).
3. S restarted with `--mine W1 --mine-threads 1` mines `BUILTIN_BLOCKS` (10) blocks:
   `/status.mining` `active`, `blocks_found` >= 10, `blocks_rejected` 0, >= 10 `new block …
   mined=true` lines; after a restart without the miner the seed has the same top and identical
   headers.
4. W1's balance scanned **through the C++ node** lists coinbase outputs at the first height mined
   via S's template and at the last built-in block (the templates pay the right keys).

### 13-transactions: version-1 transfers through xkrd

For `tx-a` (1.5 XKR, `--tx-version 1 --mixin 0`, mined by the C++ miner against the **C++
seed**) and `tx-b` (2.5 XKR, `--mixin 1` with a decoy from `/getrandom_outs`, mined by the C++
miner against **S**, i.e. from xkrd's pool and template):

* S accepts it (`/sendrawtransaction` status OK) and the wallet built version 1 with the mixin
  asked for;
* it appears in the **C++ seed's pool** (xkrd relayed it and C++ validation accepted it, ring
  signatures included);
* it is mined, and S and the C++ seed have it in a block with the **same block hash**;
* `f_transaction_json` on S: version 1, every input has `mixin + 1` ring members.

3. W2's unlocked balance rose by exactly 4.00000 XKR, scanned through S **and** through the C++
   seed (`/getwalletsyncdata` on both).

### 14-restart-persistence: clean shutdown, restart, crash

1. SIGTERM: S exits with status 0 without needing SIGKILL (30 s), its last line `xkrd stopped
   height=H` has the height it served, `p2pstate.json` was written.
2. Restarted with no reachable peer, S serves the same top height and hash at once, and an
   interior block equals the seed's.
3. `xkrd db verify --network testnet --data-dir <S>` reports `0 problems`.
4. SIGKILL while running (after a durable-commit interval): S opens its database again,
   `db verify` reports `0 problems`, and S catches up with the seed (same top).

### 20-hyper-two-nodes: Levin over Hyperswarm

A (`--add-peer` the seed, Hyperswarm on) and B (Hyperswarm only: `--p2p-bind-port 0
--out-peers 0`, dead `--seed-node`) on a local HyperDHT (`bridge/test/testnet-bootstrap.js`;
`PUBLIC_DHT=1` uses the public one).

1. The local DHT starts.
2. A reaches the C++ node's top (skipped with an external `XKR_CPP_PEER` and no `XKR_CPP_RPC`).
3. B reaches A's top height **and hash** within `TIMEOUT` (900 s).
4. B's `/status.peers`: `legacy_outgoing` 0, `legacy_incoming` 0, `hyper` >= 1, so the chain
   can only have come over Hyperswarm.

### 30-fork13: fork 13 with an old, a patched and the xkr node

Own old node (old image, mines `OLD_BLOCKS` = 30 blocks, then no miner) <- xkrd A
(`--fork13-height F`) <- patched node (`XKR_FORK13_HEIGHT=F`), `F = H + 60` (`docs/forks.md`
§4). An external old node is still possible (`OLD_NODE_PEER`, `OLD_NODE_RPC`,
`OLD_NODE_CONTAINER`, e.g. the old `xkr-tn2`).

1. `F = H + FORK_OFFSET`; the old node's top block is major 5.
2. The patched node reports `supported_height = F` (the run-time height of the patch).
3. Phase A (nothing mines): old node, xkrd and patched node agree on height and hash `< F`.
4. Phase B (xkrd `--mine`, 1 block/s): xkrd and the patched node pass `F + 20`; on both block
   `F` is v5 and `F + 1` v6; they agree on the common top (v6); the old node sits at `F` (or
   `F - OLD_SLACK`) for 15 s and its log has `failed to parse and validate block` or
   `Deserialization of Block Template failed`.
5. The old node's top block is xkrd's block at that height (it forked off nothing); xkrd logs
   `peer is on an older fork` for the old node within `OLDER_FORK_WAIT` (300 s);
   `/status.fork.active_index` = 13.

### 31-fork14: transaction version 2 and block version 7

xkrd A (miner) and B (follower) with `--fork13-height 60 --fork14-height 140 --fork14-grace 20`,
the fork-13 C++ node following A (`docs/forks.md` §5). Unchanged from the original
`fork14-scenario.sh`:

1. Wallets; A and B `/getinfo`: `upgrade_heights` end in `60,140`, `supported_height` 140,
   `tx_versions [1]`; the C++ node `supported_height` 60.
2. After fork 13 a **version-1** transfer W1 -> W2 through A is mined at or below 140; A, B and
   C++ agree on that block; B has the transaction.
3. Block 140 is v6, block 141 **v7** on A and B; the coinbase of 141 is version 2 with only
   tagged outputs (type `03` + `view_tag`), that of 140 version 1; A `tx_versions [1,2]`.
4. (4a) **v1 in the grace period**: a version-1 send is accepted and mined in 141..160.
5. (3b) A transfer with the default version (2) and `MIXIN_V2` decoys is mined with tagged
   outputs and `clsag_signatures`; B has it in the same block; W2's balance through B lists the
   v2 and both v1 outputs and reports outputs skipped by view tag.
6. (4b) After 160 A reports `tx_versions [2]` and a **version-1 send is refused**: `invalid
   transaction: transaction version not allowed at this height`.
7. (5) The C++ node stays at 140 (A's v6 block) with the deserialisation error in its log; A
   logs `peer is on an older fork` with "fork 14"; B logs no older-fork peer; A and B
   `/status.fork` `active_index` and `supported_index` 14.

### 32-p2pool: p2pool through fork 13 and fork 14 (optional)

`testnet/scenarios/32-p2pool.sh`, helpers in `testnet/lib-p2pool.sh`; patches in
`p2pool/README.md`. **Optional** (`optional=1` in its `# suite:` header): without the image
`xkr-p2pool:testnet` it prints `MISSING 32-p2pool/image: image xkr-p2pool:testnet ... missing;
get it with: p2pool/build.sh ...` and exits 2 when run directly, and the suite turns that into a
`SKIP` row, so the standard suite stays green without the image.

The image (p2pool at `8edf632` + `p2pool/patches/0001-fork13-v6.patch` +
`0002-fork14-tx-v2-coinbase.patch` + the testnet-only `0003-testnet-uncapped-pplns.patch`, plus
xmrig v6.21.0 with donation level 0) is built once, alone (no other heavy build next to it):

```sh
p2pool/build.sh
# = nice -n 19 docker build --build-arg JOBS=3 -t xkr-p2pool:testnet -f p2pool/Dockerfile p2pool/
```

It is a long C++ build (p2pool with grpc's vendored BoringSSL sources, then xmrig; tens of
minutes, up to about 1 GiB per compile job): run it on its own, never next to a cargo build, a
suite run or another image build. The compilers run inside the docker daemon, so a memory cap
on the calling shell only reaches the docker client; the Dockerfile runs every `make` under
`nice -n 19` with `JOBS` (3) jobs, and `MEMORY=4g p2pool/build.sh` adds a hard limit (legacy
builder). If a build fails only on p2pool's `-Werror`, `p2pool/build.sh --build-arg WERROR=0`
builds with warnings off.
For the optional negative check: `IMAGE=xkr-p2pool:unpatched p2pool/build.sh --build-arg
PATCHES=`.

Topology: xkrd A (P2P 22898, RPC 22899, `--fork13-height 16 --fork14-height 24 --fork14-grace
10 --out-peers 0`) mines `PHASE0_BLOCKS` (10) blocks with the built-in miner to W0, then
restarts without a miner, so every later block is p2pool's; xkrd B (RPC 22889) follows A;
p2pool A (wallet W1, sidechain P2P 22890, stratum 22333) and p2pool B (W2, 22891/22334,
`--addpeers` A) in containers from the image with `--network host`, `XKR_FORK13_HEIGHT` /
`XKR_FORK14_HEIGHT` and `XKR_TESTNET_UNCAPPED_PPLNS` (= `PPLNS_UNCAPPED`, 1) in the environment
and `p2pool/sidechain-testnet.json`
(`/etc/p2pool/sidechain-testnet.json`, minimum share difficulty 100 000, PPLNS window 60); one
xmrig per p2pool node (`cn-pico`, `XMRIG_THREADS` = 1). Testnet difficulty is 1, so every
stratum submission of xmrig (job difficulty ~1000) is a main-chain block and the chain moves
about once per 2-s `get_miner_data` poll, while a sidechain share needs ~100 000 hashes (one
per ~100 s per ~1-kH/s xmrig thread): most of the run is the wait for both nodes' first shares
(`SHARE_WAIT`, 600 s at most). Upstream p2pool caps the PPLNS window at twice the main-chain
difficulty, i.e. at one share on this testnet; patch 0003 lifts the cap when
`XKR_TESTNET_UNCAPPED_PPLNS=1`.

Checks (what a PASS proves):

1. xkrd serves `get_miner_data` (height = top + 1, major 5, `prev_id` = top hash, hex
   difficulty, `unlock_window` 1) and `get_block_headers_range`, and two polls on an unchanged
   chain are byte-identical (p2pool's dedup). Missing methods: `MISSING 32-p2pool/rpc`.
2. Both p2pool nodes log `XKR fork 13 (block major version 6) at height 16, signalling fork
   index 14` and the fork-14 line (heights taken from the environment because xkrd reports
   `"testnet": true`) and, with `PPLNS_UNCAPPED=1`, `XKR testnet: PPLNS weight not capped at 2x
   the block difficulty` (an image without 0003 fails here: rebuild), poll `get_miner_data`
   without "response is invalid", build a major-5 template, peer with each other, and xmrig's
   shares are accepted.
3. Blocks H0 + 1 .. F13 are p2pool's: major 5, **minor 14** (`EXPECT_MINOR`; the fork index the
   0001 + 0002 build signals from the start; a 0001-only build writes 13), version-1 coinbase
   with untagged outputs; xkrd A logs `submitted block accepted` for them and refuses none.
4. Fork 13: block F13 is v5, F13 + 1 .. F14 are **v6** (minor 14, version-1 coinbase) built and
   submitted by p2pool; both nodes built major-6 templates and are still peered; the number of
   `new miner data` lines stays near the number of blocks. Before the restart (8) each node has
   found a share (`SHARE FOUND`) and added one of the other's (`add_block ... verified = 0`),
   and both report the same `sidechainHeight`. Shared PPLNS: with `PPLNS_UNCAPPED=1` some
   coinbase, at least one after that share exchange, pays both W1 and W2; with
   `PPLNS_UNCAPPED=0` every coinbase pays exactly one of them (the upstream cap).
5. Fork 14: block F14 is v6 with a version-1 coinbase; F14 + 1 .. are **v7 with a version-2
   coinbase whose outputs are all tagged** (`"type":"03"` + `view_tag`), and p2pool logs `BLOCK
   ACCEPTED` for them; every template p2pool built had its height's major (5/6/7); no p2pool
   log line matches the error list (`sent an invalid block`, `daemon wants block major
   version`, `wrong view tag`, `wrong PoW`, `failed to submit a share`, `daemon returned
   error`, unusable RPC answers).
6. `xkr-testwallet balance` of W1 and W2 through A owns **every output of every p2pool coinbase
   exactly once** (v5/v6 by full derivation, v7 through the view-tag fast path, which skips an
   output whose tag does not match: full coverage proves every tag and payee); W1's `scan` line
   reports tagged outputs and outputs skipped by view tag.
7. A version-2 transaction W0 -> W2 sent right after F14 appears in `get_miner_data`'s
   `tx_backlog` and is mined in a p2pool v7 block; W2's balance lists it.
8. `docker restart` of p2pool A: it logs `loaded N cached blocks` (N > 0: its v5/v6/v7 shares
   parse again), reconnects to B, mines again, and logs no parser or share error.
9. Optional: (9a, `NEGATIVE=1` + `xkr-p2pool:unpatched`) an unpatched p2pool C peered with A
   logs `sent an invalid block` for the v6 shares (run it with `PPLNS_UNCAPPED=0`: C cannot lift
   the PPLNS cap and would also reject multi-payee shares); (9b, `OPTIONAL_CPP=1` + the fork-13 C++ image,
   container `xkr-suite-p2pool-cpp`, P2P 22868 / RPC 22869) the C++ node has A's headers for
   p2pool's v5/v6 blocks and stops at F14. SKIP when off.
10. xkrd B has the same headers as A for 1 .. T (all p2pool blocks); a summary table of heights
    F13, F13 + 1, F14, F14 + 1 and the top (major/minor, coinbase version, outputs, tagged,
    payees).

Logs under `<wd>/32-p2pool/`: `xkrd-a-phase0.log`, `xkrd-a.log`, `xkrd-b.log`,
`p2pool-<node>/p2pool.log` (and its data API under `p2pool-<node>/api/`),
`xkr-suite-{p2pool,xmrig}-<node>.log` (docker logs), `balance-w1.out`, `balance-w2.out`,
`send-v2.out`.

What it cannot show: p2pool against mainnet-sized chains and real difficulty (testnet
difficulty 1 makes every share a block and the upstream PPLNS cap meaningless, so the capped
window, uncles and the penalty zone are not exercised), many p2pool nodes, compact or
pruned share broadcasts, the compiled-in release heights (the testnet uses the
environment variables), Hugin or mixin-invalid transactions in `tx_backlog` (the pool holds
only the one v2 transfer), and the C++ daemon as p2pool's daemon (p2pool talks only to xkrd
here).

### 33-pruned-peers: stripe-pruned peers

`testnet/scenarios/33-pruned-peers.sh` (`docs/node-architecture.md`, "Pruned peers";
`docs/storage.md`, "Stripe pruning"). Default scenario: it needs nothing beyond the old image.
Stripes are 4 096-block spans, so the scenario needs a chain of more than 11 776 blocks: xkrd A
syncs the shared seed chain, then runs without peers (the seed chain stays as it is) and
extends its own chain to `PRUNED_TOP` (12 300) with `xkr-testwallet mine --to`: a
`getblocktemplate` + `submitblock` loop that sets each template's merge-mining tag like a miner
does (`set_merge_mining_tag`, C++ `adjustMergeMiningTag`; the template's zero merkle root fails
the proof of work check) and keeps nonce 0, since the testnet's fixed difficulty 1 accepts any
hash. The built-in miner would also work but spaces its blocks at least 1 s apart; most of the
run time is this step. A copy of A's `chain.redb` is pruned with `xkrd db prune --stripes 4 --stripe 1 --below
8192 --force-below` (testnet never prunes by itself, since only genesis is a checkpoint, so 8 192
is above its safety bound of 0 and needs the override) and started as xkrd P:
it keeps span 0 (0..4 095) and everything from 8 192 up, and drops span 1 (4 096..8 191).
`PRUNED_TOP` must keep span 1 outside P's recent blocks as a downloader sees them
(`PRUNED_TIP_BLOCKS` = 7 680 below P's height).

1. A synced the seed chain.
2. A extended its chain to `PRUNED_TOP` with `xkr-testwallet mine` (blocks/s printed; five
   refused blocks in a row fail the step).
3. `db prune` reports `seed stripe 1/4 (0x0401), pruned below 8192`; `db verify` on the pruned
   copy finds 0 problems.
4. P's `/getinfo` `pruning_seed` is 1025 (A's 0); P's `/status.pruning` is seed 1025, full false,
   stripes 4, stripe 1, at A's top.
5. Fresh xkrd F with only P (`--add-exclusive-node`) reaches top 4 095 (span 0, equal to A's
   block) and stays there; its `/status.pruning.missing_stripe` is 1026 (stripe 2 of 4) with
   `pruned_peers` 1, it logs "no connected peer holds these blocks in full", keeps P
   (`legacy_outgoing` 1) and nobody is banned.
6. A fresh C++ node (old image, `xkr-suite-pruned-cpp`, only P) syncs what P holds (top 3 895..4
   095: a partial answer makes it drop the connection without using the blocks) and keeps
   reconnecting: at least 3 inbound connections on P within 180 s, no ban. This is a separate
   container rather than the shared seed, which would otherwise sync P's longer chain.
7. F restarted with P and A reaches A's top; its blocks 4 095, 4 096, 8 191, 8 192 and the top
   equal A's, `missing_stripe` is null again, and `db verify` on F finds 0 problems.

Logs under `<wd>/33-pruned-peers/`: `a-seed.log`, `a-extend.log`, `a.log`, `p.log`,
`p-cpp.log`, `f-p.log`, `f-pa.log`, `mine.out`, `db-prune.out`,
`db-verify-{p,f}.out`, `xkr-suite-pruned-cpp.log`.

Not covered: Hyperswarm stripe topics and the rotation of wrong-stripe peers (unit tests in
`xkrd::hyper` and `xkrd::downloader`; F has only explicit peers, which are never rotated), and
automatic pruning as the chain grows (it needs a checkpoint above the prune height).

### 34-p2pool-mixed: the C++ p2pool and xkrd's built-in p2pool on one sidechain (optional)

`testnet/scenarios/34-p2pool-mixed.sh`, helpers in `testnet/lib-p2pool.sh`. **Optional** like
32: without the image `xkr-p2pool:testnet` it prints `MISSING 34-p2pool-mixed/image ...` and the
suite reports a `SKIP` row. It also needs what 20 needs (xkrd with libbare-kit, `node`,
`bridge/node_modules`) and `xkr-miner`.

Topology (own ports, so it can run next to 32, 40 and 65; all nodes with `--fork13-height 16
--fork14-height 24 --fork14-grace 10`; every p2pool node on `p2pool/sidechain-testnet.json`
with the PPLNS weight cap lifted):

* xkrd **X** (P2P 22798, RPC 22799, `--no-hyper --out-peers 0`) mines `PHASE0_BLOCKS` (10)
  blocks with its built-in miner to W0 and restarts without it: every later block is p2pool's.
* C++ p2pool **C** (`xkr-suite-p2pool-mixed-c`, daemon X, wallet WC, sidechain P2P 22790,
  stratum 22343, `XKR_TESTNET_UNCAPPED_PPLNS=1`, fork heights from the environment); xmrig on
  its stratum.
* xkrd **R1** (RPC 22789): Levin to X only (`--add-peer`, no listener), Hyperswarm on a local
  HyperDHT (as 20 and 40), `--p2pool --p2pool-config p2pool/sidechain-testnet.json
  --p2pool-uncapped-pplns --p2pool-allow-same-ip --p2pool-p2p 127.0.0.1:22791
  --p2pool-stratum 127.0.0.1:22344 --p2pool-addpeers 127.0.0.1:22790`, payout WR1 (`--hugin`);
  xkr-miner on its stratum.
* xkrd **R2** (RPC 22779): no legacy peer (`--p2p-bind-port 0`, dead seed, `--out-peers 0`),
  `--p2pool` with stratum 22345, no `--p2pool-p2p`, no `--p2pool-addpeers`, payout WR2: it
  reaches the main chain and the sidechain only over Hyperswarm, through R1. `R2_MINER=1` puts
  an xkr-miner on its stratum too.

Testnet difficulty is 1, so every stratum submission is a main-chain block, while a share needs
~100 000 hashes. C reads new blocks from X's `get_miner_data` every 2 s, xkrd's p2pool on every
new block, and both implementations drop a broadcast share whose main-chain parent is 2 or more
blocks behind (and ban the peer at 5 or more). A full-speed xkr-miner would move the main chain
several blocks per C poll and make C's shares stale on arrival, so each xkr-miner is stopped
(SIGSTOP) for `R_MINER_PAUSE` (2) seconds after every accepted submission. A share of R1 then
takes about 100 x `R_MINER_PAUSE` seconds; most of the run is the wait for the share exchange
(`SHARE_WAIT`, 1200 s at most).

Checks (what a PASS proves):

1. X mined phase 0 and restarted without a miner; R1 reached X's top over Levin, R2 over
   Hyperswarm only; `/status` `p2pool.state` is `running` on both.
2. One sidechain: C logs `consensus ID = 9b8712b7…53248686` (the middle masked), R1 and R2 log
   `p2pool: consensus id 9b8712b7…53248686` and report the full id in `/status`; C logs the
   0003 line and R1/R2 `p2pool: testnet sidechain, PPLNS weight not capped`. (The P2P handshake
   hashes the consensus id, so the peering in 3 also needs equal ids.)
3. R1 has C as an outgoing TCP peer and R2 as a Hyperswarm peer (`peers_out`, `peers_hyper`),
   C has a connection; R2 has `peers_hyper >= 1`, `peers_out = peers_in = 0`, and no legacy
   main-chain peer (`/status.peers`).
4. Every block after phase 0 has its height's major version (5 up to F13, 6 up to F14, 7 after),
   the v7 ones a version-2 coinbase with only tagged outputs; X refused no `submitblock`. (Blocks
   come every ~2 s and shares every ~100 s, so the fork heights are passed with C's blocks
   before the first share; the shares of 5 are v7 shares. Higher `F13`/`F14`, e.g. 300/600,
   puts shares of both implementations before the forks at the cost of a longer run.)
5. Shares cross both implementations and both transports: shares C found (`add_block ...
   verified = 1`) appear in R1's and R2's `p2pool: share added` lines, shares R1 found
   (`p2pool: share found`) in C's `add_block ... verified = 0` lines and in R2's
   `share added`; with `R2_MINER=1` R2's shares in R1 and, relayed by R1, in C (SKIP when R2
   found none).
6. The sidechains agree: C's `sidechainHeight` (data API `pool/stats`) and R1's and R2's
   `/status` `p2pool.height` are within `SC_SLACK` (1) of each other, and a share at the lowest
   of the three heights is known to all three (`/status` has no tip id and C logs only the
   height of a new tip, so the share ids of the log lines stand in for the tips); R1 and R2
   logged `p2pool: synchronized` and report `synced`.
7. C logs `BLOCK ACCEPTED`; R1's `p2pool: block found height=H id=<hash>` blocks are on X's
   chain with that hash (SKIP only when every R1 block lost the race for its height to a block
   of C as an alternative block and no error line exists).
8. `xkr-testwallet balance` of WC, WR1 and WR2 through X owns every output of every p2pool
   coinbase exactly once (v7 through the view-tag fast path): both implementations split the
   reward over the same wallets. After the share exchange some coinbase built by xkrd and some
   built by C pay both WC and WR1 (each one's PPLNS window holds the other's share); with R2's
   share exchanged, a later coinbase pays WR2.
9. No line of C's error list (32's list plus `banned for`), no `p2pool: peer ... rejected` and
   no refused, unsubmitted, orphaned or invalid found block in R1's or R2's log; xmrig and
   xkr-miner had submissions accepted and none rejected; R1 and R2 have X's headers for every
   height. Then a summary table (heights F13, F13 + 1, F14, F14 + 1, after the exchange, top:
   major/minor, coinbase version, outputs, builder and payees).

Logs under `<wd>/34-p2pool-mixed/`: `xkrd-x-phase0.log`, `xkrd-x.log`, `xkrd-r1.log`,
`xkrd-r2.log`, `xkr-miner-r1.log` (`-r2`), `bootstrap.log`, `p2pool-mixed-c/p2pool.log` (data API
under `p2pool-mixed-c/api/`), `xkr-suite-{p2pool,xmrig}-mixed-c.log`, `balance-wc.out`,
`balance-wr1.out`, `balance-wr2.out`.

What it cannot show: real difficulty (every share is a block; uncles and the capped window stay
rare), the default sidechain and its seeds, many nodes, C's own Hyperswarm (C++ has none), a
restart of the built-in p2pool (its cache), and an exact tip id comparison (see 6).

### 40-hugin-relay: xkrd as a hugin-node

Local DHT, p2pool A (image `xkr-p2pool:testnet`; without it the fake stratum pool stands in and
the p2pool check is a SKIP), A with the relay (on by default) and `--hugin <address>`, B
`--hugin-private` with the default fee address, the app simulator
`bridge/test/hugin-client-sim.js --testnet` (`docs/hugin-relay.md`, "Local test").

* `topics`: A announces its 3 Hugin topics, B its 2; address and invites appear in the logs.
* `jobs`, `login`, `fee`: A's relay logs in to the pool with its fee address; B says it donates
  its fee.
* `public`: the app finds A on the testnet public-node topic, gets a job, posts a paid DM, syncs
  it back and receives the `new-message` push.
* `shares`, `p2pool` / `submit`: A's `/status` counts the accepted share under its fee address;
  p2pool logs the share under the fee address (on the testnet it is a block).
* `mesh`: B has the message (node-mesh gossip or the xkrd sync), via B's private invite.
* `private`: the app posts through A's private-node invite.
* `restart`: A restarted still has the message (`<data dir>/hugin/messages.jsonl`).

### 50-storage: the database tools on the suite chain

1. `xkrd db stats` on S's database (S stopped) prints the `file / raw` and `tables / raw` ratios.
2. On a copy: `db compact --force` (repack + compact, `compacted:` line), `db verify` 0 problems,
   `db stats` with the same top (ratios before -> after printed); `db verify` on the original.
3. With `XKR_V1_DB=<schema v1 chain.redb>` (`XKR_V1_NETWORK`, default testnet): `db convert` into a
   new v2 file, `db verify` 0 problems, stats. Without it: SKIP (a fresh testnet has no v1
   database; schema-v1 testnet databases from earlier runs do).

### 60-64: the Rust wallet tools next to the C++ ones

Shared helpers: the last section of `testnet/fixtures/lib-wallet.sh` (`wl_*`, `rapi_*`,
`rwd_*`, `rcli_*`). The Rust programs run as local processes (`nice -n 19`, working directory
`<sd>/w`), the C++ ones in containers that see `<sd>/w` as `/w`, so both open the same files.
Seeds and keys are the fixed ones of the fixture captures (seed A, seed B, subwallet keys S1 and
S2). Sync checks use `>=`; an answer that is not the JSON asked for is a FAIL that shows it.
Snapshots of every compared answer stay in `<sd>/snap/`, transcripts in `<sd>/tx/`.

**60-wallet-api.** C++ `wallet-api` and `xkr-wallet-api` both import seed A and S1, S2; X pays A
(blocks 6..35, filler to 140). The two programs then must report the same `/balance`,
`/balances`, `/transactions` (transfers and list sorted), `/status` (block counts, view flag,
subwallet count) and `/addresses` after the coinbase, after the sends and after fusion. Sends
alternate: C++ basic to S1's Xkr form; Rust basic to S1 with a payment ID and Rust advanced
(Xkr destination, a foreign address, 15 small outputs to S1, payment ID, source and change
address); C++ basic to an integrated address and C++ advanced (15 small outputs to S2's Xkr
form); every send mined. Fusion: Rust on S1, C++ on S2, both mined with fee 0. Files: both save
and close, C++ opens a copy of the Rust file and Rust a copy of the C++ file, and each reports
what the other did before closing.

**61-walletd.** C++ `kryptokrona-service` and `xkr-walletd` both generate a container from seed
A (`-g --mnemonic-seed`), add S1 with `createAddress`, and create and delete a random address.
Compared after the coinbase and after the sends: `getBalance` (wallet and per address),
`getAddresses`, `getTransactions` from `firstBlockIndex` 1 over the whole chain, `getStatus`
(block counts, last block hash) and `getDelayedTransactionHashes`. `validateAddress` of three Xkr
forms: valid and the same answer. Sends: C++ `sendTransaction` to S1's Xkr form with a payment ID
and change address; Rust to S1 and a foreign address with source and change address; delayed
transactions in both (create two, list, send one, delete one). `export` in both; C++ opens the
Rust export and Rust the C++ export, with the same balances, addresses and transactions.

**62-wallet-migration.** Four C++ files from seed A: `wallet-api` (WalletBackend file),
`kryptokrona-service -g` plus S1 (container), `xkrwallet` and `xkrwallet-beta` `seed_restore`
(container, WalletBackend file). The C++ `wallet-api` sends 1 XKR away; its balance is B and the
other C++ tools agree. `xkr-wallet-api`, `xkr-walletd` and `xkrwallet` (script mode) each open a
copy of every file: balance B (12 checks). A container a Rust tool used keeps its bytes and gets
`<file>.xkrstate`; `xkr-walletd` reopens it from that state, scans again without it, and after
`createAddress` rewrites the container, which the C++ walletd then opens with all three
addresses. Back: the C++ `wallet-api` opens the six WalletBackend files the Rust tools saved,
the C++ walletd five containers they used, the C++ `xkrwallet` and `xkrwallet-beta` the files
`xkrwallet` left: balance B every time.

**63-wallet-fork14.** X with `--fork13-height F13 --fork14-height F14 --fork14-grace GRACE`
(40, 160, 20). `xkr-wallet-api` holds seed A, the C++ `wallet-api` seed B, both on xkrd. Before
F14 both send version 1, mined at or below F14. At F14 + 1 (`tx_versions [1,2]`) the Rust send
has a listed version (the engine takes 2) and the C++ version-1 send is still mined, both in the
window. The next block pays A with a version-2 coinbase that the Rust wallet lists. After
F14 + GRACE (`tx_versions [2]`) the Rust send is version 2 and mined, and the C++ send is
refused with `transaction version not allowed at this height` (its answer or X's log, which has
the `xkr_rpc::handlers` debug lines) with nothing added to the pool: F14 + GRACE is the last
block at which C++ wallets on xkrd can send. The C++ wallet still syncs past F14 and lists the
version-2 transfers the Rust wallet sent it.

**64-wallet-cli.** `xkrwallet` without a terminal, fed line by line: `restore` of seed A into
`a.wallet` (synced), `send` 1 XKR to the Xkr form of a foreign address (password confirmed; the
transaction reaches X), `txs`, `export` (`transactions.csv` with the C++ header), `back`
(closing steps, start menu), `open a`, `exit` (status 0). After the send is mined, `-w a.wallet
-p <password>`: `txs out` shows `-1.0001 XKR`, `export` has the OUT line. With the C++ image:
`xkrwallet` and `xkrwallet-beta` restore seed A first, and `xkrwallet -w <file> -p ""` opens
both files for A (the container keeps its bytes and gets `.xkrstate`), prints A and its Xkr
form, and shows the same balance as `a.wallet`.

### 65-miner: the standalone xkr-miner, solo and in a pool

1. `xkr-miner` with the C++ miner's flags (`--daemon-address=S --address=W1 --threads=1
   --scan-time=1 --limit=SOLO_BLOCKS`, plus `--network testnet --log-json`) mines
   `SOLO_BLOCKS` (5) blocks on S and exits 0; S logged as many `submitted block accepted` and no
   `refused`; the JSON lines have one `started`, `SOLO_BLOCKS` `accepted` (each with a
   `block_hash`), one `limit`, one `stopped`.
2. The C++ seed has the same top and identical headers for those heights.
3. `--daemon-host 127.0.0.1 --daemon-rpc-port 17899 --limit 2` (text output) mines 2 more and
   prints `Block found! Hash:` twice and `Mined requested amount of blocks (2). Quitting.`
4. `P2POOL=1` (the default; 0 turns it off): a fresh testnet xkrd A (ports of 32) mines
   `PHASE0_BLOCKS` (10) with its built-in miner and restarts without it; with the image
   `xkr-p2pool:testnet` p2pool A runs on A, without it A runs xkrd's built-in p2pool itself
   (`--p2pool --p2pool-config p2pool/sidechain-testnet.json --p2pool-stratum 127.0.0.1:22333`,
   payout W1 through `--hugin`, `/status` `p2pool.state` `running`);
   `xkr-miner --pool stratum+tcp://127.0.0.1:22333 --limit POOL_SHARES` (3) gets a job, has 3
   shares accepted and none rejected, exits 0, and A grows (every testnet share is a block);
   with the built-in p2pool A also logs `p2pool: block found` for them. The PASS lines name
   which p2pool was used (`image` or `built-in`).

### 90-cleanup

Stops every process with a pid file in `<wd>/pids`, removes every container labelled
`xkr-suite` (their logs go to `<wd>/<container>.log`), then checks that no tracked process, no
labelled container and no suite port is left.

## 5. Knobs

Environment variables (all optional; the scenario headers list their own):

| variable | default | meaning |
|---|---|---|
| `XKRD`, `XKR_TESTWALLET` | `target/release/…` | binaries |
| `CPP_OLD_IMAGE`, `CPP_FORK13_IMAGE` | `xkr-cpp-testnet:master`, `:fork13` | C++ images |
| `NODE` | `node` | Node.js for the local DHT, pool and simulator |
| `HUGIN_NODE_MODULES` | `../hugin-desktop/node_modules` (next to this repo) | 40 (`HUGIN_FORKS_NODE_MODULES` is no longer used) |
| `TIMEOUT` | per scenario (600; 900 in 20/30, 180 per step in 40) | each wait inside a scenario; setting it overrides all |
| `SEED_BLOCKS` | 100 | length of the seed chain |
| `LOG_LEVEL` | `info` (40: `info,xkr_hugin=debug`) | xkrd `--log-level` (a tracing filter or a C++ number 0-5); the scenarios wait for info lines, so keep `info` or more (xkrd alone defaults to warnings) |
| `DB_CACHE_MB` | 64 | `--db-cache-mb` of every xkrd |
| `CMP_FIELDS` | `major_version minor_version timestamp difficulty reward num_txes block_size` | header fields the differential walks compare (plus the hash) |
| `CPP_MINE_BLOCKS`, `BUILTIN_BLOCKS` | 10, 10 | 12 |
| `LIVE_BLOCKS` | 3 | 10 |
| `OLD_BLOCKS`, `FORK_OFFSET`, `AFTER`, `OLD_SLACK`, `OLDER_FORK_WAIT` | 30, 60, 20, 1, 300 | 30 |
| `F13`, `F14`, `GRACE`, `MIXIN_V2`, `CPP_SLACK` | 60, 140, 20, 2, 1 | 31 |
| `F13`, `F14`, `GRACE`, `PHASE0_BLOCKS`, `V7_BLOCKS` | 16, 24, 10, 10, 4 | 32 |
| `P2POOL_B`, `RESTART`, `OPTIONAL_CPP`, `NEGATIVE`, `EXPECT_MINOR` | 1, 1, 0, 0, 14 | 32 |
| `XMRIG_THREADS`, `XMRIG_ALGO`, `BLOCK_WAIT`, `RESTART_WAIT`, `P2POOL_LOGLEVEL` | 1, `cn-pico`, 300, 180, 3 | 32 |
| `P2POOL_IMAGE`, `P2POOL_UNPATCHED_IMAGE` | `xkr-p2pool:testnet`, `xkr-p2pool:unpatched` | 32 |
| `F13`, `F14`, `GRACE`, `PHASE0_BLOCKS`, `V7_BLOCKS` | 16, 24, 10, 10, 4 | 34 |
| `R2_MINER`, `R_MINER_THREADS`, `R_MINER_PAUSE` | 0, 1, 2 | 34 (xkr-miner on R2; threads and pause after each accepted submission of the xkr-miners) |
| `SHARE_WAIT`, `POST_SHARE_BLOCKS`, `SC_SLACK`, `BLOCK_WAIT` | 1200, 5, 1, 300 | 34 |
| `XMRIG_THREADS`, `XMRIG_ALGO`, `P2POOL_IMAGE`, `P2POOL_LOGLEVEL` | as in 32 | 34 |
| `SOLO_BLOCKS`, `P2POOL`, `PHASE0_BLOCKS`, `POOL_SHARES`, `MINER_WAIT` | 5, 1, 10, 3, 300 | 65 |
| `XKR_CPP_PEER`, `XKR_CPP_RPC`, `PUBLIC_DHT` | seed, seed, 0 | 20 |
| `XKR_V1_DB`, `XKR_V1_NETWORK` | unset, testnet | 50 |
| `MIN_DISK_MB`, `MIN_MEM_MB` | 2048, 4096 | 00 |
| `XKR_WALLET_API`, `XKR_WALLETD`, `XKR_WALLET_CLI` | `target/release/…` | 60-64 |
| `WL_IMAGE`, `WALLET_LABEL`, `WL_PASS` | `xkr-cpp-wallets:master`, `xkr-wallets`, `wl-pass` | 60-64 |
| `FX_SYNC_WAIT`, `FX_CONFIRM` | 240, 12 | 60-64 (longest wait for a wallet to sync; blocks mined after sends) |
| `FX_CLI_DELAY`, `FX_CLI_TIMEOUT`, `FX_CLI_MAX_BYTES` | 1.5, 240, 1 MiB | 62, 64 (terminal wallet sessions) |
| `F13`, `F14`, `GRACE`, `F14_LOG_LEVEL` | 40, 160, 20, `info,xkr_rpc::handlers=debug` | 63 |
| `KEEP` | 0 | 1 = leave the scenario's nodes running (`--keep` sets it) |

A run with non-default knobs is a different run: record them next to `results.json` when the
run is evidence for a fork decision.

## 6. Adding a scenario

1. Pick a number: 0x preflight, 1x the shared chain (sync, serve, mine, transact, persist),
   2x transports, 3x forks, 4x Hugin, 5x storage, 6x wallet tools and miner, 9x cleanup. Name it
   `testnet/scenarios/NN-short-name.sh`; the suite picks it up by the glob, in number order.
2. Header: line 2 is `# NN-short-name: <one-line summary>` (shown by `--list`), then the topology,
   the checks in words, the env knobs, and `# suite: expected=<s> timeout=<s>` (append
   `optional=1` for a scenario whose prerequisite is not part of the standard setup: the suite
   then reports its MISSING as SKIP).
3. Body:

   ```bash
   set -euo pipefail
   : "${TIMEOUT:=600}"                 # scenario default, before the lib
   . "$(dirname "$0")/../lib.sh"
   suite_init "${1:-}"                 # WD, SCEN, SD=<wd>/<scenario> (wiped), traps
   require_xkrd; require_image "$CPP_OLD_IMAGE" "$BUILD_OLD_IMAGE"   # exit 2 with the build command
   ensure_suite_chain "$SD/xkrd-S.log" # or start_xkrd / start_cpp for an own network
   if <condition>; then pass 1 "what was shown"; else fail 1 "what was seen instead"; fi
   finish
   ```

   Helpers: `pass|fail|skip <check> <text>`, `die` (FAIL and stop), `missing_exit`,
   `wait_for <s> <cmd…>`, `wait_height`, `wait_same_top`, `compare_chains`, `rpc`, `post`,
   `get`, `num|str|arr|bool|obj|count` (JSON without jq), `top_of`, `height_of`, `top_hash_of`,
   `header_at`, `start_xkrd <name> <rpc port> <dir> <log> [args]`, `stop_proc`, `kill_proc`,
   `watch` (wait loops die if the process exits), `start_cpp <container> <image> <p2p> <rpc>
   <ENV=v|-> [args]`, `run_cpp_miner`, `mine_cpp`, `wallet`, `send_tx`, `wait_mined`,
   `unlocked_of`, `wait_funds`, `xkrd_db`.
4. Rules: new fixed ports go into the table in `testnet/lib.sh` and §3 here (no overlap, bind
   127.0.0.1); state only under `$SD` (or `<wd>/chain` through the lib); every process through
   `start_bg`/`start_xkrd`/`start_cpp` (nice, pid file, cleanup); containers `--network host`
   with the `xkr-suite` label (done by `start_cpp`); C++ nodes always with
   `--add-exclusive-node`; never build; check ids stable (other tools key on
   `<scenario>/<check>`); a check that cannot run is a SKIP with the reason, never a silent PASS.
5. Document its assertions in §4 and its row in the duration table.

## 7. Pre-fork checklist

Nothing below is automated beyond the suite; each item is a yes/no before the release.

### (a) Fork 13 on mainnet ("flag day", block major version 6)

1. **Height.** `F13` lies at least two weeks after the release date: 2 weeks
   at 90 s = 13 440 blocks; about one month (~28 800 blocks) leaves room. `F13` must be below
   the fork-14 height 3 000 000 (xkrd warns at start-up otherwise).
2. **C++ v1.2.** Built from `testnet/patches/0001-fork13-testnet.patch` turned into a fixed
   mainnet height: the 14th `FORK_HEIGHTS` entry and `UPGRADE_HEIGHT_V6 = F13`,
   `SOFTWARE_SUPPORTED_FORK_INDEX = 13`, no environment override in mainnet builds. Its mainnet
   `/getinfo` reports `supported_height = F13` and `upgrade_heights` ending in `F13`.
3. **xkrd** with `FORK_13_HEIGHT_MAINNET = Some(F13)`; `/getinfo.upgrade_heights` of xkrd and
   C++ v1.2 identical on mainnet.
4. **Suite green on the fork-13 image with that height** baked in:

   ```sh
   cd testnet
   XKR_CPP_IMAGE=xkr-cpp-testnet:fork13-F13 XKR_CPP_PATCHES=0001-fork13-testnet.patch \
       FORK13_HEIGHT=<F13> ./build-cpp.sh
   # the baked default is the one a node uses without XKR_FORK13_HEIGHT:
   docker run -d --rm --name f13-check --network host xkr-cpp-testnet:fork13-F13 \
       /usr/src/kryptokrona/kryptokronad --data-dir=/tmp/f13 --rpc-bind-ip=127.0.0.1 \
       --rpc-bind-port=15999 --p2p-bind-port=15998 --add-exclusive-node=127.0.0.1:9 --no-console
   curl -s 127.0.0.1:15999/getinfo | jq .supported_height      # = F13
   docker rm -f f13-check
   CPP_FORK13_IMAGE=xkr-cpp-testnet:fork13-F13 testnet/suite.sh /tmp/xkr-suite-f13
   ```

   All scenarios `PASS` (30 schedules its own low height at run time; the image must still pass
   everything with the real height compiled in), `results.json` archived with the release.
5. **Readiness.** `/getinfo.fork_readiness` on a mainnet xkrd (share of the last 720 blocks
   signalling fork 13) at least 0.8 for 48 h before `F13`
   before the height is final. An unpatched p2pool writes minor 16, which does not count as a
   signal, so p2pool must be updated first (with `p2pool/patches` it writes 14; `32-p2pool`
   check 3), or its share measured separately.
6. **Notified, with the height and date:** p2pool maintainers (v6 templates, minor version),
   pools, exchanges, explorers, node operators and hugin-node operators; release notes published
   with both binaries at least two weeks before `F13`.
7. The **mainnet dry run** (c) near the height.

### (b) Fork 14 on mainnet (provisional height 3 000 000, tx version 2)

1. **Suite green including `31-fork14`**, also once with the mainnet grace ratio in mind (e.g.
   `GRACE=200 F14=400` to exercise a longer grace) and with `MIXIN_V2` at the mainnet ring size.
2. **Wallet libraries updated and tested against a 31-fork14 network** (`testnet/suite.sh <wd>
   --keep 31`, then point them at RPC 14899): `kryptokrona-wallet-backend-js`, `kryptokrona-rs`
   / `kryptokrona-crypto-rs`, the Hugin apps (desktop, native), zedwallet/the C++ service if
   still shipped: parse `view_tag` outputs (type `03`) and version-2 transactions, build v2 with
   CLSAG, use `/getinfo.tx_versions` (`docs/tx-v2.md` §9).
3. **p2pool v2 coinbase**: version 2 with tagged outputs (one view tag per payout) from `F + 1`,
   parses v2 transactions in templates; tested against xkrd on the testnet: `32-p2pool` PASS
   (not SKIP: the image must be built), with the release's patches.
4. **Grace period**: 28 800 blocks (30 days) of v1 after `F14`
   (`FORK_14_PARAMS_MAINNET.grace_blocks`), announced with the height.
5. Fork 13 has happened (or is scheduled below 3 000 000) and C++ nodes are retired or ported:
   no C++ node can follow fork 14.
6. Notified as in (a)6, with the tx-v2 wire changes.

### (c) Mainnet dry run (near a fork height)

Run a release-candidate xkrd (with the mainnet height compiled in) next to the live network a
few days before the fork and through it, without mining and without announcing anything new:

```sh
xkrd --network mainnet --data-dir ~/.xkr-dryrun --rpc-bind-port 11998 --p2p-bind-port 11997 \
     --log-json > ~/.xkr-dryrun/xkrd.jsonl 2>&1 &
# every minute: our fork state, readiness and top vs. a public C++ node
while sleep 60; do
    date -u +%FT%TZ
    curl -s 127.0.0.1:11998/status | jq -c '{height, synced, fork, block_rate, peers}'
    curl -s 127.0.0.1:11998/getinfo | jq -c '{height, supported_height, fork_readiness, tx_versions}'
    curl -s -d '{"jsonrpc":"2.0","id":0,"method":"getlastblockheader","params":{}}' \
        http://<public C++ node>:11898/json_rpc | jq -c '.result.block_header | {height, hash}'
    curl -s -d '{"jsonrpc":"2.0","id":0,"method":"getlastblockheader","params":{}}' \
        127.0.0.1:11998/json_rpc | jq -c '.result.block_header | {height, hash}'
done | tee -a ~/.xkr-dryrun/watch.log
```

Watch `/status.fork`: before the fork `next_index` / `next_height` = the fork and `readiness`
rising; at `F + 1` `active_index` switches, `next_block_major_version` becomes 6 (7 for fork
14), the first block after `F` has that major version, and the top hash keeps matching the
upgraded C++ nodes (and for fork 13, stops matching the non-upgraded ones, which stay at `F`).
In the log: `block-version fork active; peers that stay at or below the fork height will be
dropped` once, `peer is on an older fork` for peers that did not
upgrade (expected), no `block rejected` for upgraded peers' blocks, no block-rate warning. Any
divergence between xkrd and the upgraded C++ nodes is a stop: the C++ node is the reference
until fork 13.
