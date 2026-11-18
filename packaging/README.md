# Packaging

How to produce each distributable piece of xkr, on your own machine and in CI.

| artifact | what it is | local | CI (`.github/workflows/build.yml`) |
|---|---|---|---|
| `xkr-<version>-<target>.tar.gz` (`.zip` on Windows) | node, wallet tools, miner and the bridge bundle for one target | `scripts/package-release.sh` | job `build`, one per target (`ci.yml` packs the Linux x86_64 one on every push) |
| `xkr-bridge-<version>.tar.gz` | the Hyperswarm bridge with production `node_modules`, prebuilds for every platform; input of the `build` job, not a release file | `scripts/bundle-bridge.sh` | job `bridge` |
| `SHA256SUMS` | SHA-256 of every per-target archive (each archive also holds its own `SHA256SUMS` of the files inside) | `sha256sum xkr-* > SHA256SUMS` | job `release`, tags only |
| Tauri sample app (`.deb`, AppImage, `.app`, NSIS installer) | `tauri-sidecar/`: desktop app that runs xkrd as a sidecar | `tauri-sidecar/README.md` | job `tauri`, manual runs only |

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`,
`x86_64-apple-darwin`, `x86_64-pc-windows-msvc`. Every target is built natively on its own
runner, so the tests run on the target itself; there is no cross-compiling.

## Linux: glibc 2.35 and later

The Linux archives run on glibc 2.35 and later: Ubuntu 22.04 and newer, Debian 12 and newer,
Fedora 36 and newer, Rocky/Alma 9, Arch. They are built on Ubuntu 22.04 (in an `ubuntu:22.04`
container in CI, `packaging/linux/setup-build-env.sh`), because a program linked on a newer
system needs that system's glibc. `package-release.sh --max-glibc 2.35` stops when a program,
`libbare-kit.so` or a bridge addon needs more (`packaging/scripts/check-glibc.sh`, which also
checks any unpacked archive).

The same build on your own machine, in docker (x86_64 or aarch64, the host's architecture):

```sh
scripts/build-linux-release.sh [--version 1.0.1]   # -> dist/linux-release/xkr-<version>-<target>.tar.gz
```

It builds the image `xkr-linux-build:22.04` from `packaging/linux/Dockerfile`, libbare-kit in
`target/linux-release/bare-kit` (apart from the host's `target/bare-kit`), fetches the bridge
and the crates on the host (through Socket Firewall when installed) and runs
`cargo build --offline` and `package-release.sh` in the image (3 CPUs, 5 GiB, `-j 3`).

## How the bridge travels

xkrd's Hyperswarm transport runs `bridge/src/worker.js` on Bare worker threads inside xkrd
(`docs/hyper-transport.md`, "Desktop app packaging"). It does not use bare-pack, so the bundle is
the bridge directory itself: `src/`, `package.json`, `package-lock.json` and `node_modules` from
`npm ci --omit=dev --ignore-scripts`. No package in the lock file has an install script; the
two native addons (`sodium-native`, `udx-native`) ship prebuilds for every platform inside
their npm packages (`.bare` for Bare, `.node` for Node.js), so the bundle needs no compiler and
the same `node_modules` works everywhere. `scripts/prune-prebuilds.sh` drops the other
platforms' prebuilds and the `.node` builds from a copy (about 40 MB down to about 6 MB).

xkrd finds the bridge as `bridge/` next to its executable (or `--hyper-bridge-dir`,
`$XKR_BRIDGE_DIR`). The release archives use that layout and carry `libbare-kit` next to
`xkrd`. The Tauri sample ships the bridge and libbare-kit as resources and puts its `lib/` on
xkrd's library search path (`tauri-sidecar/README.md`).

### libbare-kit

xkrd links bare-kit's C library (`libbare-kit`), which `scripts/build-bare-kit.sh` builds into
`target/bare-kit/lib`; `.cargo/config.toml` sets `LIBBARE_DIR` to it. A release build of xkrd
searches its own directory (`$ORIGIN:$ORIGIN/lib` on Linux, `@executable_path` on macOS), so
`package-release.sh` copies the library next to `xkrd` (`--bare-kit <dir>`, default
`$LIBBARE_DIR` or `target/bare-kit/lib`). Both workflows build it on every runner and cache
`target/bare-kit/lib` by the script that pins bare-kit.

## Local

All commands from the repository root. Builds are heavy; on a shared machine limit them as the
main README says (`-j 3`).

```sh
# 1. libbare-kit and the binaries
scripts/build-bare-kit.sh
cargo build --release -p xkrd -p xkr-wallet-api -p xkr-walletd -p xkr-wallet-cli -p xkr-miner

# 2. bridge bundle (needs Node.js 22+ and npm; uses `sfw npm ci` when sfw is on PATH)
packaging/scripts/bundle-bridge.sh --out dist/bridge
tar -C dist -czf dist/xkr-bridge-1.0.0.tar.gz bridge

# 3. release archive for this machine's target
target=$(rustc -vV | sed -n 's/^host: //p')
packaging/scripts/package-release.sh --target "$target" --version 1.0.0 \
    --bridge dist/bridge --out dist

# 4. checksums
(cd dist && sha256sum xkr-*.tar.gz xkr-*.zip 2>/dev/null > SHA256SUMS)
```

`package-release.sh` takes the programs from `target/<target>/release` (or `target/release`,
or `--bin-dir`), copies the bridge bundle and prunes it to the target's prebuilds, and packs
one directory. `--version` defaults to the workspace version. `--bridge bridge` also works
for a local test when `bridge/node_modules` is installed (only `src/`, `package*.json` and
`node_modules/` are taken). On Windows (Git Bash) it zips with `7z` or `zip`.

```
xkr-<version>-<target>/
  xkrd                   node
  xkr-wallet-api         REST wallet service
  xkr-walletd            JSON-RPC wallet service
  xkrwallet              terminal wallet
  xkr-miner              CPU miner
  bridge/                Hyperswarm bridge (run by xkrd on Bare): src/, package.json, node_modules/
  docs/wallet-tools.md   moving from the C++ tools
  README.md, LICENSE
  SHA256SUMS             every other file (`sha256sum -c SHA256SUMS`)
```

Every program ends in `.exe` on Windows. There are no copies under the C++ program names;
`docs/wallet-tools.md` tells users who start the tools by those names how to keep them.

Quick check of an unpacked archive:

```sh
cd dist && tar -xzf xkr-1.0.0-$target.tar.gz && cd xkr-1.0.0-$target
for b in xkrd xkr-wallet-api xkr-walletd xkrwallet xkr-miner; do
    ./$b --version
done
sha256sum -c --quiet SHA256SUMS
./xkrd bridge-check            # runs bridge/ on a Bare worker, no network
```

## CI

`.github/workflows/build.yml` runs on pushes to `main`, on `v*` tags and by hand (Actions, "Build", "Run
workflow"; tick "tauri" to also build the sample app):

1. `bridge` (Ubuntu): `npm ci` and `npm test` in `bridge/`, then `bundle-bridge.sh` and the
   `xkr-bridge-<version>.tar.gz` artifact.
2. `build` (five native runners; the two Linux ones inside `ubuntu:22.04`): clippy with `-D warnings`, `cargo test --workspace`,
   the release build of `xkrd`, `xkr-wallet-api`, `xkr-walletd`, `xkr-wallet-cli` and
   `xkr-miner`, `package-release.sh`, then a smoke test of the unpacked archive (every
   program answers `--version`, the archive's `SHA256SUMS` matches, the bridge's
   addons and modules load on a Bare worker: `xkrd bridge-check`). Linux archives pass
   `--max-glibc 2.35`. Artifact `xkr-<target>`.
3. `linux-distros`: the Linux archives start (`--version`, `xkrd bridge-check`) in Debian 12
   and 13, Ubuntu 24.04, Fedora 42 and Arch containers (arm64: Debian 12, Ubuntu 24.04).
4. `release` (tag pushes only): `SHA256SUMS` over the per-target archives (the bridge bundle
   is left out: every archive carries its own `bridge/`), all of it as the `release-<version>`
   artifact, and a **draft** GitHub release with those files (published by hand).
5. `tauri` (manual runs with "tauri" ticked): builds the sample app per target from the
   archive of step 2: xkrd, libbare-kit and the pruned `bridge/` go in as they are, so the job
   needs no Node.js.

The version is the tag name on tags and `dev-<run number>` on manual runs.

`.github/workflows/ci.yml` stays the per-push and pull-request gate (rustfmt, clippy, tests,
and the Linux x86_64 release archive packed and smoke-tested like above on `ubuntu-latest`). It is fast and cheap; the
five-platform matrix with macOS and Windows runners is too slow to run on every push, so it
lives in `build.yml` and only runs for releases and on request.

All actions are pinned to full commit SHAs with the version in a comment. In CI the npm
packages come from the registry by the lock file's integrity hashes; locally the scripts use
Socket Firewall (`sfw npm ci`) when it is installed.

## Code signing

Nothing is signed yet. macOS will quarantine downloaded binaries (`xattr -d
com.apple.quarantine xkrd`) and Windows SmartScreen will warn. For the desktop app,
`tauri-sidecar/README.md` lists what signing needs (libbare-kit and the native addons
included).
