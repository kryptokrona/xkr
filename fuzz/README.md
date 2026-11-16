# xkr fuzz harnesses

[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer) targets for every byte
parser that reads network, RPC or database input.

This directory is its **own Cargo workspace** (`[workspace]` in `fuzz/Cargo.toml`) and not a
member of the root workspace, so `cargo build` / `cargo test` of the node never needs
cargo-fuzz or nightly. It has its own `fuzz/Cargo.lock` and `fuzz/target/`.

| target | input | checks |
|---|---|---|
| `cn_block` | block blob / raw block (`xkr-serialization`) | no panic; decode → encode → decode is the identity; block hash on whatever decodes |
| `cn_transaction` | transaction / prefix blob, plus every `extra` parser (`xkr-types`) | no panic; round trip; hashes |
| `epee` | epee portable storage as every P2P message type (`vendor/epee-encoding`, `xkr-p2p-legacy::wire`) | no panic; protocol messages round-trip |
| `levin_header` | byte 0 = chunk size, rest = a TCP stream into `LevinMessageCodec<LegacyMessage>` | no panic; 33-byte header fields round-trip; decoded messages re-encode |
| `hyper_frame` | a bridge frame, and (byte 0 = chunk size) a stream into `FrameCodec` with a small `max_frame` (`xkr-p2p-hyper::frame`) | no panic; frames round-trip |
| `hugin_packet` | a bare-rpc frame, a stream into `BareRpcCodec` (Hugin and xkrd limits), the JSON inside through every PoW / shape / signature check and the message pool (`xkr-hugin`) | no panic; bare-rpc round trip; pool limits hold |
| `storage_record` | block records, chunk bodies, varints, key stripping, zstd frames (`xkr-storage`) | no panic; round trips; compress → decompress identity |
| `rpc_json` | path + body (or a JSON-RPC envelope) through the axum router against a genesis-only chain in a temp file (`xkr-rpc`) | no panic; never a 5xx |

`storage_record` skips zstd frames that declare more than 16 MiB: `decompress` pre-allocates the
declared length once it passes the payload bound (at most 128 KiB per 3 payload bytes), so
large declarations only slow the run down.

## Setup (once)

Nightly and cargo-fuzz, fetched through Socket Firewall like every crate:

```sh
rustup toolchain install nightly --profile minimal
sfw cargo install cargo-fuzz --locked --version 0.13.2
```

Seed corpus from the repository fixtures (mainnet blocks and transactions, bridge frames,
bare-rpc captures, plus synthesised epee/Levin/storage/RPC inputs):

```sh
fuzz/seeds.sh          # writes fuzz/corpus/<target>/
```

## Running (one target at a time)

From the repository root. Build with at most 3 jobs at the lowest priority and keep the run under
a memory cap (for example `systemd-run --user --scope -p MemoryMax=4G`). libFuzzer's own limits keep a single input
below 2 GB.

```sh
# build all targets once
CARGO_BUILD_JOBS=3 nice -n 19 cargo +nightly fuzz build --fuzz-dir fuzz

# 60 s per target
for t in cn_block cn_transaction epee levin_header hyper_frame hugin_packet storage_record rpc_json; do
  CARGO_BUILD_JOBS=3 nice -n 19 cargo +nightly fuzz run --fuzz-dir fuzz "$t" -- \
      -max_total_time=60 -rss_limit_mb=2048 -malloc_limit_mb=2048 -timeout=10
done
```

Longer runs: raise `-max_total_time`; keep one target per run (each libFuzzer process plus
AddressSanitizer needs ~1 GB). A crash leaves the input in `fuzz/artifacts/<target>/`;
reproduce with

```sh
cargo +nightly fuzz run --fuzz-dir fuzz <target> fuzz/artifacts/<target>/<file>
```

and turn it into a regular test (`crates/*/tests/prop_*.rs` or a fixture under
`crates/*/tests/fixtures/malformed/`) before fixing it.

## Property tests

The same properties with bounded case counts run in the normal test suite
(`crates/*/tests/prop_*.rs`); `PROPTEST_CASES=10000` runs them longer.
