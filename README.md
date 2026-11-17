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

```sh
target/release/xkrd                       # mainnet, data in ~/.xkr, RPC 127.0.0.1:11898
target/release/xkrd --network testnet     # testnet
target/release/xkrd --help
```

- `--fast-sync` skips proof of work up to the last checkpoint.
- `--prune` keeps 25 % of the old ring signatures (about 45 % less disk space).
- `--explorer` answers the block explorer methods (`f_*`, C++ `--enable-blockexplorer`). An
  explorer needs the whole chain: not with `--prune`, and not on a pruned database.
- `--no-hyper` runs the legacy TCP network only.
- `--port <udp port>` for a node behind a strict firewall or NAT.
- `--private` runs a private node: it syncs from the network as a client and takes peers and
  Hugin apps only through its invite (console `invite`, `invite new` for a new one).
  `--private-peer <invite>` keeps another node connected to it.
- p2pool runs inside the node (`--no-p2pool` turns it off); `start_mining p2pool` in the console
  mines on it.

### Hugin relay

The Hugin relay is on by default (`--no-hugin` turns it off). It stores encrypted Hugin
messages for up to 24 hours for the apps. Apps pay with mining shares, which go to the node's
fee address. The default is a donation to nilsjr; set your own with `--hugin <address>` or
`fee-address <address>` in the console. Details: `docs/hugin-relay.md`.

## Docs

`docs/` covers the architecture, storage, forks, transaction version 2, mining, the Hyperswarm
transport, the Hugin relay and the testnet suite. `packaging/` builds the release archives.

## License

GPL-3.0-or-later (`LICENSE`). Vendored code keeps its MIT licence; see `vendor/VENDORED.md`
and `crates/xkr-crypto/VENDORED.md`.
