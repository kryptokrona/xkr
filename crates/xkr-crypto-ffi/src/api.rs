//! The primitives of `NativeModules.TurtleCoin` and of the transaction-version-2 additions,
//! with the argument and result encoding of the JavaScript side: hex strings in, lowercase hex
//! strings out.
//!
//! Every function is a thin layer over `xkr-crypto` (the code the node runs); this module only
//! parses and formats. Hex input may be upper or lower case. A fixed-size value (key, hash,
//! derivation, scalar) must have exactly its number of hex characters; malformed input gives an
//! error (or `false` for the `check*` predicates).
//!
//! Randomness (key generation, signing) comes from the operating system through
//! [`xkr_crypto::os_rng`].

use xkr_crypto::{EdwardsPoint, Scalar};
use xkr_types::{
    ClsagSignature, EcScalar, Hash, KeyDerivation, KeyImage, PublicKey, SecretKey, Signature,
};

use crate::error::{CryptoError, CryptoResult};

// ---------------------------------------------------------------------------------------------
// hex helpers (shared with the other modules of the crate)

/// Parses exactly `N` bytes from `2 * N` hex characters.
pub(crate) fn parse_fixed<const N: usize>(what: &'static str, hex: &str) -> CryptoResult<[u8; N]> {
    if hex.len() != 2 * N {
        return Err(CryptoError::InvalidLength {
            what,
            expected: 2 * N,
            got: hex.len(),
        });
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(hex, &mut out).map_err(|_| CryptoError::InvalidHex(what))?;
    Ok(out)
}

/// Parses a hex string of any even length (the empty string is zero bytes).
pub(crate) fn parse_bytes(what: &'static str, hex: &str) -> CryptoResult<Vec<u8>> {
    hex::decode(hex).map_err(|_| CryptoError::InvalidHex(what))
}

pub(crate) fn parse_hash(what: &'static str, hex: &str) -> CryptoResult<Hash> {
    parse_fixed(what, hex).map(Hash)
}

pub(crate) fn parse_public(what: &'static str, hex: &str) -> CryptoResult<PublicKey> {
    parse_fixed(what, hex).map(PublicKey)
}

pub(crate) fn parse_secret(what: &'static str, hex: &str) -> CryptoResult<SecretKey> {
    parse_fixed(what, hex).map(SecretKey)
}

pub(crate) fn parse_derivation(what: &'static str, hex: &str) -> CryptoResult<KeyDerivation> {
    parse_fixed(what, hex).map(KeyDerivation)
}

pub(crate) fn parse_key_image(what: &'static str, hex: &str) -> CryptoResult<KeyImage> {
    parse_fixed(what, hex).map(KeyImage)
}

fn parse_signature(what: &'static str, hex: &str) -> CryptoResult<Signature> {
    parse_fixed(what, hex).map(Signature)
}

fn parse_ec_scalar(what: &'static str, hex: &str) -> CryptoResult<EcScalar> {
    parse_fixed(what, hex).map(EcScalar)
}

/// A canonical scalar (`< l`), as C++ `sc_check` accepts it.
fn parse_canonical(what: &'static str, hex: &str) -> CryptoResult<Scalar> {
    let bytes = parse_fixed::<32>(what, hex)?;
    Option::<Scalar>::from(Scalar::from_canonical_bytes(bytes))
        .ok_or(CryptoError::InvalidScalar(what))
}

fn parse_list<T, S: AsRef<str>>(
    what: &'static str,
    items: &[S],
    parse: fn(&'static str, &str) -> CryptoResult<T>,
) -> CryptoResult<Vec<T>> {
    items.iter().map(|s| parse(what, s.as_ref())).collect()
}

/// A JavaScript array index (`usize`) from the `u64` the bindings pass.
fn to_index(what: &'static str, index: u64) -> CryptoResult<usize> {
    usize::try_from(index).map_err(|_| CryptoError::InvalidIndex(what))
}

// ---------------------------------------------------------------------------------------------
// result types

/// A key pair as the JavaScript side receives it (`{ public_key, private_key }`, snake case as
/// in `IKeyPair` of kryptokrona-crypto).
#[derive(Clone, PartialEq, Eq)]
pub struct KeyPair {
    /// Public key, 64 lowercase hex characters.
    pub public_key: String,
    /// Private key, 64 lowercase hex characters.
    pub private_key: String,
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPair")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

/// A single-key CLSAG signature (`IClsagSignature`: `{ c1, s }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clsag {
    /// The challenge `c_0`, 64 hex characters.
    pub c1: String,
    /// The responses `s_0 .. s_{n-1}` in ring order, 64 hex characters each.
    pub s: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// keys

/// `checkKey(publicKey)`: whether the 32 bytes decode to a curve point (C++ `check_key`).
/// Malformed hex is not a key: `false`.
#[must_use]
pub fn check_key(public_key: &str) -> bool {
    parse_public("publicKey", public_key).is_ok_and(|k| xkr_crypto::check_key(&k))
}

/// `generateKeys()`: a new random key pair (scalar from 64 random bytes reduced mod `l`).
#[must_use]
pub fn generate_keys() -> KeyPair {
    let (public, secret) = xkr_crypto::generate_keys(&mut xkr_crypto::os_rng());
    KeyPair {
        public_key: public.to_hex(),
        private_key: secret.to_hex(),
    }
}

/// `secretKeyToPublicKey(privateKey)`: `x * G`; refuses a non-canonical scalar (C++
/// `secret_key_to_public_key` returns `false` for it).
pub fn secret_key_to_public_key(private_key: &str) -> CryptoResult<String> {
    let secret = parse_secret("privateKey", private_key)?;
    xkr_crypto::secret_key_to_public_key(&secret)
        .map(|p| p.to_hex())
        .ok_or(CryptoError::InvalidScalar("privateKey"))
}

/// `scReduce32(scalar)`: the 32-byte little-endian value reduced mod `l`.
pub fn sc_reduce32(scalar: &str) -> CryptoResult<String> {
    let bytes = parse_fixed::<32>("scalar", scalar)?;
    Ok(hex::encode(Scalar::from_bytes_mod_order(bytes).to_bytes()))
}

/// `generateDeterministicSubwalletKeys(basePrivateKey, walletIndex)`: C++
/// `generate_deterministic_subwallet_key` followed by `secret_key_to_public_key`.
///
/// `tmp = base || le64(index)`; `index` times `tmp[0..32] = cn_fast_hash(tmp)`; the private key
/// is `sc_reduce32(tmp[0..32])`. Index 0 returns the reduced base key. The work is linear in
/// the index, as in C++ (`xkr_wallet_core::deterministic_subwallet_keys`).
pub fn generate_deterministic_subwallet_keys(
    base_private_key: &str,
    wallet_index: u64,
) -> CryptoResult<KeyPair> {
    let base = parse_secret("basePrivateKey", base_private_key)?;
    let (secret, public) = xkr_wallet_core::deterministic_subwallet_keys(&base, wallet_index);
    Ok(KeyPair {
        public_key: public.to_hex(),
        private_key: secret.to_hex(),
    })
}

/// `checkScalar(privateKey)` of kryptokrona-crypto (`scReduce32(x) == x`): `x < l`.
#[must_use]
pub fn check_scalar(private_key: &str) -> bool {
    parse_canonical("privateKey", private_key).is_ok()
}

/// `generateViewKeysFromPrivateSpendKey(privateKey)`: the view key pair of a deterministic
/// wallet (`xkr_wallet_core::view_key_from_spend`).
pub fn generate_view_keys_from_private_spend_key(private_key: &str) -> CryptoResult<KeyPair> {
    let spend = parse_secret("privateKey", private_key)?;
    let (secret, public) = xkr_wallet_core::view_key_from_spend(&spend);
    Ok(KeyPair {
        public_key: public.to_hex(),
        private_key: secret.to_hex(),
    })
}

/// `generatePrivateViewKeyFromPrivateSpendKey(privateKey)`.
pub fn generate_private_view_key_from_private_spend_key(private_key: &str) -> CryptoResult<String> {
    generate_view_keys_from_private_spend_key(private_key).map(|k| k.private_key)
}

// ---------------------------------------------------------------------------------------------
// derivations

/// `generateKeyDerivation(publicKey, privateKey)`: `8 * a * R`. Refuses a public key that does
/// not decode (C++ returns `false`).
pub fn generate_key_derivation(public_key: &str, private_key: &str) -> CryptoResult<String> {
    let public = parse_public("publicKey", public_key)?;
    let secret = parse_secret("privateKey", private_key)?;
    xkr_crypto::generate_key_derivation(&public, &secret)
        .map(|d| d.to_hex())
        .ok_or(CryptoError::InvalidPublicKey("publicKey"))
}

/// `derivePublicKey(derivation, outputIndex, publicKey)`: `B + H_s(D || index) * G`.
pub fn derive_public_key(
    derivation: &str,
    output_index: u64,
    public_key: &str,
) -> CryptoResult<String> {
    let derivation = parse_derivation("derivation", derivation)?;
    let base = parse_public("publicKey", public_key)?;
    xkr_crypto::derive_public_key(&derivation, output_index, &base)
        .map(|p| p.to_hex())
        .ok_or(CryptoError::InvalidPublicKey("publicKey"))
}

/// `deriveSecretKey(derivation, outputIndex, privateKey)`: `b + H_s(D || index)` mod `l`.
pub fn derive_secret_key(
    derivation: &str,
    output_index: u64,
    private_key: &str,
) -> CryptoResult<String> {
    let derivation = parse_derivation("derivation", derivation)?;
    let base = parse_secret("privateKey", private_key)?;
    Ok(xkr_crypto::derive_secret_key(&derivation, output_index, &base).to_hex())
}

/// `underivePublicKey(derivation, outputIndex, derivedKey)`: `P - H_s(D || index) * G`.
pub fn underive_public_key(
    derivation: &str,
    output_index: u64,
    derived_key: &str,
) -> CryptoResult<String> {
    let derivation = parse_derivation("derivation", derivation)?;
    let derived = parse_public("derivedKey", derived_key)?;
    xkr_crypto::underive_public_key(&derivation, output_index, &derived)
        .map(|p| p.to_hex())
        .ok_or(CryptoError::InvalidPublicKey("derivedKey"))
}

/// `derivationToScalar(derivation, outputIndex)`: `H_s(D || varint(index))`.
pub fn derivation_to_scalar(derivation: &str, output_index: u64) -> CryptoResult<String> {
    let derivation = parse_derivation("derivation", derivation)?;
    Ok(hex::encode(
        xkr_crypto::derivation_to_scalar(&derivation, output_index).to_bytes(),
    ))
}

/// `generateKeyDerivationScalar(publicKey, privateKey, outputIndex)`.
pub fn generate_key_derivation_scalar(
    public_key: &str,
    private_key: &str,
    output_index: u64,
) -> CryptoResult<String> {
    let derivation = generate_key_derivation(public_key, private_key)?;
    derivation_to_scalar(&derivation, output_index)
}

/// `scalarDerivePublicKey(derivationScalar, publicKey)`: `B + s * G`.
pub fn scalar_derive_public_key(derivation_scalar: &str, public_key: &str) -> CryptoResult<String> {
    let s = parse_canonical("derivationScalar", derivation_scalar)?;
    let base = parse_public("publicKey", public_key)?;
    let point =
        xkr_crypto::decompress(&base.0).ok_or(CryptoError::InvalidPublicKey("publicKey"))?;
    Ok(hex::encode(
        (point + EdwardsPoint::mul_base(&s)).compress().to_bytes(),
    ))
}

/// `scalarDeriveSecretKey(derivationScalar, privateKey)`: `b + s` mod `l`.
pub fn scalar_derive_secret_key(
    derivation_scalar: &str,
    private_key: &str,
) -> CryptoResult<String> {
    let s = parse_canonical("derivationScalar", derivation_scalar)?;
    let base = parse_secret("privateKey", private_key)?;
    Ok(hex::encode(
        (Scalar::from_bytes_mod_order(base.0) + s).to_bytes(),
    ))
}

/// `deriveViewTag(derivation, outputIndex)` (transaction version 2): the first byte of
/// `cn_fast_hash("view_tag" || D || varint(index))` as two lowercase hex characters.
pub fn derive_view_tag(derivation: &str, output_index: u64) -> CryptoResult<String> {
    let derivation = parse_derivation("derivation", derivation)?;
    Ok(format!(
        "{:02x}",
        xkr_crypto::view_tag(&derivation, output_index)
    ))
}

/// `generateKeyImage(publicKey, privateKey)`: `x * H_p(P)`. As in C++ the public key is only
/// hashed, not decoded, so any 32 bytes are accepted.
pub fn generate_key_image(public_key: &str, private_key: &str) -> CryptoResult<String> {
    let public = parse_public("publicKey", public_key)?;
    let secret = parse_secret("privateKey", private_key)?;
    Ok(xkr_crypto::generate_key_image(&public, &secret).to_hex())
}

// ---------------------------------------------------------------------------------------------
// hashing

/// `cnFastHash(data)`: Keccak-256 (`cn_fast_hash`) of the hex-decoded data.
pub fn cn_fast_hash(data: &str) -> CryptoResult<String> {
    let bytes = parse_bytes("data", data)?;
    Ok(xkr_crypto::cn_fast_hash(&bytes).to_hex())
}

/// `cnTurtleLiteSlowHashV2(data)`: CryptoNight-Turtle-Lite variant 2 (the PoW of blocks of
/// major version 5 and later, and of Hugin message shares).
pub fn cn_turtle_lite_slow_hash_v2(data: &str) -> CryptoResult<String> {
    let bytes = parse_bytes("data", data)?;
    Ok(xkr_crypto::cn_turtle_lite_slow_hash_v2_reused(&bytes).to_hex())
}

/// The CryptoNight families of kryptokrona-crypto's `cn_*_slow_hash_v{0,1,2}` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CnFamily {
    /// `cn_slow_hash_v*`.
    Cn,
    /// `cn_lite_slow_hash_v*`.
    Lite,
    /// `cn_dark_slow_hash_v*`.
    Dark,
    /// `cn_dark_lite_slow_hash_v*`.
    DarkLite,
    /// `cn_turtle_slow_hash_v*`.
    Turtle,
    /// `cn_turtle_lite_slow_hash_v*`.
    TurtleLite,
}

/// `cn_<family>_slow_hash_v<variant>(data)`; variant 1 needs at least 43 bytes.
pub fn cn_slow_hash(data: &str, family: CnFamily, variant: u8) -> CryptoResult<String> {
    use xkr_crypto::cryptonight::{CnParams, Variant};
    let bytes = parse_bytes("data", data)?;
    let params = match family {
        CnFamily::Cn => CnParams::CN,
        CnFamily::Lite => CnParams::CN_LITE,
        CnFamily::Dark => CnParams::CN_DARK,
        CnFamily::DarkLite => CnParams::CN_DARK_LITE,
        CnFamily::Turtle => CnParams::CN_TURTLE,
        CnFamily::TurtleLite => CnParams::CN_TURTLE_LITE,
    };
    let variant = match variant {
        0 => Variant::V0,
        1 => Variant::V1,
        2 => Variant::V2,
        _ => return Err(CryptoError::InvalidArgument("variant")),
    };
    xkr_crypto::cryptonight::cn_slow_hash(&bytes, variant, &params)
        .map(hex::encode)
        .map_err(|_| CryptoError::DataCanNotBeHashed)
}

/// `hashToEllipticCurve(hash)`: C++ `hash_data_to_ec` of the 32 bytes,
/// `8 * ge_fromfe_frombytes_vartime(cn_fast_hash(hash))`.
pub fn hash_to_elliptic_curve(hash: &str) -> CryptoResult<String> {
    let bytes = parse_fixed::<32>("hash", hash)?;
    Ok(xkr_crypto::hash_data_to_ec(&bytes).to_hex())
}

/// `hashToScalar(data)`: `sc_reduce32(cn_fast_hash(data))` of the hex-decoded data (any
/// length, as C++ `hashToScalar` takes it).
pub fn hash_to_scalar(data: &str) -> CryptoResult<String> {
    let bytes = parse_bytes("hash", data)?;
    Ok(hex::encode(xkr_crypto::hash_to_scalar(&bytes).to_bytes()))
}

/// `tree_hash(hashes)`: the CryptoNote Merkle root.
pub fn tree_hash<S: AsRef<str>>(hashes: &[S]) -> CryptoResult<String> {
    let hashes = parse_list("hashes", hashes, parse_hash)?;
    if hashes.is_empty() {
        return Err(CryptoError::InvalidArgument("hashes"));
    }
    Ok(xkr_crypto::tree_hash(&hashes).to_hex())
}

/// `tree_branch(hashes)`: the Merkle branch of the first hash.
pub fn tree_branch<S: AsRef<str>>(hashes: &[S]) -> CryptoResult<Vec<String>> {
    let hashes = parse_list("hashes", hashes, parse_hash)?;
    if hashes.is_empty() {
        return Err(CryptoError::InvalidArgument("hashes"));
    }
    Ok(xkr_crypto::tree_branch(&hashes)
        .iter()
        .map(Hash::to_hex)
        .collect())
}

/// `tree_depth(count)`: `floor(log2(count))`, 0 for `count <= 1` (C++ `tree_depth`, computed
/// on 64 bits so 32-bit targets agree with 64-bit ones).
#[must_use]
pub fn tree_depth(count: u64) -> u64 {
    if count <= 1 {
        0
    } else {
        u64::from(count.ilog2())
    }
}

// ---------------------------------------------------------------------------------------------
// signatures

/// `generateSignature(prefixHash, publicKey, privateKey)`: the CryptoNote Schnorr signature
/// `c || r` (128 hex characters). As in C++ the public key is hashed, not checked.
pub fn generate_signature(
    prefix_hash: &str,
    public_key: &str,
    private_key: &str,
) -> CryptoResult<String> {
    let hash = parse_hash("prefixHash", prefix_hash)?;
    let public = parse_public("publicKey", public_key)?;
    let secret = parse_secret("privateKey", private_key)?;
    Ok(xkr_crypto::generate_signature(&hash, &public, &secret, &mut xkr_crypto::os_rng()).to_hex())
}

/// `checkSignature(prefixHash, publicKey, signature)`. Malformed input is `false`.
#[must_use]
pub fn check_signature(prefix_hash: &str, public_key: &str, signature: &str) -> bool {
    let (Ok(hash), Ok(public), Ok(sig)) = (
        parse_hash("prefixHash", prefix_hash),
        parse_public("publicKey", public_key),
        parse_signature("signature", signature),
    ) else {
        return false;
    };
    xkr_crypto::check_signature(&hash, &public, &sig)
}

/// `generateRingSignatures(prefixHash, keyImage, publicKeys, privateKey, realIndex)`: the
/// CryptoNote ring signature, one `c || r` (128 hex characters) per ring member.
pub fn generate_ring_signatures<S: AsRef<str>>(
    prefix_hash: &str,
    key_image: &str,
    public_keys: &[S],
    private_key: &str,
    real_index: u64,
) -> CryptoResult<Vec<String>> {
    let hash = parse_hash("prefixHash", prefix_hash)?;
    let image = parse_key_image("keyImage", key_image)?;
    let ring = parse_list("publicKeys", public_keys, parse_public)?;
    let secret = parse_secret("privateKey", private_key)?;
    let index = to_index("realIndex", real_index)?;
    if index >= ring.len() {
        return Err(CryptoError::InvalidIndex("realIndex"));
    }
    let sigs = xkr_crypto::generate_ring_signature(
        &hash,
        &image,
        &ring,
        &secret,
        index,
        &mut xkr_crypto::os_rng(),
    )
    .ok_or(CryptoError::SigningFailed)?;
    Ok(sigs.iter().map(Signature::to_hex).collect())
}

/// `checkRingSignature(prefixHash, keyImage, publicKeys, signatures)`: C++
/// `checkRingSignature` (key image in the prime-order subgroup, canonical scalars, decodable
/// ring members). Malformed input, an empty ring and a signature count different from the
/// ring size are `false`.
#[must_use]
pub fn check_ring_signature<S: AsRef<str>, T: AsRef<str>>(
    prefix_hash: &str,
    key_image: &str,
    public_keys: &[S],
    signatures: &[T],
) -> bool {
    if public_keys.is_empty() || public_keys.len() != signatures.len() {
        return false;
    }
    let (Ok(hash), Ok(image), Ok(ring), Ok(sigs)) = (
        parse_hash("prefixHash", prefix_hash),
        parse_key_image("keyImage", key_image),
        parse_list("publicKeys", public_keys, parse_public),
        parse_list("signatures", signatures, parse_signature),
    ) else {
        return false;
    };
    xkr_crypto::check_ring_signature(&hash, &image, &ring, &sigs)
}

/// Prepared ring signatures (`IPreparedRingSignatures`: `{ signatures, k }`).
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedRingSignatures {
    /// The signatures; the real member's `r` is zero until [`complete_ring_signatures`].
    pub signatures: Vec<String>,
    /// The nonce `k` (64 hex characters).
    pub k: String,
}

impl std::fmt::Debug for PreparedRingSignatures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRingSignatures")
            .field("signatures", &self.signatures)
            .finish_non_exhaustive()
    }
}

/// `prepareRingSignatures(hash, keyImage, publicKeys, realIndex, k?)`: C++
/// `crypto_funcs::prepareRingSignatures`; `k` is drawn from the OS when `None`.
pub fn prepare_ring_signatures<S: AsRef<str>>(
    prefix_hash: &str,
    key_image: &str,
    public_keys: &[S],
    real_index: u64,
    k: Option<&str>,
) -> CryptoResult<PreparedRingSignatures> {
    use xkr_crypto::ops::{decompress, hash_to_ec, random_scalar};
    let hash = parse_hash("prefixHash", prefix_hash)?;
    let image = parse_key_image("keyImage", key_image)?;
    let ring = parse_list("publicKeys", public_keys, parse_public)?;
    let index = to_index("realIndex", real_index)?;
    if index >= ring.len() {
        return Err(CryptoError::InvalidIndex("realIndex"));
    }
    let mut random = xkr_crypto::os_rng();
    let k = match k {
        Some(k) => parse_canonical("k", k)?,
        None => random_scalar(&mut random),
    };
    let image_point = decompress(&image.0).ok_or(CryptoError::InvalidPublicKey("keyImage"))?;
    let mut sigs = vec![Signature::ZERO; ring.len()];
    let mut buf = Vec::with_capacity(32 + 64 * ring.len());
    buf.extend_from_slice(&hash.0);
    let mut sum = Scalar::ZERO;
    for (i, public) in ring.iter().enumerate() {
        let hp = hash_to_ec(public);
        if i == index {
            buf.extend_from_slice(EdwardsPoint::mul_base(&k).compress().as_bytes());
            buf.extend_from_slice((hp * k).compress().as_bytes());
        } else {
            let c = random_scalar(&mut random);
            let r = random_scalar(&mut random);
            let p = decompress(&public.0).ok_or(CryptoError::InvalidPublicKey("publicKeys"))?;
            let a = EdwardsPoint::vartime_double_scalar_mul_basepoint(&c, &p, &r);
            buf.extend_from_slice(a.compress().as_bytes());
            buf.extend_from_slice((hp * r + image_point * c).compress().as_bytes());
            sigs[i] = Signature::from_parts(&c.to_bytes(), &r.to_bytes());
            sum += c;
        }
    }
    let c_real = xkr_crypto::hash_to_scalar(&buf) - sum;
    sigs[index] = Signature::from_parts(&c_real.to_bytes(), &[0u8; 32]);
    Ok(PreparedRingSignatures {
        signatures: sigs.iter().map(Signature::to_hex).collect(),
        k: hex::encode(k.to_bytes()),
    })
}

/// `completeRingSignatures(privateKey, realIndex, k, signatures)`: sets the real member's
/// `r = k - c * x` (C++ `sc_mulsub`).
pub fn complete_ring_signatures<S: AsRef<str>>(
    private_key: &str,
    real_index: u64,
    k: &str,
    signatures: &[S],
) -> CryptoResult<Vec<String>> {
    let secret = parse_secret("privateKey", private_key)?;
    let k = parse_canonical("k", k)?;
    let mut sigs = parse_list("signatures", signatures, parse_signature)?;
    let index = to_index("realIndex", real_index)?;
    let real = sigs
        .get_mut(index)
        .ok_or(CryptoError::InvalidIndex("realIndex"))?;
    let c = Scalar::from_bytes_mod_order(real.c());
    let r = k - c * Scalar::from_bytes_mod_order(secret.0);
    *real = Signature::from_parts(&real.c(), &r.to_bytes());
    Ok(sigs.iter().map(Signature::to_hex).collect())
}

/// `clsagSign(prefixHash, ring, privateEphemeral, realOutputIndex, alpha, decoyResponses)`
/// (transaction version 2). With `alpha == ""` and no decoy responses the randomness comes from
/// the OS; otherwise both must be given (the nonce and the `ring.len() - 1` decoy responses in
/// consumption order `s_{l+1} .. s_{n-1}, s_0 .. s_{l-1}`), which makes the signature
/// deterministic. Refuses a secret that does not belong to `ring[realOutputIndex]`.
pub fn clsag_sign<S: AsRef<str>, T: AsRef<str>>(
    prefix_hash: &str,
    ring: &[S],
    private_ephemeral: &str,
    real_output_index: u64,
    alpha: &str,
    decoy_responses: &[T],
) -> CryptoResult<Clsag> {
    let hash = parse_hash("prefixHash", prefix_hash)?;
    let keys = parse_list("ring", ring, parse_public)?;
    let secret = parse_secret("privateEphemeral", private_ephemeral)?;
    if !check_scalar(private_ephemeral) {
        return Err(CryptoError::InvalidScalar("privateEphemeral"));
    }
    let index = to_index("realOutputIndex", real_output_index)?;
    if keys.is_empty() {
        return Err(CryptoError::InvalidArgument("ring"));
    }
    if index >= keys.len() {
        return Err(CryptoError::InvalidIndex("realOutputIndex"));
    }
    let signed = if alpha.is_empty() && decoy_responses.is_empty() {
        xkr_crypto::clsag_sign(&hash, &keys, &secret, index, &mut xkr_crypto::os_rng())
    } else {
        if decoy_responses.len() + 1 != keys.len() {
            return Err(CryptoError::InvalidArgument("decoyResponses"));
        }
        if !check_scalar(alpha) {
            return Err(CryptoError::InvalidScalar("alpha"));
        }
        let alpha = parse_ec_scalar("alpha", alpha)?;
        let decoys = parse_list("decoyResponses", decoy_responses, parse_ec_scalar)?;
        if !decoys
            .iter()
            .all(|s| bool::from(Scalar::from_canonical_bytes(s.0).is_some()))
        {
            return Err(CryptoError::InvalidScalar("decoyResponses"));
        }
        xkr_crypto::clsag_sign_with_randomness(&hash, &keys, &secret, index, &alpha, &decoys)
    };
    let sig = signed.ok_or(CryptoError::SigningFailed)?;
    Ok(Clsag {
        c1: sig.c1.to_hex(),
        s: sig.s.iter().map(EcScalar::to_hex).collect(),
    })
}

/// `clsagVerify(prefixHash, ring, keyImage, c1, s)` (transaction version 2). Malformed input is
/// `false`, as an invalid signature is.
#[must_use]
pub fn clsag_verify<S: AsRef<str>, T: AsRef<str>>(
    prefix_hash: &str,
    ring: &[S],
    key_image: &str,
    c1: &str,
    s: &[T],
) -> bool {
    let (Ok(hash), Ok(keys), Ok(image), Ok(c1), Ok(s)) = (
        parse_hash("prefixHash", prefix_hash),
        parse_list("ring", ring, parse_public),
        parse_key_image("keyImage", key_image),
        parse_ec_scalar("c1", c1),
        parse_list("s", s, parse_ec_scalar),
    ) else {
        return false;
    };
    xkr_crypto::clsag_verify(&hash, &keys, &image, &ClsagSignature { s, c1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DERIVATION: &str = "4827dbde0c0994c0979e2f9c046825bb4a065b6e35cabc0290ff5216af060c20";

    #[test]
    fn fixed_size_values_need_their_exact_length() {
        assert_eq!(
            sc_reduce32("00"),
            Err(CryptoError::InvalidLength {
                what: "scalar",
                expected: 64,
                got: 2
            })
        );
        assert_eq!(
            sc_reduce32(&"zz".repeat(32)),
            Err(CryptoError::InvalidHex("scalar"))
        );
        assert!(!check_key(""));
        assert!(!check_key(&"0".repeat(63)));
        assert_eq!(cn_fast_hash("0"), Err(CryptoError::InvalidHex("data")));
    }

    #[test]
    fn upper_case_hex_is_accepted_and_output_is_lower_case() {
        let lower = derive_secret_key(
            DERIVATION,
            2,
            "d9d555a892a85f64916cae1a168bd3f7f400b6471c7b12b438b599601298210b",
        )
        .unwrap();
        let upper = derive_secret_key(
            &DERIVATION.to_uppercase(),
            2,
            &"d9d555a892a85f64916cae1a168bd3f7f400b6471c7b12b438b599601298210b".to_uppercase(),
        )
        .unwrap();
        assert_eq!(lower, upper);
        assert_eq!(lower, lower.to_lowercase());
    }

    #[test]
    fn generated_keys_are_consistent_and_distinct() {
        let a = generate_keys();
        let b = generate_keys();
        assert_ne!(a, b);
        assert_eq!(
            secret_key_to_public_key(&a.private_key).unwrap(),
            a.public_key
        );
        assert!(check_key(&a.public_key));
        assert!(check_scalar(&a.private_key));
    }

    #[test]
    fn non_canonical_secret_key_is_refused() {
        // l itself is not a canonical scalar
        let l = "edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010";
        assert_eq!(
            secret_key_to_public_key(l),
            Err(CryptoError::InvalidScalar("privateKey"))
        );
        assert!(!check_scalar(l));
        assert_eq!(sc_reduce32(l).unwrap(), "0".repeat(64));
    }

    #[test]
    fn hash_to_elliptic_curve_is_the_key_image_base_point() {
        // generateKeyImage(P, 1) = 1 * H_p(P) = hashToEllipticCurve(P)
        let p = "bb55bef919d1c9f74b5b52a8a6995a1dc4af4c0bb8824f5dc889012bc748173d";
        let mut one = "01".to_owned();
        one.push_str(&"0".repeat(62));
        assert_eq!(
            hash_to_elliptic_curve(p).unwrap(),
            generate_key_image(p, &one).unwrap()
        );
    }

    #[test]
    fn signatures_round_trip() {
        let keys = generate_keys();
        let hash = cn_fast_hash("68656c6c6f").unwrap();
        let sig = generate_signature(&hash, &keys.public_key, &keys.private_key).unwrap();
        assert_eq!(sig.len(), 128);
        assert!(check_signature(&hash, &keys.public_key, &sig));
        let other = cn_fast_hash("68656c6c6e").unwrap();
        assert!(!check_signature(&other, &keys.public_key, &sig));
        assert!(!check_signature(&hash, &keys.public_key, &sig[..126]));
    }

    #[test]
    fn ring_signatures_round_trip_and_reject_malformed_rings() {
        let hash = cn_fast_hash("00").unwrap();
        let ring: Vec<KeyPair> = (0..4).map(|_| generate_keys()).collect();
        let pubs: Vec<String> = ring.iter().map(|k| k.public_key.clone()).collect();
        for real in 0..4u64 {
            let secret = &ring[real as usize].private_key;
            let image = generate_key_image(&pubs[real as usize], secret).unwrap();
            let sigs = generate_ring_signatures(&hash, &image, &pubs, secret, real).unwrap();
            assert_eq!(sigs.len(), 4);
            assert!(check_ring_signature(&hash, &image, &pubs, &sigs));
            assert!(!check_ring_signature(&hash, &image, &pubs, &sigs[..3]));
            assert!(!check_ring_signature(&hash, &image, &pubs[..3], &sigs[..3]));
        }
        let image = generate_key_image(&pubs[0], &ring[0].private_key).unwrap();
        assert_eq!(
            generate_ring_signatures(&hash, &image, &pubs, &ring[0].private_key, 4),
            Err(CryptoError::InvalidIndex("realIndex"))
        );
        let empty: [&str; 0] = [];
        assert!(!check_ring_signature(&hash, &image, &empty, &empty));
    }

    #[test]
    fn prepared_and_completed_ring_signatures_verify() {
        let hash = cn_fast_hash("01").unwrap();
        let ring: Vec<KeyPair> = (0..3).map(|_| generate_keys()).collect();
        let pubs: Vec<String> = ring.iter().map(|k| k.public_key.clone()).collect();
        let image = generate_key_image(&pubs[1], &ring[1].private_key).unwrap();
        let k = generate_keys().private_key;
        let prepared = prepare_ring_signatures(&hash, &image, &pubs, 1, Some(&k)).unwrap();
        assert_eq!(prepared.k, k);
        let done =
            complete_ring_signatures(&ring[1].private_key, 1, &prepared.k, &prepared.signatures)
                .unwrap();
        assert!(check_ring_signature(&hash, &image, &pubs, &done));
    }

    #[test]
    fn clsag_round_trip_and_randomness_rules() {
        let hash = cn_fast_hash("02").unwrap();
        let ring: Vec<KeyPair> = (0..5).map(|_| generate_keys()).collect();
        let pubs: Vec<String> = ring.iter().map(|k| k.public_key.clone()).collect();
        let none: [&str; 0] = [];
        for real in 0..5usize {
            let secret = &ring[real].private_key;
            let image = generate_key_image(&pubs[real], secret).unwrap();
            let sig = clsag_sign(&hash, &pubs, secret, real as u64, "", &none).unwrap();
            assert_eq!(sig.s.len(), 5);
            assert!(clsag_verify(&hash, &pubs, &image, &sig.c1, &sig.s));
            assert!(!clsag_verify(&hash, &pubs, &image, &sig.c1, &sig.s[..4]));
        }
        // a secret of another member
        assert_eq!(
            clsag_sign(&hash, &pubs, &ring[0].private_key, 1, "", &none),
            Err(CryptoError::SigningFailed)
        );
        // alpha without decoy responses
        let alpha = generate_keys().private_key;
        assert_eq!(
            clsag_sign(&hash, &pubs, &ring[0].private_key, 0, &alpha, &none),
            Err(CryptoError::InvalidArgument("decoyResponses"))
        );
        // explicit randomness is deterministic
        let decoys: Vec<String> = (0..4).map(|_| generate_keys().private_key).collect();
        let a = clsag_sign(&hash, &pubs, &ring[2].private_key, 2, &alpha, &decoys).unwrap();
        let b = clsag_sign(&hash, &pubs, &ring[2].private_key, 2, &alpha, &decoys).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn view_tag_is_two_hex_characters() {
        let tag = derive_view_tag(DERIVATION, 0).unwrap();
        assert_eq!(tag.len(), 2);
        assert!(
            tag.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn tree_depth_is_floor_log2() {
        assert_eq!(tree_depth(0), 0);
        assert_eq!(tree_depth(1), 0);
        assert_eq!(tree_depth(2), 1);
        assert_eq!(tree_depth(3), 1);
        assert_eq!(tree_depth(4), 2);
        assert_eq!(tree_depth(1 << 40), 40);
        for n in 0..300u64 {
            assert_eq!(tree_depth(n), xkr_crypto::tree_depth(n as usize) as u64);
        }
    }

    #[test]
    fn scalar_derivation_matches_index_derivation() {
        let base_public = "854a637b2863af9e8e8216eb2382f3d16616b3ac3e53d0976fbd6f8da6c56418";
        let base_secret = "d9d555a892a85f64916cae1a168bd3f7f400b6471c7b12b438b599601298210b";
        let s = derivation_to_scalar(DERIVATION, 2).unwrap();
        assert_eq!(
            scalar_derive_public_key(&s, base_public).unwrap(),
            derive_public_key(DERIVATION, 2, base_public).unwrap()
        );
        assert_eq!(
            scalar_derive_secret_key(&s, base_secret).unwrap(),
            derive_secret_key(DERIVATION, 2, base_secret).unwrap()
        );
    }
}
