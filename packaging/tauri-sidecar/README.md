# xkr desktop app

A Tauri 2 desktop app for the whole of xkr in one window: the wallet, the node, the built-in
miner, p2pool and the Hugin relay. The frontend is SvelteKit (Svelte 5) with shadcn-svelte and
Tailwind CSS, styled after the [Aesir wallet](https://github.com/kryptokrona/aesir-wallet):
its three themes (I'm blue, Hacker, Flashbang), its fonts (Montserrat, Roboto Mono), the icon
rail and the login screen.

| section | what it shows and controls |
|---|---|
| Wallet | login (password, switch wallet file, open any file, create, restore from seed), balance, sync, send, receive, history, seed words, rescan |
| Node | sync progress, height, peers, database, uptime; chain and node details folded under "Details"; restart |
| Mining | the built-in CPU miner: on/off, p2pool or solo, payout address, threads (Advanced); hashrate, shares or blocks found |
| P2Pool | the built-in p2pool: on/off, payout address; sidechain, stratum for other computers and port (Advanced); sidechain and stratum figures, an XMRig command line |
| Hugin relay | on/off, apps connected, messages, fee shares; all counters (Advanced) |
| Logs | xkrd's log, followed live |
| Settings | theme, Hyperswarm; fast sync, verification threads, testnet, the wallet's node, extra xkrd arguments (Advanced) |

## How it works

- **xkrd as a sidecar** (Tauri `externalBin`), with its own data directory under the app data
  directory (`<app data>/xkrd`, or `xkrd-testnet`), a free loopback RPC port, no legacy P2P
  listener (`--p2p-bind-port 0`), `--log-json` into `<app data>/xkrd.log` (the previous run's
  log is kept as `xkrd.prev.log`), `--fast-sync` and `--verify-threads` at half the cores. The
  bundled Hyperswarm bridge (`--hyper-bridge-dir <resources>/bridge`) runs on Bare worker
  threads inside xkrd (no Node.js); without the bridge xkrd starts with `--no-hyper`.
  `<resources>/lib`, which holds libbare-kit, goes in front of xkrd's library search path
  (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS, `PATH` on Windows).
- **Status** comes from xkrd's `/status`, polled every 2 s: chain, peers, `mining`, `p2pool`
  and `hugin` (`docs/node-architecture.md`, "Sidecar use").
- **Settings are start flags.** xkrd has no RPC to change its miner, p2pool or relay while it
  runs, so applying settings saves them and restarts xkrd with the new flags (a few seconds;
  sync continues where it was). If xkrd does not come up with them, the app restores the
  previous settings, starts xkrd again and shows its last log lines. The miner and p2pool wait
  until the node is synchronized.
- **The wallet runs in the app** (`src-tauri/src/wallet.rs`, the `xkr-wallet` crate of the main
  workspace) and syncs from the bundled xkrd, or from the node set under Settings → Advanced
  (a remote node works before the local one has synchronized). New wallets go to
  `<app data>/wallets` (`wallets-testnet` on testnet) in the wallet-api format; any file the
  wallet tools read can be opened, and old C++ wallet files are converted on first open (the
  original is kept as `<file>.backup`). The wallet saves by itself while open.
- **On quit** the wallet is saved, then xkrd gets SIGINT (Ctrl-C on Windows) and up to 30 s to
  exit before it is killed, so it unannounces from the DHT, stops its bridge and makes a
  durable database commit.

The app is its own Cargo workspace (`[workspace]` in `src-tauri/Cargo.toml`, path dependencies
on `xkr-wallet` and `xkr-consensus`), so the xkr workspace build never compiles Tauri, and it
has its own `src-tauri/target/`.

### Settings file

`<app data>/settings.json`; fields left out take their defaults:

```json
{
  "testnet": false,
  "fast_sync": true,
  "verify_threads": 0,
  "hyper": true,
  "hugin": true,
  "payout_address": "",
  "mining": { "enabled": false, "mode": "solo", "address": "", "threads": 0 },
  "p2pool": { "enabled": true, "sidechain": "default", "stratum_lan": false, "stratum_port": 3333 },
  "wallet_node": "",
  "extra_args": []
}
```

`0` thread counts mean half of the cores. `payout_address` is xkrd's `--hugin-payout`: the
p2pool payout and the Hugin relay's fee (empty: the address xkrd saved before, or its donation
address). `mining.mode` is `p2pool` (`--mine-p2pool`) or `solo` (`--mine <address>`).
`stratum_lan` binds the stratum to `0.0.0.0` instead of `127.0.0.1`. `wallet_node` is
`host:port` (empty: the bundled xkrd). `extra_args` are appended to xkrd's command line as they
are. The app data directory is the platform's (`~/.local/share/org.kryptokrona.xkr.desktop` on
Linux, `~/Library/Application Support/org.kryptokrona.xkr.desktop` on macOS,
`%APPDATA%\org.kryptokrona.xkr.desktop` on Windows). Theme and folded sections are kept in the
web view's local storage.

## Layout

| path | content |
|---|---|
| `src-tauri/src/main.rs` | Tauri setup, the commands (node, settings, wallet), quit handling |
| `src-tauri/src/node.rs` | the sidecar: settings, arguments, library path, start, wait for RPC, restart with rollback, clean stop |
| `src-tauri/src/wallet.rs` | the embedded wallet: list, create, restore, open, overview, transactions, send, seed words, rescan |
| `src-tauri/src/rpc.rs` | a small HTTP/1.1 GET for xkrd's loopback RPC (no HTTP client crate) |
| `src-tauri/tauri.conf.json` | `externalBin`: `binaries/xkrd`; resources: `resources/bridge/` to `bridge/`, `resources/lib/` (libbare-kit) to `lib/`; the frontend build and dev server |
| `src/routes/` | the pages: `wallet/` (overview, `send`, `receive`, `history`), `node`, `mining`, `p2pool`, `hugin`, `logs`, `settings` |
| `src/lib/` | `api.ts` (typed commands), `live.svelte.ts` (2 s polling), `draft.svelte.ts` (settings being edited), `theme.svelte.ts`, `format.ts`, components (`components/ui/` from shadcn-svelte) |
| `src/routes/layout.css` | Tailwind, the theme tokens (Aesir's colours) and fonts |
| `static/` | fonts and the Kryptokrona wordmark from Aesir |
| `scripts/prepare-sidecars.sh` | fills `src-tauri/binaries/`, `src-tauri/resources/lib/` and `src-tauri/resources/bridge/` |
| `src-tauri/icons/` | plain placeholder icons; replace with `npm run tauri icon <1024px png>` |

Imports inside the frontend use `#lib/...` with the file extension (`#lib/api.js`,
`#lib/components/ui/button/index.js`): SvelteKit 3 resolves `#lib` as a Node subpath import
(`package.json` `imports`), which needs the full path. shadcn-svelte components are added with
`npx shadcn-svelte@1.8.1 add <name>` (`components.json`). Dependency versions are exact
(`.npmrc` `save-exact`), as in the Cargo workspaces.

## Build

Requirements on every platform: the Rust toolchain from the repository's `rust-toolchain.toml`
and Node.js 22 or later with npm (the frontend, the Tauri CLI from `package.json`, the bridge
bundle and `scripts/build-bare-kit.sh`; the app itself ships no Node.js).

Steps, from the repository root:

```sh
# 1. libbare-kit and xkrd for this machine
scripts/build-bare-kit.sh
cargo build --release -p xkrd

# 2. xkrd, libbare-kit and the bridge resource (runs npm ci through bundle-bridge.sh)
packaging/tauri-sidecar/scripts/prepare-sidecars.sh

# 3. the frontend's packages, then run in development or build the installers
cd packaging/tauri-sidecar
npm ci
npm run tauri dev
npm run tauri build
```

`npm run tauri dev` starts the Vite dev server (`localhost:5173`, hot reload) and the app;
`npm run tauri build` builds the frontend into `build/` first. `npm run check` type-checks the
frontend (svelte-check).

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
`libsoup_3`, `openssl` and `pkg-config`. `npm run tauri build` makes a `.deb`, an `.rpm` and an
AppImage in `src-tauri/target/release/bundle/`; `--bundles deb` limits it.

### macOS

Xcode command line tools (`xcode-select --install`). `npm run tauri build` makes an `.app` and a
`.dmg`. For distribution, sign and notarise: the bundler signs `xkrd` with
`src-tauri/Entitlements.plist` (JIT for V8 in libbare-kit; library validation off because
libbare-kit and the bridge's native prebuilds are not signed with your team ID, so sign them
too if you turn it back on; `DYLD_LIBRARY_PATH` allowed so xkrd finds libbare-kit in
`Resources/lib`). Untested so far: shipping libbare-kit in `Contents/Frameworks` with an extra
`@executable_path/../Frameworks` search path in xkrd would avoid the `DYLD_LIBRARY_PATH`
entitlement.

### Windows

Visual Studio Build Tools with the "Desktop development with C++" workload, and WebView2
(preinstalled on Windows 10 21H2+ and 11). Run the scripts from Git Bash. `npm run tauri build`
makes an NSIS installer and an MSI. The sidecar is started without a console window; quitting
sends Ctrl-C through xkrd's hidden console (`AttachConsole` + `GenerateConsoleCtrlEvent`) and
falls back to killing it after 30 s.

### Checking without xkrd

To type-check and lint the app without building xkrd, libbare-kit or the bridge, use
placeholders (empty files that satisfy tauri-build; the app cannot run with them):

```sh
packaging/tauri-sidecar/scripts/prepare-sidecars.sh --placeholders
cd packaging/tauri-sidecar
npm ci && npm run check && npm run build
cd src-tauri
cargo check && cargo clippy -- -D warnings && cargo test
```

`src-tauri/Cargo.lock` and `package-lock.json` are committed so CI uses the same dependency
versions. CI builds the installers on manual runs of `.github/workflows/build.yml` with
"Also build the desktop app" (job `tauri`).

## Known limitations

- If the app itself crashes, xkrd keeps running (it has no "exit when the parent dies"
  option); the next start of the app then finds the database locked and shows xkrd's error.
  Stop the stray `xkrd` by hand. An `--exit-on-stdin-close` flag in xkrd would fix this.
- No auto-restart after an xkrd crash; the window shows `exited` with the last log lines.
- Changing mining, p2pool or relay settings restarts xkrd; runtime RPCs in xkrd for these
  would remove the restart (the app would call them instead of `NodeManager::update_settings`).
- The wallet syncs over HTTP only (the bundled xkrd or a `host:port`), not over Hyperswarm.
- No QR code on the receive page yet; one wallet is open at a time.
- The Windows Ctrl-C path is untested on real hardware so far; if it fails, xkrd is killed
  after the timeout (the database stays consistent, the last few seconds of sync are redone).
