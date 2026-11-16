# testnet/

Local Kryptokrona testnets for xkrd. The entry point is the regression suite:

```sh
testnet/suite.sh /tmp/xkr-suite          # all scenarios, about 25 min; results.tsv / results.json
testnet/suite.sh /tmp/xkr-suite 13       # one scenario
testnet/suite.sh --list                  # scenarios and expected durations
```

**Read [`docs/testnet-suite.md`](../docs/testnet-suite.md)**: what PASS proves for every
scenario, the port table, prerequisites (it never builds), results format, how to add a
scenario, and the pre-fork checklist for the real forks.

| path | what |
|---|---|
| `suite.sh` | runs `scenarios/NN-*.sh` in order, writes `<wd>/results.tsv`, `results.json`, `checks.tsv` |
| `lib.sh` | shared helpers and the port table, sourced by every scenario |
| `scenarios/` | `00-preflight`, `10-sync-from-cpp`, `11-serve-to-cpp`, `12-mining`, `13-transactions`, `14-restart-persistence`, `20-hyper-two-nodes`, `30-fork13`, `31-fork14`, `32-p2pool` and `34-p2pool-mixed` (optional: image `xkr-p2pool:testnet`), `33-pruned-peers`, `40-hugin-relay`, `50-storage`, `60-wallet-api` .. `64-wallet-cli`, `65-miner`, `90-cleanup` |
| `fixtures/` | wallet fixture captures from the C++ wallet tools (`fixtures/capture-all.sh <wd>`; ports 24xxx, see the suite doc §3) |
| `fork13-scenario.sh`, `fork14-scenario.sh`, `hyper-two-nodes.sh`, `hugin-relay-test.sh` | compatibility wrappers for `scenarios/30`, `31`, `20`, `40` |
| `build-cpp.sh`, `Dockerfile.cpp-testnet`, `patches/` | build the C++ images `xkr-cpp-testnet:master` (old node) and `:fork13` (`patches/README.md`) |
| `docker-compose.yml`, `up.sh`, `down.sh`, `status.sh` | a manual three-node C++ testnet + miner on 11898-13899 for exploring by hand; **the suite does not use it** |

Build the prerequisites first (heavy, one at a time):

```sh
nice -n 19 cargo build --release -j 3 -p xkrd
XKR_CPP_IMAGE=xkr-cpp-testnet:master XKR_CPP_PATCHES=none testnet/build-cpp.sh
XKR_CPP_IMAGE=xkr-cpp-testnet:fork13 XKR_CPP_PATCHES=0001-fork13-testnet.patch testnet/build-cpp.sh
```

## Manual C++ testnet (`docker-compose.yml`)

```sh
cd testnet
./up.sh --build          # C++ master as xkr/kryptokrona-testnet:master (make -j3, slow)
./status.sh              # heights must rise and the three top hashes must agree
```

The prebuilt `ghcr.io/kryptokrona/kryptokrona-testnet:latest` (2025-03-31) lacks
`TESTNET_FIXED_DIFFICULTY` and the `UPGRADE_HEIGHT_V2..V5 = 0` testnet change, so its miner can
stall at the V4/V5 PoW switch; use the `--build` image.

The miner mines to the address from upstream `scripts/testnet/miner-cron` by default. Testnet
uses the mainnet address prefix, so any `SEKR…` address works. To own the rewards:

```sh
mkdir -p data/wallet
# interactive: choose "create", file /wallet/miner.wallet, then "address", "exit"
docker run --rm -it --network xkr-testnet_xkrnet -v "$PWD/data/wallet:/wallet" \
    xkr/kryptokrona-testnet:master /usr/src/kryptokrona/xkrwallet \
    --remote-daemon 172.31.98.11:11899
# non-interactive alternative (address via the service's getAddresses JSON-RPC):
docker run --rm -v "$PWD/data/wallet:/wallet" xkr/kryptokrona-testnet:master \
    /usr/src/kryptokrona/kryptokrona-service --generate-container \
    --container-file /wallet/miner.wallet --container-password testnet
MINER_ADDRESS=SEKR... docker compose up -d miner
```
