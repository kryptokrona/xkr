//! CryptoNight, vendored from Cuprate's `cuprate-cryptonight` (MIT, see `LICENSE-MIT` and
//! `crates/xkr-crypto/VENDORED.md`) and parametrised for the CN / CN-Lite / CN-Turtle family
//! used by Kryptokrona (C++ `src/crypto/slow-hash.c`, `src/crypto/hash.h`).
//!
//! [`cn_slow_hash`] allocates a scratchpad per call and uses table AES (block verification);
//! [`CnHasher`] keeps its scratchpad and uses the CPU's AES instructions (mining, share checks).

mod blake256;
mod cnaes;
mod hash_v2;
mod hasher;
mod slow_hash;
mod util;

pub use hasher::{AesImpl, CnHasher, hardware_aes_available};
pub use slow_hash::{CnParams, DataCanNotBeHashed, Variant};

/// Generic entry point: C++ `cn_slow_hash(data, len, hash, light, variant, 0, page_size,
/// scratchpad, iterations)` with `params` selecting `light`/`page_size`/`scratchpad`/`iterations`.
pub fn cn_slow_hash(
    data: &[u8],
    variant: Variant,
    params: &CnParams,
) -> Result<[u8; 32], DataCanNotBeHashed> {
    slow_hash::cn_slow_hash(data, variant, params)
}
