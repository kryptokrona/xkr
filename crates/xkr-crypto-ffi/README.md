# xkr-crypto-ffi

The crypto primitives of the Kryptokrona JavaScript stack (kryptokrona-crypto's `Crypto` API
and the Hugin mobile app's native module) on top of `xkr-crypto` and `xkr-wallet-core`, with a
C ABI for C++ callers (the React Native TurboModule). The crate only parses and formats; the
work is done by the node's own crates.

| path | what |
|---|---|
| `src/api.rs` | the primitives, hex strings in and out |
| `src/scan.rs` | `processBlockOutputs` (typed and JSON) |
| `src/pow.rs` | `findPowShare` |
| `src/error.rs` | `CryptoError` and the C status codes |
| `src/ffi.rs` | the C ABI (`xkr_crypto_*`) |
| `include/xkr_crypto_ffi.h` | C header (`cbindgen.toml` regenerates an equivalent one) |
| `tests/` | known answers, scan, PoW shares, compat dump |
| `compat/` | differential harness against the C++ implementations |

## Build

```
cargo build --release -p xkr-crypto-ffi        # target/release/libxkr_crypto_ffi.a
cargo test -p xkr-crypto-ffi
```

Both build types are produced: `staticlib` for apps and the harness, `rlib` for the Node-API
addon (which calls `api`, `scan` and `pow` directly) and the tests.

C ABI conventions: every function returns `int32_t` (`XKR_CRYPTO_OK` = 0 or a negative
`XKR_CRYPTO_ERR_*`, text from `xkr_crypto_status_message()`); outputs are written only on
success; strings are `XkrCryptoStr { ptr, len }`; fixed-size results are hex plus NUL in a
caller buffer, variable-size results come in an `XkrCryptoBuffer` released with
`xkr_crypto_buffer_free()`; `xkr_crypto_abi_version()` must equal `XKR_CRYPTO_ABI_VERSION`.

## Compatibility checks

Against the two C++ copies in hugin-native (host build, no device):

```
crates/xkr-crypto-ffi/compat/run.sh [seed] [count]    # HUGIN=<hugin-native checkout>
```

`run.sh` builds the Rust static library, then five drivers (iOS and Android C++ copies, each
with hardware-AES and portable slow hash, plus Rust), and compares known answers
(`compat/kat.txt`), seeded inputs byte for byte, and signatures across all backends. For the
ARM slow-hash files: `make rust CROSS=aarch64-linux-gnu-`, `make -j3 drivers
CROSS=aarch64-linux-gnu-`, then `RUN="qemu-aarch64 -L /usr/aarch64-linux-gnu" ./run.sh`.

Against kryptokrona-crypto's Node addon:

```
XKR_COMPAT_DUMP=/tmp/xkr-compat.json cargo test -p xkr-crypto-ffi --test compat_dump
node crates/xkr-crypto-ffi/compat/compare.js /tmp/xkr-compat.json <kryptokrona-crypto checkout>
```
