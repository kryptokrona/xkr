# Tauri sidecar sample

A minimal Tauri 2 desktop app that runs `xkrd` as a sidecar. It is the starting point for a
desktop node and miner app, not a finished product.

What it does:

- starts `xkrd` (Tauri `externalBin`) with its own data directory under the app data directory
  (`<app data>/xkrd`, or `xkrd-testnet`), a free loopback RPC port, no legacy P2P listener
  (`--p2p-bind-port 0`), `--log-json` into `<app data>/xkrd.log` (the previous run's log is
  kept as `xkrd.prev.log`), `--fast-sync` and `--verify-threads` at half the cores;
- passes the bundled Hyperswarm bridge (`--hyper-bridge-dir <resources>/bridge`), which xkrd
  runs on Bare worker threads (no Node.js); without the bridge it starts with `--no-hyper`;
- puts `<resources>/lib`, which holds libbare-kit, in front of xkrd's library search path
  (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS, `PATH` on Windows): xkrd loads
  the library at start, with or without `--no-hyper`;
- waits until the RPC answers `/status`, then polls it every 2 s: height, network height,
  sync progress, peers (legacy out/in, Hyperswarm), version;
- mining with xkrd's built-in miner: payout address, thread count (default half of the cores,
  at most all cores), start/stop, and the miner's readout from `/status` `mining` (hashrate,
  total hashes, height and difficulty being mined, blocks accepted/rejected, last error);
- on quit sends SIGINT (Ctrl-C on Windows), waits up to 30 s for xkrd to exit, and kills it only
  after that. xkrd then unannounces from the DHT, stops its bridge and makes a durable database
  commit (`docs/node-architecture.md`, "Sidecar use").

It is its own Cargo workspace (`[workspace]` in `src-tauri/Cargo.toml`), so the xkr workspace
build never compiles Tauri, and it has its own `src-tauri/target/`.

## Mining: restart, not RPC

xkrd has no RPC to start or stop its built-in miner while running: `--mine <address>` and
`--mine-threads N` are start flags (`docs/mining.md`, "Built-in miner"), and the RPC only
reports the miner's state. So "Start mining", "Apply" and "Stop mining" save the settings and
restart xkrd with the new flags. The restart takes a few seconds (clean shutdown, start,
database open, RPC up); sync continues from where it was and the Hyperswarm key is kept in the
data directory. The app checks the address only roughly (Base58, length); xkrd checks it for
real. If xkrd does not come up with the new flags, the app restores the previous settings,
starts xkrd with them again and shows xkrd's last log lines.

The miner pauses while the node is not synchronized, so on a fresh install it starts hashing
only after the initial sync. A runtime start/stop RPC in xkrd would remove the restart; the
app only needs to call it instead of `NodeManager::set_mining`.

Settings live in `<app data>/settings.json`:

```json
{
  "testnet": false,
  "fast_sync": true,
  "verify_threads": 0,
  "hyper": true,
  "mining": { "enabled": false, "address": "", "threads": 0 },
  "extra_args": []
}
```

`0` thread counts mean half of the cores. `extra_args` are appended to xkrd's command line as
they are. The app data directory is the platform's (`~/.local/share/org.kryptokrona.xkr.desktop-sample`
on Linux, `~/Library/Application Support/org.kryptokrona.xkr.desktop-sample` on macOS,
`%APPDATA%\org.kryptokrona.xkr.desktop-sample` on Windows).

## Layout

| path | content |
|---|---|
| `src-tauri/src/main.rs` | Tauri setup, commands `node_info`, `node_status`, `set_mining`, quit handling |
| `src-tauri/src/node.rs` | the sidecar: paths, arguments, library path, start, wait for RPC, restart, clean stop |
| `src-tauri/src/rpc.rs` | a small HTTP/1.1 GET for xkrd's loopback RPC (no HTTP client crate) |
| `src-tauri/tauri.conf.json` | `externalBin`: `binaries/xkrd`; resources: `resources/bridge/` to `bridge/`, `resources/lib/` (libbare-kit) to `lib/` |
| `ui/` | static HTML/CSS/JS, no bundler, uses `window.__TAURI__` |
| `scripts/prepare-sidecars.sh` | fills `src-tauri/binaries/`, `src-tauri/resources/lib/` and `src-tauri/resources/bridge/` |
| `src-tauri/icons/` | plain placeholder icons; replace with `cargo tauri icon <1024px png>` |

## Build

Requirements on every platform: the Rust toolchain from the repository's `rust-toolchain.toml`,
Node.js 22 or later with npm (for the bridge bundle and `scripts/build-bare-kit.sh`; the app
itself ships no Node.js), and the Tauri CLI at the version that
matches the crate:

```sh
cargo install tauri-cli --version =2.12.3 --locked
```

Steps, from the repository root:

```sh
# 1. libbare-kit and xkrd for this machine
scripts/build-bare-kit.sh
cargo build --release -p xkrd

# 2. xkrd, libbare-kit and the bridge resource (runs npm ci through bundle-bridge.sh)
packaging/tauri-sidecar/scripts/prepare-sidecars.sh

# 3. run in development, or build the installers
cd packaging/tauri-sidecar/src-tauri
cargo tauri dev
cargo tauri build
```

Instead of steps 1 and 2 building everything, an unpacked release archive
(`packaging/README.md`) can supply xkrd and the bridge (libbare-kit comes from the archive
directory once the archive carries it, else from `--bare-kit-dir`):

```sh
packaging/tauri-sidecar/scripts/prepare-sidecars.sh \
    --xkrd-dir dist/xkr-1.0.0-x86_64-unknown-linux-gnu \
    --bridge-dir dist/xkr-1.0.0-x86_64-unknown-linux-gnu/bridge \
    --bare-kit-dir target/bare-kit/lib
```

A libbare-kit built on NixOS (`scripts/build-bare-kit.sh` builds it in a `node:24-bookworm`
container there) links against that container's glibc and libstdc++; build it on the oldest
system you support for anything you hand out.

### Linux

WebKitGTK 4.1 and friends (Debian/Ubuntu names):

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
    libxdo-dev libssl-dev patchelf
```

On NixOS run the build inside a shell that provides `webkitgtk_4_1`, `gtk3`, `librsvg`,
`libsoup_3`, `openssl` and `pkg-config`. `cargo tauri build` makes a `.deb`, an `.rpm` and an
AppImage in `src-tauri/target/release/bundle/`; `--bundles deb` limits it.

### macOS

Xcode command line tools (`xcode-select --install`). `cargo tauri build` makes an `.app` and a
`.dmg`. For distribution, sign and notarise: the bundler signs `xkrd` with
`src-tauri/Entitlements.plist` (JIT for V8 in libbare-kit; library validation off because
libbare-kit and the bridge's native prebuilds are not signed with your team ID, so sign them
too if you turn it back on; `DYLD_LIBRARY_PATH` allowed so xkrd finds libbare-kit in
`Resources/lib`). Untested so far: shipping libbare-kit in `Contents/Frameworks` with an extra
`@executable_path/../Frameworks` search path in xkrd would avoid the `DYLD_LIBRARY_PATH`
entitlement.

### Windows

Visual Studio Build Tools with the "Desktop development with C++" workload, and WebView2
(preinstalled on Windows 10 21H2+ and 11). Run the scripts from Git Bash. `cargo tauri build`
makes an NSIS installer and an MSI. The sidecar is started without a console window; quitting
sends Ctrl-C through xkrd's hidden console (`AttachConsole` + `GenerateConsoleCtrlEvent`) and
falls back to killing it after 30 s.

### Checking without xkrd

To type-check and lint the app without building xkrd, libbare-kit or the bridge, use placeholders (empty
files that satisfy tauri-build; the app cannot run with them):

```sh
packaging/tauri-sidecar/scripts/prepare-sidecars.sh --placeholders
cd packaging/tauri-sidecar/src-tauri
cargo check && cargo clippy -- -D warnings && cargo test
```

Commit `src-tauri/Cargo.lock` after the first build so CI can use the same dependency versions.

## Known limitations

- If the app itself crashes, xkrd keeps running (it has no "exit when the parent dies"
  option); the next start of the app then finds the database locked and shows xkrd's error.
  Stop the stray `xkrd` by hand. An `--exit-on-stdin-close` flag in xkrd would fix this.
- No auto-restart after an xkrd crash; the window shows `exited` with the last log lines.
- Only mining is editable in the window; the other settings are in `settings.json`.
- The Windows Ctrl-C path is untested on real hardware so far; if it fails, xkrd is killed
  after the timeout (the database stays consistent, the last few seconds of sync are redone).
