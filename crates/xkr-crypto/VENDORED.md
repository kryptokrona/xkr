# Vendored code

## `src/cryptonight/` — CryptoNight (from Cuprate)

* Upstream: <https://github.com/Cuprate/cuprate>, crate `cuprate-cryptonight` (`cryptonight/src`)
* Revision: `a7b28239c0b8aac1752fbe74c913002c9ce29958`
* License: MIT, Copyright (c) 2023-2024 Cuprate Contributors (`src/cryptonight/LICENSE-MIT`).
  Every vendored file starts with a header naming its upstream path and this file.

| File | Status |
|---|---|
| `blake256.rs`, `cnaes.rs`, `util.rs` | verbatim except `crate::` -> `crate::cryptonight::` (and `subarray_mut` marked test-only) |
| `hash_v2.rs` | scratchpad passed as `&mut [u128]` instead of `&mut [u128; 2 MiB / 16]`; CryptoNight-R branch and its two test vectors removed |
| `slow_hash.rs` | rewritten around upstream's loop: new `CnParams` (page size, scratchpad, iterations, light flag) replacing the fixed `MEMORY`/`ITER` constants, `Variant::R` and `hash_v4.rs` removed, Keccak-1600 / Keccak-f from `crate::keccak` instead of the `sha3`/`keccak` crates, variant 1 short input returns `Err` instead of panicking. Upstream tests kept (keccak, mul, permutation, finalisers, Monero v0/v1/v2 vectors) plus all 18 vectors of the Kryptokrona C++ `src/crypto_test/main.cpp` (= `docs/test-vectors/cryptonight.txt`). |
| `hash_v4.rs` (CryptoNight-R) | not vendored (XKR never used it), so `seq-macro` is not needed |
| `hasher.rs` | not vendored: `CnHasher`, the loop of `slow_hash.rs` over a reused scratchpad with the AES rounds behind a trait (table rounds from `cnaes.rs`, or `aes::hazmat` rounds). `slow_hash.rs` helpers and constants are `pub(super)` for it, and `CnParams` gained `rounds` and `scratchpad_blocks`. |

### Parametrisation (C++ `src/crypto/slow-hash.c`, x86-64 path, `src/crypto/hash.h`)

`TOTALBLOCKS = page_size / 16`; address mask `TOTALBLOCKS / (light ? 2 : 1) - 1`;
`init_rounds = scratchpad / 128` lines are filled and folded back; `iterations / 2` main-loop rounds.

| Preset | page | scratchpad | iterations | light | addressed | used by |
|---|---|---|---|---|---|---|
| `CN` | 2 MiB | 2 MiB | 2^20 | no | 2 MiB | major 1-3 (`cn_slow_hash_v0`) |
| `CN_LITE` | 2 MiB | 1 MiB | 2^19 | yes | 1 MiB | major 4 (`cn_lite_slow_hash_v1`) |
| `CN_TURTLE_LITE` | 256 KiB | 256 KiB | 2^17 | yes | **128 KiB** | major >= 5 (`cn_turtle_lite_slow_hash_v2`) |

The portable C fallback at the end of `slow-hash.c` ignores the light flag and indexes bytes with a
block index; it is not what x86-64 nodes run and is not mirrored.

### Dependencies

`groestl =0.10.1`, `jh =0.1.0`, `skein =0.1.1` (0.1.0, used upstream, is yanked; same API) and
`digest =0.10.7` for the finalisers. `sha3`, `keccak` and `seq-macro` from upstream are not used.
