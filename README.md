# xkr

A Kryptokrona (XKR) node in Rust, with the wallet tools and the miner. It works with the C++
`kryptokronad` network, and every node is also a Hugin node on Hyperswarm.

| Program | Replaces |
|---|---|
| `xkrd` | `kryptokronad` (node) |
| `xkr-wallet-api` | `wallet-api` |
| `xkr-walletd` | `kryptokrona-service` |
| `xkrwallet` | `xkrwallet` |
| `xkr-miner` | `miner` |

The wallet tools and the miner take the same flags, ports, wallet files and API as the C++
programs. Moving over: `docs/wallet-tools.md`.

## Build

Rust 1.100 (pinned in `rust-toolchain.toml`).

```sh
scripts/build-bare-kit.sh   # once per checkout: libbare-kit for the Hyperswarm transport
cd bridge && npm ci && cd ..
cargo build --release
cargo test --workspace
```

## Run

The most used commands (`target/release/` in front when run from a build):

```sh
xkrd                              # node + Hugin relay + p2pool (mainnet, data in ~/.xkr, RPC 127.0.0.1:11898)
xkrd --fast-sync                  # first sync: skip proof of work up to the last checkpoint
xkrd --prune                      # keep 25 % of the old ring signatures (about half the disk)
xkrd --node                       # only the node
xkrd --node --hugin               # node + Hugin relay, no p2pool
xkrd --hugin --p2pool             # Hugin relay + p2pool without a database (main chain from public nodes)
xkrd --p2pool --mine-p2pool       # mine on p2pool without a database
xkrd --hugin-payout <address>     # your fee address for the relay and p2pool (default: a donation)
xkrd --private                    # private node: peers and apps only through its invite (console: invite)
xkrd --network testnet            # the test network
xkrd --help                       # every flag (also listed below)

xkrwallet                         # terminal wallet: the local node, else a node found over Hyperswarm
xkrwallet -r node.example:11898   # a node of your choice (as with the C++ tools)
xkrwallet -r xkr:<invite>         # your private node, over Hyperswarm
xkr-walletd -w wallet.container -p <password>        # kryptokrona-service drop-in
xkr-wallet-api --rpc-password <password>              # wallet-api drop-in
```

Modes: plain `xkrd` runs the node, the Hugin relay and p2pool (on the testnet the node and the
relay). `--node`, `--hugin` and `--p2pool` run only the ones named; `--no-hugin` and
`--no-p2pool` leave one out of plain `xkrd`. Without `--node` there is no database and no sync:
the main chain comes from public xkrd nodes over Hyperswarm.

| command | node | Hugin relay | p2pool |
|---|---|---|---|
| `xkrd` | ✔ | ✔ | ✔ (not on the testnet) |
| `xkrd --node` | ✔ | – | – |
| `xkrd --hugin` | remote nodes | ✔ | – |
| `xkrd --p2pool` | remote nodes | – | ✔ |
| `xkrd --hugin --p2pool` | remote nodes | ✔ (jobs from its own p2pool) | ✔ |

`xkrd --p2pool`: `docs/mining.md`, "p2pool without the node". `xkrd --hugin`: the relay takes
its sync state from the remote nodes and its jobs from the local p2pool, `--hugin-p2pool`, then
the p2pool of a remote node (`docs/hugin-relay.md`, "The relay without the node").
The relay's fee for the traffic it carries reaches `--hugin-payout` only through its own p2pool:
run `xkrd --hugin --p2pool` (or plain `xkrd`) to be paid. Without it the start-up says
"fees reach --hugin-payout only with --p2pool".

Wallets (`xkrwallet`, `xkr-walletd`, `xkr-wallet-api`) use a daemon address when one is given,
else a local node when one answers, else a node found over Hyperswarm whose state hash matches
the built-in one (`docs/wallet-tools.md`, "Which node a wallet uses").

### Hugin relay

The Hugin relay is on by default (`--no-hugin` turns it off, `--node --hugin` runs it without
p2pool). It stores encrypted Hugin messages for up to 24 hours for the apps. Apps pay with
mining shares, which go to the node's fee address. The default is a donation to nilsjr; set
your own with `--hugin-payout <address>` or `fee-address <address>` in the console. Details:
`docs/hugin-relay.md`.

## All xkrd commands and flags

`xkrd --help` prints the same with longer explanations.

### Commands

| command | what |
|---|---|
| `xkrd db stats` | per-table entries and bytes, page use, file and chain size |
| `xkrd db compact` | repack under-filled tables and compact the file |
| `xkrd db verify` | walk every block and cross-check all tables |
| `xkrd db state-hashes [--to H]` | print the state hash of every 4 096-block span (`docs/storage.md`, "State hashes") |
| `xkrd db verify-state` | recompute the state hashes and compare them with the stored and built-in ones |
| `xkrd db prune --stripes N [--stripe K] \| --all` | drop ring signatures (`docs/storage.md`, "Stripe pruning") |
| `xkrd db train-dict` | train a zstd dictionary and re-compress the sealed chunks |
| `xkrd db convert` | copy a schema v1 database into a new v2 database |
| `xkrd bridge-check` | start one Bare worker and stop it: checks libbare-kit and the bridge |

Console (typed while xkrd runs in a terminal): `help`, `status`, `height`, `print_pl`,
`set_log <0-4>`, `fee-address [<address>]`, `start_mining <address>|p2pool [threads]`,
`stop_mining`, `invite [new]`, `exit`.

### General and modes

| flag | default | what |
|---|---|---|
| `--network <mainnet\|testnet>` | mainnet | the network |
| `--node` | | run the node (sync, storage, RPC) |
| `--hugin` | | run the Hugin relay |
| `--p2pool` | | run the built-in p2pool |
| `--io-threads <N>` | min(cores, 4) | network and RPC runtime threads |
| `-h`, `--help` / `-V`, `--version` | | help (`-h` short, `--help` long) / version |

### Node: legacy P2P (Levin over TCP)

| flag | default | what |
|---|---|---|
| `--add-peer <HOST:PORT>` | | peer to keep connected (repeatable) |
| `--add-exclusive-node <HOST:PORT>` | | connect only to these peers (repeatable) |
| `--seed-node <HOST:PORT>` | built-in list | seed node (repeatable, replaces the list) |
| `--p2p-bind-ip <IP>` | 0.0.0.0 | listen address |
| `--p2p-bind-port <PORT>` | 11897 (testnet 11898) | listen port; 0: do not listen |
| `--p2p-external-port <PORT>` | listen port | port announced to peers (NAT forwarding) |
| `--hide-my-port` | | announce no listening port |
| `--allow-local-ip` | (testnet: on) | accept private addresses from peer lists |
| `--out-peers <N>` | 8 | outgoing connections |
| `--in-peers <N>` | 64 | incoming connections |
| `--max-conns-per-ip <N>` | 4 | incoming connections per IP (IPv6 /64) |
| `--ban-time <SECONDS>` | 86400 | how long a peer that breaks the rules stays banned |

### Node: Hyperswarm

| flag | default | what |
|---|---|---|
| `--no-hyper` | | no Hyperswarm (legacy TCP only; no Hugin relay) |
| `--port <PORT>` | a free port | UDP port of the DHT node (forward it only behind a strict firewall) |
| `--hyper-bridge-dir <DIR>` | `bridge/` next to xkrd | the Hyperswarm bridge (`$XKR_BRIDGE_DIR` too) |
| `--hyper-bootstrap <HOST:PORT>` | public bootstrap nodes | DHT bootstrap node (repeatable) |
| `--hyper-max-peers <N>` | 32 | Hyperswarm peers |
| `--hyper-client-only` | | look up and dial only, announce nothing |
| `--private` | | private node: public topics as a client, peers and apps only by invite |
| `--private-peer <INVITE>` | | keep a private node connected (`xkr:…`, repeatable) |
| `--max-clients <N>` | 200 | connections of programs that use this node over Hyperswarm (wallets, chainless p2pool and Hugin relays) |

### Node: RPC

| flag | default | what |
|---|---|---|
| `--rpc-bind-ip <IP>` | 127.0.0.1 | RPC address (no authentication: 0.0.0.0 opens it to everyone who reaches the port) |
| `--rpc-bind-port <PORT>` | 11898 (testnet 11899) | RPC port; 0: no RPC server |
| `--rpc-allowed-host <NAME>` | | also accept this name in `Host` (repeatable) |
| `--enable-cors <ORIGIN>` | | allow this web origin (repeatable; `*` for any) |
| `--explorer` | | answer the block explorer methods (`f_*`; not with `--prune`) |
| `--fee-address <ADDRESS>` | none | remote node fee address reported by `/fee` |
| `--fee-amount <AMOUNT>` | 0 | remote node fee in atomic units |

### Node: storage and verification

| flag | default | what |
|---|---|---|
| `--data-dir <DIR>` | `~/.xkr` (testnet `~/.xkr/testnet`) | data directory |
| `--db-cache-mb <MIB>` | 512 | database page cache |
| `--db-durable-secs <SECONDS>` | 5 | durable commit interval while syncing |
| `--no-db-maintenance` | | no automatic repack and compact after the first sync |
| `--prune` | | stripe pruning: keep the ring signatures of one stripe of 4 |
| `--prune-stripes <N>` | 4 | stripes of the seed `--prune` picks (2–8) |
| `--fast-sync` | | skip proof of work up to the last checkpoint |
| `--verify-threads <N>` | all cores | threads for proof of work and signatures |
| `--max-reorg-depth <BLOCKS>` | 1000 | deepest reorganisation accepted (pruned: at most 7 168) |
| `--fork13-height <HEIGHT>` | 0 | testnet only: fork 13 at this height |
| `--fork14-height <HEIGHT>` | 0 | testnet only: fork 14 at this height |
| `--fork14-grace <BLOCKS>` | 20 | testnet only: blocks after fork 14 that still take version-1 transactions |

### Mining

| flag | default | what |
|---|---|---|
| `--mine <ADDRESS>` | | mine solo to this address (needs the node) |
| `--mine-p2pool` | | mine on the built-in p2pool (needs p2pool) |
| `--mine-threads <N>` | half the cores | mining threads |

### p2pool

| flag | default | what |
|---|---|---|
| `--no-p2pool` | | no p2pool (without mode flags) |
| `--p2pool-stratum <IP:PORT>` | 127.0.0.1:3333 | stratum for miners (0.0.0.0:3333 takes miners from the network) |
| `--p2pool-stratum-ban-time <SECONDS>` | 600 | how long a stratum miner that breaks the rules stays banned |
| `--p2pool-no-autodiff` | | fixed stratum difficulty from the sidechain |
| `--p2pool-tcp` / `--no-p2pool-tcp` | TCP on | also reach sidechain peers over TCP (Hyperswarm always) |
| `--p2pool-p2p <IP:PORT>` | | accept sidechain peers over TCP here |
| `--p2pool-addpeers <HOST:PORT>` | | sidechain peers to connect to (repeatable) |
| `--p2pool-out-peers <N>` | 10 | outgoing TCP sidechain connections |
| `--p2pool-in-peers <N>` | 450 | incoming TCP sidechain connections |
| `--p2pool-allow-same-ip` | | more than one sidechain connection per IP |
| `--p2pool-mini` / `--p2pool-nano` | main | the mini or nano sidechain |
| `--p2pool-config <FILE>` | | a custom sidechain (JSON) |
| `--p2pool-uncapped-pplns` | | testnet only: no PPLNS weight cap |

### Hugin relay

| flag | default | what |
|---|---|---|
| `--no-hugin` | | no relay (without mode flags) |
| `--hugin-payout <ADDRESS>` | donation to nilsjr | fee address of the relay and p2pool (saved in `hugin/identity.json`) |
| `--hugin-p2pool <HOST:PORT>` | local p2pool, built-in nodes | p2pool stratum for the apps' shares (repeatable, in order) |
| `--hugin-share-difficulty <N>` | 200 | share difficulty of the apps' jobs |
| `--hugin-max-clients <N>` | 200 | Hugin app connections |
| `--hugin-sync-slots <N>` | 8 | apps fetching stored messages at once |
| `--hugin-queue-budget-mb <MIB>` | 256 | send budget of all app connections |
| `--hugin-max-clients-per-ip <N>` | 4 | app connections per IP (IPv6 /64) |
| `--hugin-private` | | not on the public-node topic (invited apps only) |

### Logging and console

| flag | default | what |
|---|---|---|
| `--log-level <0-5\|FILTER>` | 2 (warnings) | log level as in C++, or a tracing filter (`RUST_LOG` wins) |
| `--log-file <FILE>` | | also write the log to this file |
| `--log-json` | | JSON log lines on stdout (for a desktop app) |
| `--no-console` | | no console commands |

## Docs

`docs/` covers the architecture, storage, forks, transaction version 2, mining, the Hyperswarm
transport, the Hugin relay and the testnet suite. `packaging/` builds the release archives.

## License

GPL-3.0-or-later (`LICENSE`). Vendored code keeps its MIT licence; see `vendor/VENDORED.md`
and `crates/xkr-crypto/VENDORED.md`.
