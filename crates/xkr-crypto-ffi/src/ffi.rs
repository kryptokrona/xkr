//! The C ABI (`include/xkr_crypto_ffi.h`) for the React Native C++ `TurboModule` and the host
//! differential harness. Every symbol is prefixed `xkr_crypto_`.
//!
//! Conventions (the header repeats them):
//!
//! * Every function returns an `int32_t` status: `XKR_CRYPTO_OK` (0) or a negative
//!   `XKR_CRYPTO_ERR_*` code ([`crate::error::status`]); `xkr_crypto_status_message` names it.
//!   Outputs are only written on success.
//! * Strings are passed as [`XkrCryptoStr`] (pointer + length, UTF-8, no terminator needed);
//!   arrays of strings as a pointer to `XkrCryptoStr` + count.
//! * Fixed-size results (keys, hashes, scalars, signatures) are written as lowercase hex plus a
//!   terminating NUL into a caller buffer with its capacity (`XKR_CRYPTO_HEX32_SIZE` = 65,
//!   `XKR_CRYPTO_HEX64_SIZE` = 129 bytes).
//! * Variable-size results are returned in an [`XkrCryptoBuffer`] the library allocates; the
//!   caller copies it and releases it with `xkr_crypto_buffer_free` (exactly once).
//! * Booleans are `uint8_t` (0 or 1).
//! * JavaScript numbers (indexes) are passed as `double` and checked here: finite, integral,
//!   `0 <= n <= 2^53 - 1`, otherwise `XKR_CRYPTO_ERR_INVALID_INDEX`.
//! * Functions are reentrant and thread-safe; heavy ones (`cn_turtle_lite_slow_hash_v2`,
//!   `find_pow_share`, `process_block_outputs`, ring and CLSAG signing) must be called off the
//!   JavaScript thread.
//! * A Rust panic never crosses the boundary: it is caught and reported as
//!   `XKR_CRYPTO_ERR_INTERNAL` (with `panic = "abort"` builds the process aborts instead).
//!
//! # Safety
//!
//! For every function: each `XkrCryptoStr` must point to `len` readable bytes (or have
//! `len == 0`); each array pointer must point to `count` readable `XkrCryptoStr`s (or have
//! `count == 0`); each output pointer must be valid for writes of the stated size; buffers must
//! not overlap the inputs.
#![allow(unsafe_code)] // the C ABI boundary; the rest of the crate has no unsafe code
#![allow(clippy::must_use_candidate, clippy::too_many_arguments)]

use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::error::{CryptoError, CryptoResult, status};
use crate::{api, pow, scan};

/// Version of this ABI; bumped on any incompatible change of a signature or struct.
pub const ABI_VERSION: u32 = 1;

/// A borrowed UTF-8 string: `ptr` to `len` bytes, no terminator required.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XkrCryptoStr {
    /// First byte (may be null when `len == 0`).
    pub ptr: *const c_char,
    /// Number of bytes.
    pub len: usize,
}

/// A byte buffer allocated by this library; release it with [`xkr_crypto_buffer_free`].
#[repr(C)]
#[derive(Debug)]
pub struct XkrCryptoBuffer {
    /// First byte (never null after a successful call, also when `len == 0`).
    pub ptr: *mut u8,
    /// Number of bytes.
    pub len: usize,
}

/// Result of [`xkr_crypto_find_pow_share`].
#[repr(C)]
#[derive(Debug)]
pub struct XkrCryptoPowResult {
    /// 1 if a share was found (`nonce_hex` and `result_hex` are then set), else 0.
    pub found: u8,
    /// The nonce bytes as hex, NUL-terminated.
    pub nonce_hex: [u8; 9],
    /// The share hash as hex, NUL-terminated.
    pub result_hex: [u8; 65],
    /// Hashes computed by all workers.
    pub hashes_performed: u64,
    /// Number of workers used.
    pub threads: u32,
}

// ---------------------------------------------------------------------------------------------
// marshalling helpers

type Status = i32;

/// Runs `f`, turning an error into its status and a panic into `INTERNAL`.
fn guard(f: impl FnOnce() -> Result<(), Status>) -> Status {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => status::OK,
        Ok(Err(code)) => code,
        Err(_) => status::INTERNAL,
    }
}

fn err(e: &CryptoError) -> Status {
    e.status()
}

/// # Safety
/// `s` must satisfy the module contract.
unsafe fn read_str<'a>(s: XkrCryptoStr) -> Result<&'a str, Status> {
    if s.len == 0 {
        return Ok("");
    }
    if s.ptr.is_null() {
        return Err(status::NULL_POINTER);
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`.
    let bytes = unsafe { std::slice::from_raw_parts(s.ptr.cast::<u8>(), s.len) };
    std::str::from_utf8(bytes).map_err(|_| status::INVALID_HEX)
}

/// # Safety
/// `items` must satisfy the module contract.
unsafe fn read_strs<'a>(items: *const XkrCryptoStr, count: usize) -> Result<Vec<&'a str>, Status> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if items.is_null() {
        return Err(status::NULL_POINTER);
    }
    // SAFETY: the caller guarantees `count` readable elements at `items`.
    let slice = unsafe { std::slice::from_raw_parts(items, count) };
    slice
        .iter()
        // SAFETY: every element satisfies the contract.
        .map(|s| unsafe { read_str(*s) })
        .collect()
}

/// A JavaScript number used as an index or count.
fn js_index(value: f64) -> Result<u64, Status> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let integral = value.fract().abs().to_bits() == 0;
    if value.is_finite() && (0.0..=MAX_SAFE).contains(&value) && integral {
        Ok(value as u64)
    } else {
        Err(status::INVALID_INDEX)
    }
}

/// Writes `text` and a NUL into `out[..cap]`.
///
/// # Safety
/// `out` must be valid for `cap` bytes of writes.
unsafe fn write_text(out: *mut c_char, cap: usize, text: &str) -> Result<(), Status> {
    if out.is_null() {
        return Err(status::NULL_POINTER);
    }
    if cap <= text.len() {
        return Err(status::BUFFER_TOO_SMALL);
    }
    // SAFETY: `out` has room for `text.len() + 1` bytes (checked above) and does not overlap
    // `text` (a Rust-owned string).
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), text.len());
        *out.add(text.len()) = 0;
    }
    Ok(())
}

/// Checks that `out` can take `len` characters and a NUL, so a function with two outputs can
/// verify both before writing either.
fn room(out: *mut c_char, cap: usize, len: usize) -> Result<(), Status> {
    if out.is_null() {
        Err(status::NULL_POINTER)
    } else if cap <= len {
        Err(status::BUFFER_TOO_SMALL)
    } else {
        Ok(())
    }
}

/// # Safety
/// `out` must be valid for one write.
unsafe fn write_bool(out: *mut u8, value: bool) -> Result<(), Status> {
    if out.is_null() {
        return Err(status::NULL_POINTER);
    }
    // SAFETY: checked non-null, valid per the contract.
    unsafe { *out = u8::from(value) };
    Ok(())
}

/// Hands `bytes` to the caller.
///
/// # Safety
/// `out` must be valid for one write.
unsafe fn write_buffer(out: *mut XkrCryptoBuffer, bytes: Vec<u8>) -> Result<(), Status> {
    if out.is_null() {
        return Err(status::NULL_POINTER);
    }
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed).cast::<u8>();
    // SAFETY: checked non-null, valid per the contract.
    unsafe { *out = XkrCryptoBuffer { ptr, len } };
    Ok(())
}

fn hex_result(r: CryptoResult<String>) -> Result<String, Status> {
    r.map_err(|e| err(&e))
}

// ---------------------------------------------------------------------------------------------
// library

/// The ABI version ([`ABI_VERSION`]); the `TurboModule` refuses to start on a mismatch.
#[unsafe(no_mangle)]
pub extern "C" fn xkr_crypto_abi_version() -> u32 {
    ABI_VERSION
}

/// A static NUL-terminated English message for a status code (never null).
#[unsafe(no_mangle)]
pub extern "C" fn xkr_crypto_status_message(code: i32) -> *const c_char {
    let text: &'static std::ffi::CStr = match code {
        status::OK => c"ok",
        status::NULL_POINTER => c"null pointer argument",
        status::INVALID_HEX => c"argument is not a hexadecimal string",
        status::INVALID_LENGTH => c"argument has the wrong length",
        status::INVALID_PUBLIC_KEY => c"not a valid public key",
        status::INVALID_SCALAR => c"not a valid private key or scalar",
        status::INVALID_INDEX => c"invalid index",
        status::BUFFER_TOO_SMALL => c"output buffer too small",
        status::INVALID_JSON => c"invalid JSON argument",
        status::SIGNING_FAILED => c"could not sign with the given keys",
        status::INVALID_ARGUMENT => c"invalid argument",
        status::DATA_CAN_NOT_BE_HASHED => c"data can not be hashed with this variant",
        status::INTERNAL => c"internal error",
        _ => c"unknown status",
    };
    text.as_ptr()
}

/// Releases a buffer returned by this library. A buffer with a null `ptr` is ignored.
///
/// # Safety
/// `buffer` must come from this library and must not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_buffer_free(buffer: XkrCryptoBuffer) {
    if buffer.ptr.is_null() {
        return;
    }
    // SAFETY: `ptr`/`len` came from `Box<[u8]>::into_raw` in `write_buffer`.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buffer.ptr, buffer.len)) });
}

// ---------------------------------------------------------------------------------------------
// keys

/// `checkKey`: writes 1 if `public_key` is a valid point, else 0 (also for malformed hex).
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_check_key(public_key: XkrCryptoStr, out_valid: *mut u8) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let key = unsafe { read_str(public_key) }?;
        unsafe { write_bool(out_valid, api::check_key(key)) }
    })
}

/// `generateKeys`: a random key pair from the OS RNG.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_keys(
    out_public_key: *mut c_char,
    public_cap: usize,
    out_private_key: *mut c_char,
    private_cap: usize,
) -> i32 {
    guard(|| {
        room(out_public_key, public_cap, 64)?;
        room(out_private_key, private_cap, 64)?;
        let keys = api::generate_keys();
        // SAFETY: per the contract.
        unsafe {
            write_text(out_public_key, public_cap, &keys.public_key)?;
            write_text(out_private_key, private_cap, &keys.private_key)
        }
    })
}

/// `secretKeyToPublicKey`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_secret_key_to_public_key(
    private_key: XkrCryptoStr,
    out_public_key: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let key = unsafe { read_str(private_key) }?;
        let public = hex_result(api::secret_key_to_public_key(key))?;
        unsafe { write_text(out_public_key, out_cap, &public) }
    })
}

/// `scReduce32`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_sc_reduce32(
    scalar: XkrCryptoStr,
    out_scalar: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let s = unsafe { read_str(scalar) }?;
        let reduced = hex_result(api::sc_reduce32(s))?;
        unsafe { write_text(out_scalar, out_cap, &reduced) }
    })
}

/// `generateDeterministicSubwalletKeys`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_deterministic_subwallet_keys(
    base_private_key: XkrCryptoStr,
    wallet_index: f64,
    out_public_key: *mut c_char,
    public_cap: usize,
    out_private_key: *mut c_char,
    private_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let base = unsafe { read_str(base_private_key) }?;
        let index = js_index(wallet_index)?;
        room(out_public_key, public_cap, 64)?;
        room(out_private_key, private_cap, 64)?;
        let keys = api::generate_deterministic_subwallet_keys(base, index).map_err(|e| err(&e))?;
        unsafe {
            write_text(out_public_key, public_cap, &keys.public_key)?;
            write_text(out_private_key, private_cap, &keys.private_key)
        }
    })
}

// ---------------------------------------------------------------------------------------------
// derivations

/// `generateKeyDerivation`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_key_derivation(
    public_key: XkrCryptoStr,
    private_key: XkrCryptoStr,
    out_derivation: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (public, secret) = unsafe { (read_str(public_key)?, read_str(private_key)?) };
        let d = hex_result(api::generate_key_derivation(public, secret))?;
        unsafe { write_text(out_derivation, out_cap, &d) }
    })
}

/// `derivePublicKey`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_derive_public_key(
    derivation: XkrCryptoStr,
    output_index: f64,
    public_key: XkrCryptoStr,
    out_public_key: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (d, public) = unsafe { (read_str(derivation)?, read_str(public_key)?) };
        let index = js_index(output_index)?;
        let derived = hex_result(api::derive_public_key(d, index, public))?;
        unsafe { write_text(out_public_key, out_cap, &derived) }
    })
}

/// `deriveSecretKey`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_derive_secret_key(
    derivation: XkrCryptoStr,
    output_index: f64,
    private_key: XkrCryptoStr,
    out_private_key: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (d, secret) = unsafe { (read_str(derivation)?, read_str(private_key)?) };
        let index = js_index(output_index)?;
        let derived = hex_result(api::derive_secret_key(d, index, secret))?;
        unsafe { write_text(out_private_key, out_cap, &derived) }
    })
}

/// `underivePublicKey`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_underive_public_key(
    derivation: XkrCryptoStr,
    output_index: f64,
    derived_key: XkrCryptoStr,
    out_public_key: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (d, derived) = unsafe { (read_str(derivation)?, read_str(derived_key)?) };
        let index = js_index(output_index)?;
        let base = hex_result(api::underive_public_key(d, index, derived))?;
        unsafe { write_text(out_public_key, out_cap, &base) }
    })
}

/// `generateKeyImage`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_key_image(
    public_key: XkrCryptoStr,
    private_key: XkrCryptoStr,
    out_key_image: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (public, secret) = unsafe { (read_str(public_key)?, read_str(private_key)?) };
        let image = hex_result(api::generate_key_image(public, secret))?;
        unsafe { write_text(out_key_image, out_cap, &image) }
    })
}

/// `deriveViewTag` (transaction version 2): two hex characters + NUL (`out_cap >= 3`).
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_derive_view_tag(
    derivation: XkrCryptoStr,
    output_index: f64,
    out_view_tag: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let d = unsafe { read_str(derivation) }?;
        let index = js_index(output_index)?;
        let tag = hex_result(api::derive_view_tag(d, index))?;
        unsafe { write_text(out_view_tag, out_cap, &tag) }
    })
}

// ---------------------------------------------------------------------------------------------
// hashing

/// `cnFastHash`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_cn_fast_hash(
    data: XkrCryptoStr,
    out_hash: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let d = unsafe { read_str(data) }?;
        let h = hex_result(api::cn_fast_hash(d))?;
        unsafe { write_text(out_hash, out_cap, &h) }
    })
}

/// `cnTurtleLiteSlowHashV2` (heavy: call off the JavaScript thread).
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_cn_turtle_lite_slow_hash_v2(
    data: XkrCryptoStr,
    out_hash: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let d = unsafe { read_str(data) }?;
        let h = hex_result(api::cn_turtle_lite_slow_hash_v2(d))?;
        unsafe { write_text(out_hash, out_cap, &h) }
    })
}

/// `hashToEllipticCurve`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_hash_to_elliptic_curve(
    hash: XkrCryptoStr,
    out_point: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let h = unsafe { read_str(hash) }?;
        let p = hex_result(api::hash_to_elliptic_curve(h))?;
        unsafe { write_text(out_point, out_cap, &p) }
    })
}

/// `hashToScalar`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_hash_to_scalar(
    data: XkrCryptoStr,
    out_scalar: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let d = unsafe { read_str(data) }?;
        let s = hex_result(api::hash_to_scalar(d))?;
        unsafe { write_text(out_scalar, out_cap, &s) }
    })
}

// ---------------------------------------------------------------------------------------------
// signatures

/// `generateSignature`: 128 hex characters + NUL (`out_cap >= 129`).
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_signature(
    prefix_hash: XkrCryptoStr,
    public_key: XkrCryptoStr,
    private_key: XkrCryptoStr,
    out_signature: *mut c_char,
    out_cap: usize,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, public, secret) = unsafe {
            (
                read_str(prefix_hash)?,
                read_str(public_key)?,
                read_str(private_key)?,
            )
        };
        let sig = hex_result(api::generate_signature(h, public, secret))?;
        unsafe { write_text(out_signature, out_cap, &sig) }
    })
}

/// `checkSignature`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_check_signature(
    prefix_hash: XkrCryptoStr,
    public_key: XkrCryptoStr,
    signature: XkrCryptoStr,
    out_valid: *mut u8,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, public, sig) = unsafe {
            (
                read_str(prefix_hash)?,
                read_str(public_key)?,
                read_str(signature)?,
            )
        };
        unsafe { write_bool(out_valid, api::check_signature(h, public, sig)) }
    })
}

/// `generateRingSignatures`: the signatures concatenated (128 hex characters each, no
/// separator, no NUL) in `out_signatures`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_generate_ring_signatures(
    prefix_hash: XkrCryptoStr,
    key_image: XkrCryptoStr,
    public_keys: *const XkrCryptoStr,
    public_keys_count: usize,
    private_key: XkrCryptoStr,
    real_index: f64,
    out_signatures: *mut XkrCryptoBuffer,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, image, keys, secret) = unsafe {
            (
                read_str(prefix_hash)?,
                read_str(key_image)?,
                read_strs(public_keys, public_keys_count)?,
                read_str(private_key)?,
            )
        };
        let index = js_index(real_index)?;
        let sigs =
            api::generate_ring_signatures(h, image, &keys, secret, index).map_err(|e| err(&e))?;
        unsafe { write_buffer(out_signatures, sigs.concat().into_bytes()) }
    })
}

/// `checkRingSignature`.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_check_ring_signature(
    prefix_hash: XkrCryptoStr,
    key_image: XkrCryptoStr,
    public_keys: *const XkrCryptoStr,
    public_keys_count: usize,
    signatures: *const XkrCryptoStr,
    signatures_count: usize,
    out_valid: *mut u8,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, image, keys, sigs) = unsafe {
            (
                read_str(prefix_hash)?,
                read_str(key_image)?,
                read_strs(public_keys, public_keys_count)?,
                read_strs(signatures, signatures_count)?,
            )
        };
        unsafe { write_bool(out_valid, api::check_ring_signature(h, image, &keys, &sigs)) }
    })
}

/// `clsagSign` (transaction version 2): `c1` into `out_c1` (65 bytes), the responses
/// concatenated (64 hex characters each, no NUL) in `out_s`. An empty `alpha` with no decoy
/// responses draws the randomness from the OS.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_clsag_sign(
    prefix_hash: XkrCryptoStr,
    ring: *const XkrCryptoStr,
    ring_count: usize,
    private_ephemeral: XkrCryptoStr,
    real_output_index: f64,
    alpha: XkrCryptoStr,
    decoy_responses: *const XkrCryptoStr,
    decoy_responses_count: usize,
    out_c1: *mut c_char,
    c1_cap: usize,
    out_s: *mut XkrCryptoBuffer,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, keys, secret, alpha, decoys) = unsafe {
            (
                read_str(prefix_hash)?,
                read_strs(ring, ring_count)?,
                read_str(private_ephemeral)?,
                read_str(alpha)?,
                read_strs(decoy_responses, decoy_responses_count)?,
            )
        };
        let index = js_index(real_output_index)?;
        let sig = api::clsag_sign(h, &keys, secret, index, alpha, &decoys).map_err(|e| err(&e))?;
        // Check both outputs before writing either.
        room(out_c1, c1_cap, 64)?;
        if out_s.is_null() {
            return Err(status::NULL_POINTER);
        }
        unsafe {
            write_text(out_c1, c1_cap, &sig.c1)?;
            write_buffer(out_s, sig.s.concat().into_bytes())
        }
    })
}

/// `clsagVerify` (transaction version 2).
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_clsag_verify(
    prefix_hash: XkrCryptoStr,
    ring: *const XkrCryptoStr,
    ring_count: usize,
    key_image: XkrCryptoStr,
    c1: XkrCryptoStr,
    s: *const XkrCryptoStr,
    s_count: usize,
    out_valid: *mut u8,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (h, keys, image, c1, s) = unsafe {
            (
                read_str(prefix_hash)?,
                read_strs(ring, ring_count)?,
                read_str(key_image)?,
                read_str(c1)?,
                read_strs(s, s_count)?,
            )
        };
        unsafe { write_bool(out_valid, api::clsag_verify(h, &keys, image, c1, &s)) }
    })
}

// ---------------------------------------------------------------------------------------------
// wallet scan and PoW

/// `processBlockOutputs`: `block_json` is `JSON.stringify(block)`, `spend_keys_json` is
/// `JSON.stringify([{ publicKey, privateKey }])`; the result in `out_json` is the JSON array
/// of `{ publicSpendKey, input }` (see `scan.rs`). Heavy: call off the JavaScript thread.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_process_block_outputs(
    block_json: XkrCryptoStr,
    private_view_key: XkrCryptoStr,
    spend_keys_json: XkrCryptoStr,
    is_view_wallet: u8,
    process_coinbase_transactions: u8,
    out_json: *mut XkrCryptoBuffer,
) -> i32 {
    guard(|| {
        // SAFETY: per the contract.
        let (block, view_key, spend_keys) = unsafe {
            (
                read_str(block_json).map_err(|_| status::INVALID_JSON)?,
                read_str(private_view_key)?,
                read_str(spend_keys_json).map_err(|_| status::INVALID_JSON)?,
            )
        };
        let json = scan::process_block_outputs_json(
            block,
            view_key,
            spend_keys,
            is_view_wallet != 0,
            process_coinbase_transactions != 0,
        )
        .map_err(|e| err(&e))?;
        unsafe { write_buffer(out_json, json.into_bytes()) }
    })
}

/// `findPowShare`. Returns `XKR_CRYPTO_ERR_INVALID_ARGUMENT` where the JavaScript method
/// resolves `null` (unusable blob or target); otherwise `XKR_CRYPTO_OK` with `out->found`
/// telling whether a share was found. `start_nonce` and `max_attempts` are the JavaScript
/// numbers as given; `threads == 0` picks 80 % of the cores (1..=8). Heavy: call off the
/// JavaScript thread.
///
/// # Safety
/// See the module documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn xkr_crypto_find_pow_share(
    blob_hex: XkrCryptoStr,
    target_hex: XkrCryptoStr,
    start_nonce: f64,
    max_attempts: f64,
    threads: u32,
    out: *mut XkrCryptoPowResult,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(status::NULL_POINTER);
        }
        // SAFETY: per the contract.
        let (blob, target) = unsafe { (read_str(blob_hex)?, read_str(target_hex)?) };
        let search = pow::find_pow_share(
            blob,
            target,
            pow::start_nonce_from_js(start_nonce),
            pow::max_attempts_from_js(max_attempts),
            threads,
        )
        .ok_or(status::INVALID_ARGUMENT)?;
        let mut result = XkrCryptoPowResult {
            found: 0,
            nonce_hex: [0; 9],
            result_hex: [0; 65],
            hashes_performed: search.hashes_performed,
            threads: search.threads,
        };
        if let Some(share) = &search.share {
            result.found = 1;
            result.nonce_hex[..8].copy_from_slice(share.nonce.as_bytes());
            result.result_hex[..64].copy_from_slice(share.result.as_bytes());
        }
        // SAFETY: checked non-null, valid per the contract.
        unsafe { out.write(result) };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str) -> XkrCryptoStr {
        XkrCryptoStr {
            ptr: text.as_ptr().cast(),
            len: text.len(),
        }
    }

    fn take(buffer: XkrCryptoBuffer) -> String {
        // SAFETY: the buffer came from this library.
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) }.to_vec();
        unsafe { xkr_crypto_buffer_free(buffer) };
        String::from_utf8(bytes).unwrap()
    }

    fn text(buf: &[c_char]) -> String {
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| c.to_ne_bytes()[0])
            .collect();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn js_index_rules() {
        assert_eq!(js_index(0.0), Ok(0));
        assert_eq!(js_index(7.0), Ok(7));
        assert_eq!(js_index(9_007_199_254_740_991.0), Ok(9_007_199_254_740_991));
        assert_eq!(
            js_index(9_007_199_254_740_992.0),
            Err(status::INVALID_INDEX)
        );
        assert_eq!(js_index(-1.0), Err(status::INVALID_INDEX));
        assert_eq!(js_index(1.5), Err(status::INVALID_INDEX));
        assert_eq!(js_index(f64::NAN), Err(status::INVALID_INDEX));
        assert_eq!(js_index(f64::INFINITY), Err(status::INVALID_INDEX));
    }

    #[test]
    fn fixed_outputs_need_room_for_the_terminator() {
        let data = "00";
        let mut out: [c_char; 65] = [0; 65];
        let mut small: [c_char; 64] = [0; 64];
        unsafe {
            assert_eq!(
                xkr_crypto_cn_fast_hash(s(data), small.as_mut_ptr(), small.len()),
                status::BUFFER_TOO_SMALL
            );
            assert_eq!(
                xkr_crypto_cn_fast_hash(s(data), out.as_mut_ptr(), out.len()),
                status::OK
            );
            assert_eq!(
                xkr_crypto_cn_fast_hash(s(data), std::ptr::null_mut(), 65),
                status::NULL_POINTER
            );
        }
        assert_eq!(text(&out), api::cn_fast_hash(data).unwrap());
    }

    #[test]
    fn null_strings_with_length_are_refused_and_empty_is_fine() {
        let mut out: [c_char; 65] = [0; 65];
        let null = XkrCryptoStr {
            ptr: std::ptr::null(),
            len: 4,
        };
        let empty = XkrCryptoStr {
            ptr: std::ptr::null(),
            len: 0,
        };
        unsafe {
            assert_eq!(
                xkr_crypto_cn_fast_hash(null, out.as_mut_ptr(), out.len()),
                status::NULL_POINTER
            );
            assert_eq!(
                xkr_crypto_cn_fast_hash(empty, out.as_mut_ptr(), out.len()),
                status::OK
            );
        }
        // Keccak-256 of the empty string
        assert_eq!(
            text(&out),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }

    #[test]
    fn ring_signatures_through_the_abi() {
        let hash = api::cn_fast_hash("aa").unwrap();
        let keys: Vec<api::KeyPair> = (0..3).map(|_| api::generate_keys()).collect();
        let pubs: Vec<XkrCryptoStr> = keys.iter().map(|k| s(&k.public_key)).collect();
        let image = api::generate_key_image(&keys[2].public_key, &keys[2].private_key).unwrap();
        let mut buffer = XkrCryptoBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        let code = unsafe {
            xkr_crypto_generate_ring_signatures(
                s(&hash),
                s(&image),
                pubs.as_ptr(),
                pubs.len(),
                s(&keys[2].private_key),
                2.0,
                &raw mut buffer,
            )
        };
        assert_eq!(code, status::OK);
        let joined = take(buffer);
        assert_eq!(joined.len(), 3 * 128);
        let sigs: Vec<&str> = (0..3).map(|i| &joined[i * 128..(i + 1) * 128]).collect();
        let sig_strs: Vec<XkrCryptoStr> = sigs.iter().copied().map(s).collect();
        let mut valid = 0u8;
        let code = unsafe {
            xkr_crypto_check_ring_signature(
                s(&hash),
                s(&image),
                pubs.as_ptr(),
                pubs.len(),
                sig_strs.as_ptr(),
                sig_strs.len(),
                &raw mut valid,
            )
        };
        assert_eq!((code, valid), (status::OK, 1));
        // a fractional index is refused before any work
        let mut unused = XkrCryptoBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        let code = unsafe {
            xkr_crypto_generate_ring_signatures(
                s(&hash),
                s(&image),
                pubs.as_ptr(),
                pubs.len(),
                s(&keys[2].private_key),
                2.5,
                &raw mut unused,
            )
        };
        assert_eq!(code, status::INVALID_INDEX);
        assert!(unused.ptr.is_null());
    }

    #[test]
    fn empty_buffer_round_trips() {
        let mut buffer = XkrCryptoBuffer {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        unsafe { write_buffer(&raw mut buffer, Vec::new()) }.unwrap();
        assert!(!buffer.ptr.is_null());
        assert_eq!(take(buffer), "");
        unsafe {
            xkr_crypto_buffer_free(XkrCryptoBuffer {
                ptr: std::ptr::null_mut(),
                len: 0,
            });
        }
    }

    #[test]
    fn status_messages_exist() {
        for code in [0, -1, -2, -3, -4, -5, -6, -7, -8, -9, -10, -11, -100, 42] {
            let msg = unsafe { std::ffi::CStr::from_ptr(xkr_crypto_status_message(code)) };
            assert_ne!(msg.to_bytes().len(), 0);
        }
        assert_eq!(xkr_crypto_abi_version(), ABI_VERSION);
    }
}
