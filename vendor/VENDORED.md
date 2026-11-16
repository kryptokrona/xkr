# Vendored code

| Directory | Crate name here | Upstream | Upstream path | License |
|---|---|---|---|---|
| `vendor/levin` | `xkr-levin` | Cuprate `cuprate-levin` 0.1.0 | `net/levin` | MIT (`vendor/levin/LICENSE-MIT`) |
| `vendor/epee-encoding` | `xkr-epee` | Cuprate `cuprate-epee-encoding` 0.5.0 | `net/epee-encoding` | MIT (`vendor/epee-encoding/LICENSE-MIT`) |

Upstream repository: https://github.com/Cuprate/cuprate
Upstream git rev: `a7b28239c0b8aac1752fbe74c913002c9ce29958` (2026-11-11, "Caching: only save cache if we're on main (#738)").
Copied from the local checkout at `~/projects/xkr-reference/cuprate`.

**License.** Cuprate licenses each crate under either MIT or AGPL-3.0 (declared in the crate's
`Cargo.toml`, see upstream `LICENSE`). Both crates vendored here are MIT upstream
(`license = "MIT"` in `net/levin/Cargo.toml` and `net/epee-encoding/Cargo.toml` at the revision
above), and no AGPL-3.0 Cuprate code is vendored. The MIT notice is kept in each crate's
`LICENSE-MIT` and `license = "MIT"` in its `Cargo.toml`; MIT code may be combined with the rest of
the workspace under GPL-3.0-or-later. The same holds for the CryptoNight code vendored from
Cuprate's `cryptonight` crate (MIT) in `crates/xkr-crypto/src/cryptonight/`
(`crates/xkr-crypto/VENDORED.md`).

Both crates keep `edition = "2021"` (the upstream edition) so the code stays close to
Cuprate and can be re-synced with a plain diff. The rest of the workspace is edition 2024.
They also do not opt into `[lints] workspace = true`, so the pedantic workspace lints do
not fire on upstream code.

## Local changes

### `xkr-levin`
- Package renamed `cuprate-levin` -> `xkr-levin`; all dependencies come from
  `[workspace.dependencies]` (exact versions).
- `cuprate-helper` removed. The two casts it provided (`usize_to_u64`,
  `u64_to_usize`) are inlined in `src/lib.rs` with the same semantics
  (`u64_to_usize` panics on 32-bit targets when the value does not fit).
- Added `LevinMessageCodec::new(Protocol)` (upstream only has `Default`, i.e. Monero
  limits) so callers can set `max_packet_size` / signature.
- `rand` dev-dependency removed: `tests/fragmented_message.rs` fills its buffer with a
  deterministic xorshift instead of `rand::thread_rng()`.
- The `arbitrary` optional feature and its `cfg_attr(feature = "arbitrary", ..)`
  derives were removed (fuzzing support we do not use).
- `unused_crate_dependencies` dropped from the `#![expect]` in
  `tests/fragmented_message.rs`: upstream enables that lint workspace-wide, here it is
  allow-by-default, so the expectation was unfulfilled.
- `LevinBucketCodec::decode` reserves at most `MAX_EAGER_RESERVE` (64 KiB) for a body that
  has not arrived yet instead of the whole declared size (up to `max_packet_size`, 100 MB):
  a bare 33-byte header could otherwise pin 100 MB per connection.
- Per-connection read limit:
  `LevinBucketCodec::set_read_limit(Arc<AtomicU64>)` / `LevinMessageCodec::with_read_limit`;
  a header declaring a larger body fails with the new `BucketError::BucketExceededReadLimit`
  (reassembled fragments are bounded by it too).
- `LevinMessageCodec` lifts the pre-handshake limit only after a handshake message has
  *decoded*; the bare `LevinBucketCodec` keeps the upstream rule (lift on a handshake
  header), so its tests are unchanged. `LevinMessageCodec::default()` is now
  `new(Protocol::default())`.
- The inner header of a reassembled fragment is checked like an outer one (signature,
  `max_packet_size`, read limit).

### `xkr-epee`
- Package renamed `cuprate-epee-encoding` -> `xkr-epee`; every
  `cuprate_epee_encoding::` path (macro, docs, tests) is now `xkr_epee::`.
- `cuprate-fixed-bytes` removed: `ByteArray<N>`, `ByteArrayVec<N>` and `FixedByteError`
  are inlined as `src/fixed_bytes.rs` (serde support dropped; added
  `ByteArrayVec::{as_bytes, iter}`), re-exported from the crate root.
- `cuprate-hex` removed: the `EpeeValue` impls for `Hex<N>`, `Vec<Hex<N>>` and
  `HexVec` were deleted (they only exist for Monero's JSON-RPC types).
- The `#![expect(unused_crate_dependencies)]` header was removed from every file in
  `tests/` (same reason as for `xkr-levin`).
- Typed objects nested inside each other are capped at 32 (`MAX_DEPTH_OF_TYPED_OBJECTS`, a
  thread-local counter with the `std` feature): the skip
  budget restarts at every typed level, so a future self-referential wire type could otherwise
  recurse as deep as the input allows.

## Protocol compatibility with Kryptokrona (TurtleCoin-derived C++ node)

Verified against `~/projects/xkr-reference/kryptokrona` (`src/p2p/levin_protocol.{h,cpp}`,
`src/serialization/kv_binary_*`):

- **Levin header** (`bucket_head2`, `#pragma pack(1)`, 33 bytes):
  `u64 signature = 0x0101010101012101`, `u64 cb`, `bool have_to_return_data`,
  `u32 command`, `i32 return_code`, `u32 flags`, `u32 protocol_version = 1`, all
  little-endian. Identical to Monero and to `header.rs` here (`HEADER_SIZE = 33`).
- **Flags**: C++ only ever sends `LEVIN_PACKET_REQUEST = 1` (requests and
  notifications) or `LEVIN_PACKET_RESPONSE = 2`. It never fragments (no
  `START_FRAGMENT`/`END_FRAGMENT` 0x4/0x8) and never sends dummy buckets, so the
  vendored fragment support is simply unused. `sendMessage(cmd, buf, needResponse)` sets
  `have_to_return_data = needResponse`, `return_code = 0`; `sendReply` sets
  `have_to_return_data = false`, `flags = RESPONSE` and the handler's return code
  (P2P handlers return `1`; unknown request commands get
  `LevinError::ERROR_CONNECTION_HANDLER_NOT_DEFINED = -6`). That matches
  `MessageType::from_flags_and_have_to_return` exactly.
- **C++ reader** (`readCommand`) is more lenient than ours: it ignores
  `protocol_version` and `return_code`, treats `isNotify = !have_to_return_data` and
  `isResponse = flags & 2`. It rejects bodies > `LEVIN_DEFAULT_MAX_PACKET_SIZE`
  (100 000 000 bytes) - same as `MONERO_MAX_PACKET_SIZE`. C++ has no pre-handshake size
  limit; Cuprate's 256 KiB limit is harmless (a handshake response with the maximum
  ~250 peerlist entries is ~6 KiB).
- **Signature** stays configurable through `Protocol { signature, .. }`; XKR uses the
  Monero value, so `Protocol::default()` is correct for mainnet and testnet.
- **Portable storage**: `PORTABLE_STORAGE_SIGNATUREA = 0x01011101`,
  `SIGNATUREB = 0x01020101`, `FORMAT_VER = 1` -> header bytes
  `01 11 01 01 01 01 02 01 01`, identical to `HEADER` in `epee-encoding/src/lib.rs`.
  Type tags 1..12 and the array flag 0x80 match `kv_binary_common.h`; the size varint
  (`writeArraySize`: 2-bit length mark, 1/2/4/8 bytes, `<= 63`, `<= 16383`,
  `<= 1073741823`) matches `varint.rs`.
- **Empty values**: the C++ writer omits empty arrays (`beginArray` with size 0 never
  emits a preamble) and zero-length `binary()` blobs, which is what Cuprate's
  `should_write() == !is_empty()` does. Difference: C++ *does* write an empty
  `std::string` via `operator()(std::string&)`; Cuprate skips empty strings. No XKR P2P
  message carries a string that can legitimately be empty (`status = "OK"`,
  block/tx blobs), and the C++ reader leaves missing fields at their default, so this
  is wire-compatible.
