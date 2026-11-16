# xkr

A new Kryptokrona (XKR) node in Rust. Data-oriented, component-per-crate, backward compatible
with the C++ `kryptokronad` network, with P2P on Hyperswarm/HyperDHT so every xkr node is also a
Hugin node. Design docs are listed under "Docs" below.

## Status

Version 1.0.0, not yet released, under active development. xkrd is backward compatible with the
current C++ network: it syncs from and serves to unmodified C++ nodes (`docs/testnet-suite.md`)
and syncs mainnet from genesis to the tip (timings under "Initial sync and CPU use" below;
database sizes in `docs/storage.md`, "Measured on mainnet": 19.1 GB full, about 10 GB
stripe-pruned at 2.62 M blocks). Neither of the new forks is
active on mainnet: fork 13 has no mainnet height yet, and fork 14 (transaction version 2) carries
only a provisional mainnet height of 3 000 000 (`docs/forks.md`, `docs/tx-v2.md`); both are
exercised on private testnets.

## Download and switch

Each release has one archive per platform (`xkr-<version>-<target>.tar.gz`, `.zip` on
Windows) with the node, the wallet tools and the miner, plus copies of them under the C++
program names, so you can put the new files in place of the old ones:

| C++ program | Rust program | copy in the archive |
|---|---|---|
| `wallet-api` (REST wallet API) | `xkr-wallet-api` | `wallet-api` |
| `kryptokrona-service` (walletd, JSON-RPC) | `xkr-walletd` | `kryptokrona-service` |
| `xkrwallet`, `xkrwallet-beta` (terminal wallets) | `xkrwallet` | |
| `miner` | `xkr-miner` | `miner` |
| `kryptokronad` (node) | `xkrd` | |

The wallet tools and the miner take the same flags and config files, listen on the same ports
(8070 for both wallet services), give the same API answers and open the same wallet files and
seeds; stop the old program, keep a copy of the wallet file, start the new one with the same
command line. Point them at an `xkrd` node before fork 14. `xkrd` is the node itself: it keeps
its chain in its own database (`~/.xkr`) and syncs it on first start, so it runs next to or
instead of `kryptokronad` rather than as a renamed file. The full guide, including the fork 14
steps and a checklist, is `docs/wallet-tools.md` (also inside the archive).

`xkrd` runs the bundled `bridge/` (Hyperswarm) with Node.js 22 or later from `PATH`;
`xkrd --no-hyper` needs no Node.js. Check a download with `sha256sum -c SHA256SUMS` (the
release's file for the archives, the archive's own for the files inside).

## Building

Rust 1.100 (pinned in `rust-toolchain.toml`; rustup picks it up automatically):

```sh
scripts/build-bare-kit.sh         # once per checkout: libbare-kit for xkrd's Hyperswarm transport
cargo build --release -p xkrd     # xkrd and the test wallet xkr-testwallet
cargo build --release -p xkr-wallet-api -p xkr-walletd -p xkr-wallet-cli -p xkr-miner
cargo test --workspace
```

xkrd runs its Hyperswarm transport on Bare worker threads inside its own process, through
bare-kit's C library (`libbare-kit`), which every build of xkrd links.
`scripts/build-bare-kit.sh` builds bare-kit 3.1.0 into `target/bare-kit/lib` (needs git,
Node.js with npm and network access; on NixOS, or with `--docker`, the build runs in a
`node:24-bookworm` container), and `.cargo/config.toml` points cargo at it, so after that cargo
works as usual and `target/release/xkrd` finds the library without `LD_LIBRARY_PATH`. Without
it, building xkrd (and `cargo test --workspace`) stops with a message that names the script.
Details: `docs/hyper-transport.md`, "Building".

Release builds are heavy; on a shared machine limit the load, e.g. `cargo build -j 3` and
`cargo test -j 3 -- --test-threads=3`.

Dependencies are pinned to exact versions in the root `Cargo.toml`. Dependency fetches in this
project go through Socket Firewall (`sfw`) when available (`sfw cargo fetch`, and `sfw npm ci` in
`bridge/`). Contributing: `CONTRIBUTING.md`.

All code comments, docs and commit messages are in English.

## Running

After `cargo build --release -p xkrd` (above), the binaries are in `target/release/`.

The Hyperswarm transport (on by default) runs the JavaScript in `bridge/` on Bare worker
threads inside xkrd, with no Node.js (`bridge/node_modules` must be installed: `cd bridge &&
npm ci`); xkrd finds `bridge/` next to `target/release/` by itself (`--hyper-bridge-dir`
otherwise). On NixOS the bridge's native addons need `libstdc++` on the library path:
`LD_LIBRARY_PATH="$(dirname "$(gcc -print-file-name=libstdc++.so.6)")" target/release/xkrd`.
`--no-hyper` runs legacy Levin/TCP only.

The Hyperswarm network (node sync, the Hugin relay, the built-in p2pool) needs no open port:
holepunching connects nodes behind ordinary NATs, and xkrd picks a free UDP port. Behind a very
strict firewall or NAT, forward one UDP port to the node and start it with `--port <port>`; every
part of the Hyperswarm network then uses that one port. xkrd says so once ("Behind a strict
firewall: …") when the DHT finds the node behind such a NAT. `status` in the console and
`/status` (`hyper`: `udp_port`, `firewalled`, `nat`) show the port in use. `--port` is UDP and
separate from the legacy TCP port (`--p2p-bind-port`, 11897).

**Mainnet** (data in `~/.xkr`, P2P 11897, RPC `127.0.0.1:11898`):

```sh
target/release/xkrd
curl -s 127.0.0.1:11898/status | jq '.height, .network_height, .peers'
```

In a terminal xkrd prints the Kryptokrona header and keeps a live panel at the bottom (the
height in big digits, the sync progress, each new block; plain lines without colours or a
terminal), shows the shutdown step by step, and takes the C++ console commands (`help`,
`status`, `height`, `print_pl`, `set_log`, `fee-address`, `exit`; off with `--no-console`). Logs show warnings and errors only by default:
`--log-level 3` (or `info`, `debug`, a tracing filter, `RUST_LOG`) for more, `--log-file` to
keep them, `--log-json` for a desktop app. `xkrd --help` lists the flags by topic.

**Testnet against the C++ testnet** (data in `~/.xkr/testnet`, RPC 11899 by default; the ports
below avoid the ones of `testnet/docker-compose.yml`, started with `testnet/up.sh --build`):

```sh
target/release/xkrd --network testnet --data-dir /tmp/xkrd-tn \
    --p2p-bind-port 14898 --rpc-bind-port 14899 \
    --add-peer 127.0.0.1:11898 --add-peer 127.0.0.1:12898 --add-peer 127.0.0.1:13898
```

`--fork13-height` / `--fork14-height` schedule the forks on testnet (`docs/forks.md`,
`docs/tx-v2.md`).

**Initial sync and CPU use.** `--fast-sync` skips proof of work up to the last hard-coded
checkpoint (`crates/xkr-chain/src/checkpoints.rs`; signatures, amounts and double spends are
still verified). Limit the load with `--verify-threads N` (default: cores - 1), not with a
cgroup CPU quota: under `systemd-run -p CPUQuota=...` the rayon verification pool showed busy
threads but did a third of the work. Measured on mainnet (2026-11-14, 16-core desktop): full
verification 540 blocks/s on ~13 cores; `--fast-sync --verify-threads 8` 430 blocks/s on ~3.7
cores, with the chain thread and the network as the limit. A clean `--fast-sync` from genesis
to the tip (2.62 M blocks) took 8 h 39 min on a shared 16-core laptop with 4–8 verify threads;
`--verify-threads 4` is plenty and keeps the machine cool. `--prune` keeps one of four
signature stripes (`docs/storage.md`, "Stripe pruning").

**Built-in miner:** `--mine <SEKR address> [--mine-threads N]`; its state is `/status` `mining`.
Pools and the C++ `miner` use `getblocktemplate` / `submitblock`, p2pool `get_miner_data`
(`docs/mining.md`).

**Built-in p2pool:** `--p2pool` runs the Kryptokrona p2pool inside the node (off by default for
now; `docs/mining.md`, "Built-in p2pool"). It starts once the node is synchronized, takes local
miners on `127.0.0.1:3333` (`--p2pool-stratum <ip:port>`), joins the sidechain over TCP to
other p2pool nodes and over the Hyperswarm network, and pays every share to the node's fee
address (the one below: the donation to nilsjr until you set your own). `--p2pool-mini`,
`--p2pool-nano` or `--p2pool-config <file>` pick another sidechain, `--p2pool-p2p <ip:port>`
accepts TCP peers. The Hugin relay then mines on it directly. Its state is in `status` and
`/status` `p2pool`.

**Hugin relay:** `./xkrd` is also a hugin-node for Hugin apps (with the Hyperswarm transport).
Two defaults to know before you run it:

- **The relay is on by default.** Turn it off with `--no-hugin`. While it runs, the node stores
  other people's end-to-end encrypted Hugin messages (it cannot read them) for up to 24 hours,
  within the pool limit of 128 MiB (oldest first out), and hands them to the apps that sync.
- **The default fee address is a donation to nilsjr**, the xkrd developer. The apps pay for
  their messages with mining shares, which the node submits to p2pool (the local p2pool at
  127.0.0.1:3333 first) under its fee address. Until you set your own address the fee goes to
  that donation address: `--hugin <address>` sets and saves it, `fee-address <address>` in the
  console changes it while the node runs. With a terminal the node shows the notice at start-up
  and asks; a headless node (no terminal, e.g. a service) logs the line
  `You are donating your hugin node fee to nilsjr` once at start-up.

Other flags: `--hugin-p2pool`, `--hugin-private`, `--hugin-share-difficulty` (200, the
apps' mining difficulty), `--hugin-max-clients` (200), `--hugin-max-clients-per-ip`,
`--hugin-sync-slots`, `--hugin-queue-budget-mb`; the counters
are `/status` `hugin` (`docs/hugin-relay.md`).

**Test wallet** (testnet only, no encryption):

```sh
W=target/release/xkr-testwallet
$W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 address
$W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 balance
$W --wallet-dir /tmp/tw-a --daemon 127.0.0.1:14899 send SEKR... 12.5 --fee 0.01
```

**Database tools** (stop the node first; `--network`, `--data-dir` or `--db FILE` select it):

```sh
target/release/xkrd db stats   --network testnet --data-dir /tmp/xkrd-tn   # tables, fill, ratio
target/release/xkrd db verify  --network testnet --data-dir /tmp/xkrd-tn   # full cross-check
target/release/xkrd db compact --network testnet --data-dir /tmp/xkrd-tn   # repack and compact
target/release/xkrd db prune   --data-dir ~/.xkr --stripes 4             # keep one random stripe of 4
target/release/xkrd db prune   --data-dir ~/.xkr --all                   # keep no old signatures
target/release/xkrd db train-dict | convert                               # see --help
```

**Pruned node** (`docs/storage.md`, "Stripe pruning"): `xkrd --prune` keeps the ring signatures
of one random stripe of four below min(last checkpoint, tip − 8 192), about 10 GB on mainnet
instead of 19 GB; `--prune-stripes N` (2–8) picks another stripe count for a new seed. The seed
is stored in the database, so later starts keep pruning with or without the flag.

**Testnet regression suite** (never builds; needs the release binaries and the C++ images, see
`testnet/README.md`):

```sh
testnet/suite.sh /tmp/xkr-suite        # all scenarios, about 25 min
testnet/suite.sh /tmp/xkr-suite 13     # one scenario
testnet/suite.sh --list
```

**Fuzzing:** `fuzz/` is its own workspace on nightly; see `fuzz/README.md`.

## Packaging

`.github/workflows/build.yml` builds, lints and tests on Linux (x86_64, aarch64), macOS (arm64,
x86_64) and Windows (x86_64) on `v*` tags and on request, and packs one archive per target
(`xkrd`, `xkr-wallet-api`, `xkr-walletd`, `xkrwallet`, `xkr-miner`, the copies under the C++
names, `docs/wallet-tools.md` and the Hyperswarm bridge with its `node_modules`; libbare-kit is
not in the archive yet, see `packaging/README.md`) plus `SHA256SUMS`; a tag push makes a draft
release. The same scripts run locally; see `packaging/README.md`. `packaging/tauri-sidecar/` is
a sample Tauri 2 desktop app that runs xkrd as a sidecar (a child process) with the bridge and
libbare-kit, shows sync and peer state and controls the built-in miner.

## Docs

- `docs/node-architecture.md`: threads, P2P manager, sync, RPC surface, known gaps.
- `docs/core-crates.md`: types, serialization, crypto and consensus crates.
- `docs/storage.md`: the chain database (schema v2), sizes, the `xkrd db` tools.
- `docs/forks.md`: the fork table, fork 13 and the testnet fork scenario.
- `docs/tx-v2.md`: transaction version 2 (fork 14): view tags and CLSAG, RPC changes.
- `docs/wallet-tools.md`: moving from the C++ wallet tools and miner (operator guide).
- `docs/mining.md`: `getblocktemplate`, `submitblock`, the built-in miner, the end-to-end test.
- `docs/hyper-transport.md`: how xkrd uses Hyperswarm/HyperDHT.
- `docs/hyper-bridge-protocol.md`: the frame protocol between Rust and the Bare workers.
- `docs/hugin-relay.md`: hugin-node inside xkrd (on by default, `--no-hugin`).
- `docs/testnet-suite.md`: the testnet regression suite and what each scenario proves.
- `docs/test-vectors/`: vectors shared with the C++ and JavaScript implementations.

## License

xkr is licensed under the GNU General Public License, version 3 or (at your option) any later
version (`GPL-3.0-or-later`); the full text is in `LICENSE`. The vendored crates keep their
upstream licence: `vendor/levin` and `vendor/epee-encoding` (from Cuprate) and the CryptoNight
code in `crates/xkr-crypto/src/cryptonight/` (from Cuprate) are MIT-licensed, each with its
`LICENSE-MIT`; see `vendor/VENDORED.md` and `crates/xkr-crypto/VENDORED.md`. MIT code may be
combined with and distributed under GPL-3.0-or-later.
