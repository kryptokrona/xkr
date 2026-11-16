# C++ testnet patches

`Dockerfile.cpp-testnet` applies the `*.patch` files that `build-cpp.sh` selects
(`XKR_CPP_PATCHES`: `all` = every file here, `none`, or a list of names) with `patch -p1`, in
name order, to a copy of the C++ checkout (the checkout on disk is never modified). Every patch
names its base commit in its header.

## Two images

| image | patches | fork index | role |
|---|---|---|---|
| `xkr-cpp-testnet:master` | none | 12 (`FORK_HEIGHTS[0..=12]`) | today's C++ v1.2.2: the **old node** that must stop at fork 13 |
| `xkr-cpp-testnet:fork13` | `0001-fork13-testnet.patch` | 13 | "C++ v1.2 with fork 13": follows xkrd through the fork |

```sh
cd testnet
XKR_CPP_IMAGE=xkr-cpp-testnet:master XKR_CPP_PATCHES=none ./build-cpp.sh
XKR_CPP_IMAGE=xkr-cpp-testnet:fork13 XKR_CPP_PATCHES=0001-fork13-testnet.patch ./build-cpp.sh
```

Both are testnet builds (`-DTEST_NET=ON`). `./up.sh --build` builds the plain one.

## `0001-fork13-testnet.patch`

Base: kryptokrona master `cf5a63e16b64200adb5445ea97d4186a0fdcb88a` (v1.2.2). It is the C++
side of fork 13 (`docs/forks.md`): block major version 6 with the v5 block format, PoW,
difficulty and reward.

* `src/config/cryptonote_config.h`: `BLOCK_MAJOR_VERSION_6`; for testnet builds (or any build
  with `-DXKR_FORK13_HEIGHT=<h>`) a 14th `FORK_HEIGHTS` entry, `SOFTWARE_SUPPORTED_FORK_INDEX =
  13` and `parameters::fork13Height()`: the `XKR_FORK13_HEIGHT` **environment variable** read at
  start-up (testnet builds only), else the compile-time `XKR_FORK13_HEIGHT`, else 0. 0 means not
  scheduled. Mainnet builds without the definition are unchanged.
* `currency.h/.cpp`: `m_upgradeHeightV6` / `upgradeHeightV6()` (0 -> `UNDEF_HEIGHT`, so version
  6 never activates), `upgradeHeight(6)`, `checkProofOfWork` accepts 6.
* `core.cpp`, `blockchain_cache.cpp`, `database_blockchain_cache.cpp`:
  `addMajorBlockVersion(BLOCK_MAJOR_VERSION_6, upgradeHeight(6))` next to V2..V5.
* `cryptonote_serialization.cpp`: `serializeBlockHeader` accepts major 6 (the unpatched node
  throws "Wrong major version" here, which is why it stops at the fork).
* `rpc_server.cpp`: `/getinfo` `upgrade_heights` / `supported_height` show the run-time height
  (the 14th entry is left out while fork 13 is not scheduled, like xkrd).

Height convention (same as `xkr-consensus` and the C++ `UpgradeManager`): with fork height `F`,
block `F` is the last v5 block and `F + 1` the first v6 block.

The image is built once; the scenario picks `F` at run time:

```sh
docker run -d --name xkr-fork13 --network host -e XKR_FORK13_HEIGHT=5060 \
    xkr-cpp-testnet:fork13 /usr/src/kryptokrona/kryptokronad --data-dir=/data ...
```

`FORK13_HEIGHT=<h> ./build-cpp.sh` bakes a default height into the binaries instead (build
argument -> `-DXKR_FORK13_HEIGHT`).

## Expected behaviour at the fork

* patched node + xkrd (`--fork13-height F`): accept v6 from `F + 1`, refuse v5 there and v6
  before it (C++ `BlockValidationError::WRONG_VERSION`, xkrd `wrong block major version`), and
  build v6 templates after `F`;
* unpatched node: follows up to `F`, then fails to deserialise the first v6 block: in sync
  `sent wrong block: failed to parse and validate block ... dropping connection` (ERROR), for a
  relayed lite block `Deserialization of Block Template failed, dropping connection` (WARNING).
  It drops the connection, never bans, retries, and stays at top index `F`;
* xkrd drops the unpatched node with `peer is on an older fork` and bans it for 10 minutes
  (`docs/node-architecture.md`, "Peers at a fork").

`testnet/fork13-scenario.sh` checks all of this.
