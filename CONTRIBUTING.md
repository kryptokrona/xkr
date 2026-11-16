# Contributing to xkr

**Toolchain.** Rust 1.100 with rustfmt and clippy, pinned in `rust-toolchain.toml` (rustup
installs it on first use). The fuzz targets in `fuzz/` are a separate workspace on nightly
(`fuzz/README.md`). Building xkr-p2p-hyper (and so xkrd) needs libbare-kit: run
`scripts/build-bare-kit.sh` once (it needs git and Node.js with npm; on NixOS docker). The
Hyperswarm bridge in `bridge/` runs on Bare inside xkrd; its tests need Node.js 22 or later.

**Before you open a pull request**, run what CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Builds are heavy; on a shared machine add `-j 3` (and `-- --test-threads=3` for tests). Tests
that need `node` and `bridge/node_modules` are `#[ignore]`d; run them with
`cargo test -p xkr-p2p-hyper --test loopback -- --ignored --test-threads=1` after `npm ci` in
`bridge/`.

**Language.** All code, comments, docs and commit messages are in English.

**Dependencies.** Every dependency is declared once in `[workspace.dependencies]` in the root
`Cargo.toml` with an exact version (`"=1.2.3"`, never `*` or a caret range); crates refer to
it with `{ workspace = true }`. Keep `Cargo.lock` committed. Dependency fetches go through
Socket Firewall (`sfw cargo fetch`, `sfw npm ci`) when available. Vendored code and its local
changes are recorded in `vendor/VENDORED.md` and `crates/xkr-crypto/VENDORED.md`.

**Testnet suite.** Changes to consensus, P2P, RPC, storage or the Hyperswarm/Hugin code must
pass `testnet/suite.sh <work dir>` against the C++ node (`testnet/suite.sh --list` lists the
scenarios). It needs the release binaries and the C++ Docker images and never builds them;
see `docs/testnet-suite.md` and `testnet/README.md`.

**Input limits.** Code that parses network or database input keeps the size, count and rate
limits pinned by the property tests (`crates/*/tests/prop_*.rs`) and the fuzz targets
(`fuzz/`); run them after changing a parser. Please report problems you would rather not
discuss in public through GitHub's private reporting form, not in a public issue.

By contributing you agree that your contribution is licensed under GPL-3.0-or-later (`LICENSE`).
