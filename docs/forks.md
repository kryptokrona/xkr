# Forks: the fork table, fork 13 and the testnet scenario

Status: written 2026-11-13; forks 13 and 14 are scheduled on mainnet (2 700 000 and 2 750 000,
set on 2026-11-17) and testable on the testnet (§4, §5).

## 1. The fork table (`xkr-consensus::forks`)

The C++ node spreads fork knowledge over three unrelated lists in
`src/config/cryptonote_config.h`:

* `FORK_HEIGHTS[0..=12]` + `SOFTWARE_SUPPORTED_FORK_INDEX = 12`: purely informational. It is
  only read by `/getinfo` (`upgrade_heights`, `supported_height`) and the daemon's
  "update needed" status (`format_tools.cpp`). No consensus code looks at it.
* `UPGRADE_HEIGHT_V2..V5`: the block major versions. `Core`, `BlockchainCache` and
  `DatabaseBlockchainCache` each register them with an `UpgradeManager`
  (`addMajorBlockVersion(version, currency.upgradeHeight(version))`), whose
  `getBlockMajorVersion(index)` returns the highest version with `upgradeHeight < index`.
* per-rule constants: `MIXIN_LIMITS_V*_HEIGHT`, `LWMA_*_DIFFICULTY_BLOCK_INDEX`,
  `DUST_THRESHOLD_V2_HEIGHT`, `BLOCK_BLOB_SHUFFLE_CHECK_HEIGHT`, ...

`ForkSchedule` joins them into one table per network. Each entry (`Fork`) has

| field | meaning |
|---|---|
| `index` | position in C++ `FORK_HEIGHTS` |
| `height` | `F`, or `None` = not scheduled |
| `major_version` | block major version after the fork (5 for 0..=12, 6 for 13) |
| `minor_version_signal` | minor version that signals support (= index) |
| `tx_versions` | allowed transaction versions (v1 only up to fork 13) |
| `rules` | `Rules` bit set of what changed |

| index | height | rules |
|---|---|---|
| 0 | 187 000 | `ZAWY_DIFFICULTY` (dead branch for major >= 3) |
| 1 | 350 000 | none (release marker) |
| 2 | 440 000 | `MIXIN_LIMITS_V1` |
| 3 | 620 000 | `LWMA_2`, `MIXIN_LIMITS_V2`, `DUST_THRESHOLD_V2`, `FUTURE_TIME_LIMIT_V3` |
| 4 | 700 000 | `LWMA_2_V2`, `FUTURE_TIME_LIMIT_V4` |
| 5 | 800 000 | `LWMA_2_V3`, `MIXIN_LIMITS_V3`, `FUSION_DUST_THRESHOLD_V2`, `TIMESTAMP_WINDOW_V3` |
| 6, 7 | 1 000 000, 1 200 000 | none |
| 8 | 1 300 000 | `MAX_EXTRA_SIZE_V2` (wallet only) |
| 9, 10, 11 | 1 400 000, 1 600 000, 1 800 000 | none |
| 12 | 2 200 000 | `LWMA_3` |
| 13 | mainnet: 2 700 000; testnet: `--fork13-height` | `MAJOR_VERSION` (v6) |
| 14 | mainnet: 2 750 000; testnet: `--fork14-height` (§5) | `MAJOR_VERSION` (v7), `TX_VERSION_2`, `VIEW_TAGS`, `CLSAG` |

Off-table rules (C++ heights that are not `FORK_HEIGHTS` entries): `MIXIN_LIMITS_V4` at
1 250 000 and `BLOCK_BLOB_SHUFFLE_CHECK` + `TX_SIGNATURE_COUNT` at 1 750 000
(`OFF_TABLE_RULES`). The testnet uses the same `FORK_HEIGHTS` (C++ has no testnet variant); only
its `UPGRADE_HEIGHT_V2..V5` are 0.

**Activation convention.** A fork at height `F` governs blocks `F + 1, F + 2, ...`; block `F`
is the last block under the old rules. That is the `UpgradeManager` rule
(`upgradeHeight < blockIndex`, which is why mainnet v2 starts at height 2, see
`docs/core-crates.md`) and also how LWMA is selected (by the *previous* block index `>= F`).

API: `ForkSchedule::{supported_fork_index, fork, forks, fork_at, is_fork_active, has_reached,
next_fork, last_version_fork_at, signal_target, block_major_version_for, upgrade_height,
upgrade_heights, supported_height}` (fork 14 additions in §5); `NetworkParams::forks()`,
`NetworkParams::with_fork13_height()` and `with_fork14_height()`.

* `is_fork_active(i, h)` = `h > F` (the convention above); `has_reached(i, h)` = `h >= F`, the
  comparison the historical `*_HEIGHT` rules use (mixin limits, dust).
* `block_major_version_for(h)` replaces the old loop over `upgrade_heights`
  (`block_major_version_for_height` delegates to it).
* `next_difficulty` selects LWMA-2/LWMA-3 through `is_fork_active`. The other historical rules
  keep their exact C++ comparison in their own function; a unit test pins every such constant to
  its table height, so the table and the rules cannot drift apart. Mainnet behaviour is
  unchanged below 2 700 000: the table yields the old versions there. With forks 13 and 14
  scheduled (§2, §5), `/getinfo` `upgrade_heights` has 15 entries (the 13 C++ heights,
  2 700 000 and 2 750 000) and `supported_height` is 2 750 000.
* `params::FORK_HEIGHTS` / `SOFTWARE_SUPPORTED_FORK_INDEX` / `CURRENT_FORK_INDEX` keep the C++
  v1.2.2 values (index 12); xkrd's own supported index is `forks::SUPPORTED_FORK_INDEX = 13`.

## 2. Fork 13 ("flag day")

* Block major version **6**. Block and transaction format, PoW (CN-Turtle-Lite v2, already
  "major >= 5"), difficulty (LWMA-3), reward, full reward zone, timestamps: identical to v5.
* Validation: `validate_block_header_and_coinbase` compares the block's major version with
  `block_major_version_for(height)`, so v6 is required from `F + 1`, a v5 block at `F + 1` and a
  v6 block at `<= F` are `WRONG_VERSION`. `check_proof_of_work` accepts 1..=6.
  `xkr-serialization::MAX_BLOCK_MAJOR_VERSION` is 6 (decoding is not consensus; the height check
  is).
* Heights: `FORK_13_HEIGHT_MAINNET = Some(2_700_000)` (set on 2026-11-17; block 2 700 001 is
  the first v6 block), `FORK_13_HEIGHT_TESTNET = None`; `xkrd --network testnet --fork13-height F`
  schedules it at run time (`ChainConfig::fork13_height`; refused on mainnet; 0 = not scheduled).
* Templates and the built-in miner build v6 after `F` (`Chain::block_template` uses the table).
* **Signalling.** The C++ daemon writes minor version 0 into its templates
  (`Core::getBlockTemplate`; both branches give 0 because every upgrade height is defined) and
  the `UpgradeManager` never reads minor versions (`SimpleUpgradeDetector`; the voting
  `BasicUpgradeDetector` is unused). p2pool writes 16, Monero's `HARDFORK_SUPPORTED_VERSION`
  (`p2pool/src/common.h`), which is what the mainnet blocks with minor 16 are. xkrd templates
  therefore write the supported fork index, 13, as the signal (`template_minor_version`; never 1,
  the old vote value). A block signals fork `t` if `minor >= t` and `minor != 16`.
* **Readiness.** `/getinfo.fork_readiness` = share of the last 720 blocks (18 h) that signal the
  next block-version fork, scheduled or not (so maintainers can watch it before choosing a
  height); `null` when no such fork remains. Counted from the block blobs, cached per tip.
  `/status.fork` = `{active_index, next_index, next_height, supported_index,
  next_block_major_version, signal_index, readiness, signalling, window}`.
* **Peer policy** at the fork (drop + 10 min ban of peers that stay at or below `F` for 150 s or
  send v5 blocks after it, "peer is on an older fork"): `docs/node-architecture.md`, "Peers at a
  fork". **Block-rate monitor**: same section.

## 3. The C++ patch (`testnet/patches/0001-fork13-testnet.patch`)

Base `cf5a63e16b64200adb5445ea97d4186a0fdcb88a` (master, v1.2.2), applied with `patch -p1`.
Details in `testnet/patches/README.md`. In short: `BLOCK_MAJOR_VERSION_6`, a 14th
`FORK_HEIGHTS` entry and `SOFTWARE_SUPPORTED_FORK_INDEX = 13` for testnet builds (or with
`-DXKR_FORK13_HEIGHT=<h>`), `parameters::fork13Height()` (environment variable
`XKR_FORK13_HEIGHT` on testnet, else the compile-time value), `Currency::upgradeHeight(6)`,
`addMajorBlockVersion(6, ...)` in the three upgrade managers, `serializeBlockHeader` and
`checkProofOfWork` accept 6, `/getinfo` reports the run-time height. A mainnet build without the
definition is unchanged, which is the shape of the real C++ v1.2 release: there the line becomes
a fixed `FORK_HEIGHTS` entry and `UPGRADE_HEIGHT_V6` once the height is picked.

Why old nodes stop: the unpatched `serializeBlockHeader` throws "Wrong major version" for
`majorVersion > 5`, so the first v6 block cannot even be parsed. In sync the node logs `sent
wrong block: failed to parse and validate block` (ERROR), for a relayed lite block
`Deserialization of Block Template failed` (WARNING), drops the connection and stays at `F`
(or a few blocks below if it lagged: C++ rejects the whole `NOTIFY_RESPONSE_GET_OBJECTS` batch
when one block fails to parse). It does not ban and keeps retrying. With no miner of its own it
never forks off; an old node *with* miners would continue a v5 chain that xkrd and patched nodes
refuse (`WRONG_VERSION`), and xkrd drops such peers at once.

## 4. The scenario (`testnet/fork13-scenario.sh <work dir>`)

Now `testnet/scenarios/30-fork13.sh`, part of the regression suite (`docs/testnet-suite.md`;
the old path is a wrapper). It starts its own old node (`xkr-cpp-testnet:master`, container
`xkr-suite-f13-old`, P2P 19898, RPC 19899) that mines 30 blocks from genesis, so `xkr-tn2` is
no longer needed (`OLD_NODE_PEER`/`OLD_NODE_RPC` still select an external one); the patched
container is `xkr-suite-f13-patched`. The steps below are unchanged.

Old node (`xkr-cpp-testnet:master`, default the running container `xkr-tn2`) <- xkrd
(`--fork13-height F --mine`, P2P 14898, RPC 14899) <- patched node (`xkr-cpp-testnet:fork13`,
`--network host`, P2P 15898, RPC 15899, `XKR_FORK13_HEIGHT=F`, exclusive peer xkrd).

1. `H` = old node's top index, `F = H + 60`.
2. Phase A: xkrd without the miner and the patched node start; the patched node must report
   `supported_height = F`.
3. All three agree on height and top hash below `F` (nothing is mined yet, so the check is not
   racy). Phase B: xkrd restarts with `--mine <xkr-testwallet address> --mine-threads 1`
   (testnet difficulty 1: one block per second).
4. Past `F + 20`: block `F` is v5 and `F + 1` v6 on xkrd and the patched node, both agree at
   their common height with v6; the old node sits at `F` (or `F - OLD_SLACK`) and its log shows
   one of the two deserialisation messages.
5. The old node's top block is xkrd's block at that height (it forked off nothing), xkrd logs
   `peer is on an older fork` for its address (within 300 s; the stall rule needs 150 s), and
   `/status.fork.active_index = 13`.
6. Summary table (node, height, top hash, major version); exit status 1 on any FAIL.

The script never builds. It needs `target/release/xkrd`, `target/release/xkr-testwallet` and
the image `xkr-cpp-testnet:fork13`; all processes run under `nice -n 19`.

## 5. Fork 14: transaction version 2

Full specification, wire format, transcripts, tests and parameters in `docs/tx-v2.md`; the
RPC changes wallets and explorers need are in `docs/tx-v2.md` §9.

* **Heights.** Mainnet: `FORK_14_HEIGHT_MAINNET = Some(2_750_000)`, set on 2026-11-17
  (~128 000 blocks, about 4.5 months at 90 s, above ~2.62 M): block 2 750 001 is the first v7
  block with a version-2 coinbase; version 1 stays valid up to block 2 778 800
  (`FORK_14_PARAMS_MAINNET.grace_blocks` = 28 800, 30 days), version 2 only from 2 778 801.
  Between fork 13 (2 700 000) and 2 750 000 only the block version differs from today (§2);
  the chain warns at start-up when fork 14 is scheduled at or before fork 13.
  Testnet: `FORK_14_HEIGHT_TESTNET = None`, set at run time (below).
* Entry (`ForkSchedule::fork14_entry`): index 14, block major version **7**, minor signal 14,
  `tx_versions` 1..=2, rules `MAJOR_VERSION | TX_VERSION_2 | VIEW_TAGS | CLSAG`. It is part of
  `ForkSchedule::forks()` only once scheduled (`forks()` returns a `Vec`: 14 entries without
  fork 14, 15 with it).
* **Supported index and signalling.** `SUPPORTED_FORK_INDEX` (13) is the baseline;
  `ForkSchedule::supported_index()` is 14 on a schedule with fork 14 (mainnet now, testnet with
  `--fork14-height`), 13 otherwise. It drives `/getinfo.supported_height` (the fork-14 height),
  `/status.fork.supported_index` and the template minor version
  (`ForkSchedule::template_minor_version`): a fork-14 node writes minor **14**, which signals
  both fork 13 and fork 14 (`signals_fork` is `minor >= target`). `/getinfo.fork_readiness`
  counts blocks signalling the next block-version fork that is not active yet (`signal_target`):
  fork 13 first (unscheduled on mainnet), then fork 14.
* Parameters (`Fork14Params` in `NetworkParams::fork14`): `grace_blocks` (version 1 stays valid
  in blocks `F + 1 ..= F + grace`; mainnet 28 800, testnet 20) and `v2_ring_size`
  (`RingSizeRule::MixinLimits` on both networks, or `Range { min, max }`).
* Queries: `is_tx_v2_active(h)`, `tx_versions_at(h)`, `allows_tx_version(h, v)` (version 0
  counts as 1), `coinbase_tx_version(h)`, `allows_coinbase_version(h, v)`,
  `upgrade_height(7)`; `validation::check_transaction_version` applies them. `/getinfo` and
  `/status.fork` report `tx_versions`, the versions the next block accepts (`[1]`, `[1, 2]` in
  the grace period, `[2]`).
* **Testnet flags** (refused on mainnet, like `--fork13-height`):
  `xkrd --network testnet --fork13-height F13 --fork14-height F14 [--fork14-grace N]`
  (`ChainConfig::{fork14_height, fork14.grace_blocks}` -> `NetworkParams::with_fork14_height`
  / `with_fork14_params`; 0 = not scheduled; the grace defaults to
  `FORK_14_PARAMS_TESTNET.grace_blocks` = 20, `--fork14-grace 0` is a hard cut at `F14 + 1`).
  Every node of a test network needs the same flags.
* **Peers.** The older-fork policy (§2) is generic: once the tip is past `F14`, a peer that stays
  at or below `F14` for 150 s is dropped ("peer is on an older fork", detail "has not passed fork
  14"). The stall watch is kept per IP **and peer id**, so an up-to-date node behind the same
  address (two nodes on loopback in the scenario, NAT) no longer resets an old node's watch.
* C++ nodes cannot follow fork 14 without a real port (they cannot parse version-2
  transactions); at `F + 1` they see major version 7 and stop exactly like at fork 13 (the
  fork-13-patched node accepts up to major 6: "failed to parse and validate block" /
  "Deserialization of Block Template failed").

### The fork-14 scenario (`testnet/fork14-scenario.sh <work dir>`)

Now `testnet/scenarios/31-fork14.sh` in the regression suite (`docs/testnet-suite.md`), same
steps; the C++ container is `xkr-suite-f14-cpp`.

A fresh, private testnet from genesis (the old `xkr-tn2` container is not involved; it is stuck
at a past fork-13 height):

```
xkrd A   --mine W1, --fork13-height 60 --fork14-height 140 --fork14-grace 20,
         P2P 127.0.0.1:14898 (loopback only), RPC 14899, --out-peers 0, --no-hyper
xkrd B   same fork flags, no miner, --add-peer 127.0.0.1:14898, P2P 0, RPC 14889, --out-peers 0
C++      xkr-cpp-testnet:fork13, --network host, XKR_FORK13_HEIGHT=60, P2P 15898, RPC 15899,
         --add-exclusive-node 127.0.0.1:14898
```

1. Wallets W1 (mining address) and W2 (`xkr-testwallet`); A and B report `upgrade_heights`
   ending in `60,140`, `supported_height` 140, `tx_versions [1]`; the C++ node reports
   `supported_height` 60.
2. After fork 13 (height >= 63) W1 sends a **version-1** transaction to W2 through A; it is mined
   below 140 and A, B and the C++ node agree on that block.
3. Block 140 is v6, block 141 is **v7** on A and B; the coinbase of 141 is version 2 with only
   tagged outputs (`"type": "03"` + `view_tag` in `f_transaction_json`), the coinbase of 140 is
   still version 1; `tx_versions` is `[1, 2]`.
4. In the grace period (blocks 141..160) a version-1 send is accepted and mined (done before the
   v2 send because the grace is only ~20 s).
5. W1 sends with the **default version** (2, from `/getinfo.tx_versions`) and `--mixin 2`; it is
   mined with tagged outputs and `clsag_signatures`; B has it in the same block; W2's balance
   **through B** lists its outputs and the `scan` line shows outputs skipped by view tag.
6. After block 160 (`tx_versions [2]`) a version-1 send is refused by the pool: `invalid
   transaction: transaction version not allowed at this height`.
7. The C++ node stays at 140 (A's v6 block), its log shows the deserialisation error, A logs
   `peer is on an older fork` with "fork 14" (within 300 s), `/status.fork` on A and B has
   `active_index` and `supported_index` 14.
8. Summary table (node, height, top hash, major; the three transactions and their heights);
   exit status 1 on any FAIL.

Environment: `F13`, `F14`, `GRACE`, `MIXIN_V2`, `TIMEOUT`, `OLDER_FORK_WAIT`, `CPP_IMAGE`,
`CPP_CONTAINER`, `KEEP=1` (header of the script). It never builds; it needs
`target/release/xkrd`, `target/release/xkr-testwallet` and the image `xkr-cpp-testnet:fork13`;
all processes run under `nice -n 19`. About 7-8 minutes, most of it the 150 s stall rule.

## 6. Upgrade notes

* **Mainnet height of fork 13** (`FORK_13_HEIGHT_MAINNET`): 2 700 000, set on 2026-11-17 at
  height ~2.62 M (about 80 days ahead at 90 s). Node software with the fixed height has to be
  released well before `F` (two weeks at 90 s = 13 440 blocks) so pools, exchanges, explorers
  and node operators can upgrade; the C++ release needs the same height (§3);
  `/getinfo.fork_readiness` shows how much of the network signals the fork.
* **p2pool** accepts major versions up to 16 (`HARDFORK_SUPPORTED_VERSION`); the patches in
  `p2pool/patches/` (`p2pool/README.md`) add v6 shares from `F + 1` and make it signal fork
  index 13 instead of writing minor 16.
* Old nodes get no grace period: the version change is a hard cut at `F + 1`. A longer
  transition would need voting (`BasicUpgradeDetector`), which neither C++ nor xkr uses.
* The handshake does not announce the supported fork index; the peer policy acts after
  the fork, on a peer's height and the versions of the blocks it sends (§2).
