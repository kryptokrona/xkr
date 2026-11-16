# p2pool patches for forks 13 and 14

Patches against the Kryptokrona p2pool (`https://github.com/kryptokrona/p2pool`), base commit
**`8edf632fae7ebcbb29d3525144ac77b0dbd6477e`** ("Increment patch version to 2"). The RPC p2pool
uses: `docs/mining.md` "External p2pool"; the testnet recipe: `docs/testnet-suite.md` "32-p2pool".

| file | what |
|---|---|
| `patches/0001-fork13-v6.patch` | fork 13: block major version 6 in the sidechain's fork table (otherwise peers reject every v6 share), fork heights from `XKR_FORK13_HEIGHT` (compile time, or the environment against a testnet daemon), minor version 13 (the fork-index signal) instead of 16, an error log when the daemon's major version disagrees with the table |
| `patches/0002-fork14-tx-v2-coinbase.patch` | fork 14 (on top of 0001): major version 7, a version-2 coinbase with tagged outputs (`0x03`, key, view tag) from major 7 on, the parser's version/tag binding, the view-tag check in `SideChain::verify`, minor version 14, `XKR_FORK14_HEIGHT` (default 3 000 000 = xkrd's provisional mainnet height) |
| `patches/0003-testnet-uncapped-pplns.patch` | testnet only (on top of 0002): `XKR_TESTNET_UNCAPPED_PPLNS=1`, honoured only against a daemon that reports `"testnet": true`, lifts upstream's PPLNS weight cap (2x the main-chain difficulty), which on the difficulty-1 testnet leaves one share per window so that every coinbase pays only the node that built it; a sidechain consensus rule, so every node of the test sidechain sets it alike. Without the variable nothing changes |
| `Dockerfile` | p2pool at the base commit with the patches (0001-0003), plus xmrig (CN-Turtle stratum miner, donation 0) for the testnet; build arguments `PATCHES`, `XKR_FORK13_HEIGHT`, `XKR_FORK14_HEIGHT`, `CXX_DEFS`, `WERROR`, `JOBS` (header of the file) |
| `build.sh` | builds the image `xkr-p2pool:testnet` (`IMAGE`, `JOBS` = 3, `MEMORY`) under `nice -n 19` (and the optional `CAPPED` wrapper); a long C++ build, run it alone |
| `sidechain-testnet.json` | private sidechain for the test network (own consensus id, PPLNS window 60) |

Apply in order from the checkout root:

```sh
cd p2pool        # a clone of github.com/kryptokrona/p2pool at 8edf632
patch -p1 < ../xkr/p2pool/patches/0001-fork13-v6.patch
patch -p1 < ../xkr/p2pool/patches/0002-fork14-tx-v2-coinbase.patch
patch -p1 < ../xkr/p2pool/patches/0003-testnet-uncapped-pplns.patch
```

All three apply without offset or fuzz (checked with `patch -p1` on a copy of the tree); each
needs the ones before it. A mainnet release needs only 0001 + 0002 (0003 is inert there). Build with at most 3 jobs.

Guarding: everything consensus-relevant is keyed on the block major version (which comes from
the daemon's `get_miner_data` for the own template and from the fork table for peers' shares),
so below the fork heights the coinbase, the block layout and the parser rules are those of the
unpatched p2pool. The one intended change before the forks is the block minor version (13 or
14 instead of 16): it is the readiness signal xkrd counts, no node validates it, and although
the byte enters the aux hash, PoW blob and sidechain id, peers recompute those from the
received blob, so patched and unpatched nodes share a sidechain until `F13`.

Fork heights (a fork at `F` governs blocks `F + 1, ...`, as in xkrd and the C++ patch):

* release builds: `-DXKR_FORK13_HEIGHT=<h>` / `-DXKR_FORK14_HEIGHT=<h>` (Docker build arguments
  `XKR_FORK13_HEIGHT` / `XKR_FORK14_HEIGHT`, or `CXX_DEFS`); defaults 0 (fork 13 not scheduled) and 3 000 000 (fork 14);
* testnet: the environment variables `XKR_FORK13_HEIGHT` / `XKR_FORK14_HEIGHT`, honoured only
  when the daemon's `get_info` reports `"testnet": true` (Kryptokrona addresses carry no
  network, so p2pool cannot tell otherwise). They must equal xkrd's `--fork13-height` /
  `--fork14-height`, on every p2pool node of the sidechain.

Testnet regression: `testnet/scenarios/32-p2pool.sh` (optional suite scenario, skipped without
the image) runs xkrd from genesis through both forks with two patched p2pool nodes and xmrig
and checks the templates, the blocks, the coinbases and the wallets (`docs/testnet-suite.md`
"32-p2pool"). Build the image first, alone:

```sh
p2pool/build.sh                      # = nice -n 19 docker build --build-arg JOBS=3 -t xkr-p2pool:testnet ...
testnet/scenarios/32-p2pool.sh /tmp/xkr-suite
```
