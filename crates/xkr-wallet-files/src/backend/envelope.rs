//! The encrypted envelope of a `WalletBackend` file.
//!
//! Mirrors `src/wallet_backend/wallet_backend.cpp` `WalletBackend::openWallet` (decrypt) and
//! `WalletBackend::unsafeSave` (encrypt), with the constants of
//! `src/wallet_backend/constants.h`; wallet-backend-js `lib/WalletEncryption.ts` is the same
//! format.
//!
//! ```text
//! file  = IS_A_WALLET_IDENTIFIER (64 bytes, plain) || salt (16) || AES-128-CBC-PKCS7(key, iv = salt, plain)
//! key   = PBKDF2-HMAC-SHA256(password, salt, 500 000 iterations, 16 bytes)
//! plain = IS_CORRECT_PASSWORD_IDENTIFIER (26 bytes) || JSON
//! ```

use aes::cipher::{
    BlockModeDecrypt as _, BlockModeEncrypt as _, KeyIvInit as _, block_padding::Pkcs7,
};
use zeroize::{Zeroize as _, Zeroizing};

use crate::FileError;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// C++ `Constants::IS_A_WALLET_IDENTIFIER`: the plain first 64 bytes of every `WalletBackend`
/// file (`"If I pull that off, will you die?\nIt would be extremely painful."`).
pub const IS_A_WALLET_IDENTIFIER: [u8; 64] =
    *b"If I pull that off, will you die?\nIt would be extremely painful.";

/// C++ `Constants::IS_CORRECT_PASSWORD_IDENTIFIER`: the first 26 bytes of the decrypted data
/// (`"You're a big guy.\nFor you."`).
pub const IS_CORRECT_PASSWORD_IDENTIFIER: [u8; 26] = *b"You're a big guy.\nFor you.";

/// C++ `Constants::PBKDF2_ITERATIONS`.
pub const PBKDF2_ITERATIONS: u32 = 500_000;

/// Size of the salt, which is also the AES IV.
pub const SALT_SIZE: usize = 16;

/// Size of the AES-128 key derived from the password.
const KEY_SIZE: usize = 16;

/// Offset of the ciphertext in the file.
const CIPHERTEXT_OFFSET: usize = IS_A_WALLET_IDENTIFIER.len() + SALT_SIZE;

/// AES block size.
const BLOCK: usize = 16;

/// The salt of a `WalletBackend` file, `None` if `file` is too short or lacks the identifier.
/// Re-encrypting with [`encrypt`] and this salt reproduces the file byte for byte when the
/// plaintext is unchanged.
#[must_use]
pub fn salt_of(file: &[u8]) -> Option<[u8; SALT_SIZE]> {
    if !file.starts_with(&IS_A_WALLET_IDENTIFIER) {
        return None;
    }
    file.get(IS_A_WALLET_IDENTIFIER.len()..CIPHERTEXT_OFFSET)?
        .try_into()
        .ok()
}

/// Decrypts a `WalletBackend` file and returns the JSON text (without the password identifier).
///
/// Errors, in the order C++ `WalletBackend::openWallet` checks them:
/// * identifier missing or file shorter than 64 bytes: [`FileError::NotAWalletFile`];
/// * fewer than 16 bytes of salt: [`FileError::WalletFileCorrupted`];
/// * AES/PKCS7 failure (also an empty or misaligned ciphertext): [`FileError::WrongPassword`];
/// * decrypted data shorter than 26 bytes: [`FileError::WalletFileCorrupted`];
/// * password identifier missing: [`FileError::WrongPassword`].
pub fn decrypt(file: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>, FileError> {
    decrypt_with_iterations(file, password, PBKDF2_ITERATIONS)
}

/// Encrypts `json` into a `WalletBackend` file with the given salt (C++ uses 16 random bytes,
/// see [`super::save`]).
#[must_use]
pub fn encrypt(json: &[u8], password: &str, salt: &[u8; SALT_SIZE]) -> Vec<u8> {
    encrypt_with_iterations(json, password, salt, PBKDF2_ITERATIONS)
}

pub(crate) fn decrypt_with_iterations(
    file: &[u8],
    password: &str,
    iterations: u32,
) -> Result<Zeroizing<Vec<u8>>, FileError> {
    if !file.starts_with(&IS_A_WALLET_IDENTIFIER) {
        return Err(FileError::NotAWalletFile);
    }
    let rest = &file[IS_A_WALLET_IDENTIFIER.len()..];
    if rest.len() < SALT_SIZE {
        return Err(FileError::corrupted("file ends inside the salt"));
    }
    let (salt, ciphertext) = rest.split_at(SALT_SIZE);
    let salt: [u8; SALT_SIZE] = salt.try_into().expect("split at SALT_SIZE");

    // Crypto++ `StreamTransformationFilter` rejects an empty or misaligned ciphertext as it
    // rejects bad padding; C++ reports all of them as a wrong password.
    if ciphertext.is_empty() || ciphertext.len() % BLOCK != 0 {
        return Err(FileError::WrongPassword);
    }

    let mut key = derive_key(password, &salt, iterations);
    let plain =
        Aes128CbcDec::new(&key.into(), &salt.into()).decrypt_padded_vec::<Pkcs7>(ciphertext);
    key.zeroize();
    let plain = Zeroizing::new(plain.map_err(|_| FileError::WrongPassword)?);

    if plain.len() < IS_CORRECT_PASSWORD_IDENTIFIER.len() {
        return Err(FileError::corrupted(
            "decrypted data shorter than the password identifier",
        ));
    }
    if !plain.starts_with(&IS_CORRECT_PASSWORD_IDENTIFIER) {
        return Err(FileError::WrongPassword);
    }
    Ok(Zeroizing::new(
        plain[IS_CORRECT_PASSWORD_IDENTIFIER.len()..].to_vec(),
    ))
}

pub(crate) fn encrypt_with_iterations(
    json: &[u8],
    password: &str,
    salt: &[u8; SALT_SIZE],
    iterations: u32,
) -> Vec<u8> {
    let mut plain = Zeroizing::new(Vec::with_capacity(
        IS_CORRECT_PASSWORD_IDENTIFIER.len() + json.len(),
    ));
    plain.extend_from_slice(&IS_CORRECT_PASSWORD_IDENTIFIER);
    plain.extend_from_slice(json);

    let mut key = derive_key(password, salt, iterations);
    let ciphertext =
        Aes128CbcEnc::new(&key.into(), &(*salt).into()).encrypt_padded_vec::<Pkcs7>(&plain);
    key.zeroize();

    let mut out = Vec::with_capacity(CIPHERTEXT_OFFSET + ciphertext.len());
    out.extend_from_slice(&IS_A_WALLET_IDENTIFIER);
    out.extend_from_slice(salt);
    out.extend_from_slice(&ciphertext);
    out
}

/// C++ `CryptoPP::PKCS5_PBKDF2_HMAC<CryptoPP::SHA256>::DeriveKey` with a 16-byte output; JS
/// `pbkdf2.pbkdf2Sync(password, salt, PBKDF2_ITERATIONS, 16, 'sha256')`.
fn derive_key(password: &str, salt: &[u8; SALT_SIZE], iterations: u32) -> [u8; KEY_SIZE] {
    let mut key = [0u8; KEY_SIZE];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), salt, iterations, &mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: u32 = 2;

    #[test]
    fn identifiers_match_cpp_constants() {
        // constants.h, byte by byte
        let a: [u8; 64] = [
            0x49, 0x66, 0x20, 0x49, 0x20, 0x70, 0x75, 0x6c, 0x6c, 0x20, 0x74, 0x68, 0x61, 0x74,
            0x20, 0x6f, 0x66, 0x66, 0x2c, 0x20, 0x77, 0x69, 0x6c, 0x6c, 0x20, 0x79, 0x6f, 0x75,
            0x20, 0x64, 0x69, 0x65, 0x3f, 0x0a, 0x49, 0x74, 0x20, 0x77, 0x6f, 0x75, 0x6c, 0x64,
            0x20, 0x62, 0x65, 0x20, 0x65, 0x78, 0x74, 0x72, 0x65, 0x6d, 0x65, 0x6c, 0x79, 0x20,
            0x70, 0x61, 0x69, 0x6e, 0x66, 0x75, 0x6c, 0x2e,
        ];
        let b: [u8; 26] = [
            0x59, 0x6f, 0x75, 0x27, 0x72, 0x65, 0x20, 0x61, 0x20, 0x62, 0x69, 0x67, 0x20, 0x67,
            0x75, 0x79, 0x2e, 0x0a, 0x46, 0x6f, 0x72, 0x20, 0x79, 0x6f, 0x75, 0x2e,
        ];
        assert_eq!(IS_A_WALLET_IDENTIFIER, a);
        assert_eq!(IS_CORRECT_PASSWORD_IDENTIFIER, b);
    }

    /// PBKDF2-HMAC-SHA256 vectors (RFC 7914 §11 for c = 1 and the widely published c = 2 and
    /// c = 4096 values for P = "password", S = "salt"), first 16 bytes, as the envelope uses.
    #[test]
    fn pbkdf2_known_answers() {
        let mut out = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", b"salt", 1, &mut out);
        assert_eq!(
            hex::encode(out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", b"salt", 2, &mut out);
        assert_eq!(
            hex::encode(out),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", b"salt", 4096, &mut out);
        assert_eq!(
            hex::encode(out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    /// FIPS-197 appendix C.1 (AES-128) as a one-block CBC encryption with a zero IV; the second
    /// block is the PKCS7 padding block.
    #[test]
    fn aes128_cbc_known_answer() {
        let key: [u8; 16] = hex::decode("000102030405060708090a0b0c0d0e0f")
            .unwrap()
            .try_into()
            .unwrap();
        let pt = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let ct = Aes128CbcEnc::new(&key.into(), &[0u8; 16].into()).encrypt_padded_vec::<Pkcs7>(&pt);
        assert_eq!(ct.len(), 32);
        assert_eq!(hex::encode(&ct[..16]), "69c4e0d86a7b0430d8cdb78070b4c55a");
        let back = Aes128CbcDec::new(&key.into(), &[0u8; 16].into())
            .decrypt_padded_vec::<Pkcs7>(&ct)
            .unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn roundtrip_and_layout() {
        let salt = [7u8; 16];
        let json = br#"{"walletFileFormatVersion":0}"#;
        let file = encrypt_with_iterations(json, "hunter2", &salt, FAST);
        assert!(file.starts_with(&IS_A_WALLET_IDENTIFIER));
        assert_eq!(salt_of(&file), Some(salt));
        // 26 + 29 = 55 bytes of plaintext -> 4 blocks with PKCS7
        assert_eq!(file.len(), 64 + 16 + 64);
        let plain = decrypt_with_iterations(&file, "hunter2", FAST).unwrap();
        assert_eq!(plain.as_slice(), json);
        // deterministic for a fixed salt
        assert_eq!(file, encrypt_with_iterations(json, "hunter2", &salt, FAST));
    }

    #[test]
    fn empty_password_is_a_password() {
        let file = encrypt_with_iterations(b"{}", "", &[1u8; 16], FAST);
        assert_eq!(
            decrypt_with_iterations(&file, "", FAST).unwrap().as_slice(),
            b"{}"
        );
        assert!(matches!(
            decrypt_with_iterations(&file, "x", FAST),
            Err(FileError::WrongPassword | FileError::WalletFileCorrupted(_))
        ));
    }

    #[test]
    fn error_order_matches_cpp() {
        // too short / no identifier
        assert!(matches!(
            decrypt_with_iterations(b"", "", FAST),
            Err(FileError::NotAWalletFile)
        ));
        assert!(matches!(
            decrypt_with_iterations(&IS_A_WALLET_IDENTIFIER[..63], "", FAST),
            Err(FileError::NotAWalletFile)
        ));
        let mut other = IS_A_WALLET_IDENTIFIER.to_vec();
        other[0] ^= 1;
        other.extend_from_slice(&[0u8; 48]);
        assert!(matches!(
            decrypt_with_iterations(&other, "", FAST),
            Err(FileError::NotAWalletFile)
        ));

        // identifier but no full salt
        let mut f = IS_A_WALLET_IDENTIFIER.to_vec();
        f.extend_from_slice(&[0u8; 15]);
        assert!(matches!(
            decrypt_with_iterations(&f, "", FAST),
            Err(FileError::WalletFileCorrupted(_))
        ));

        // salt, no ciphertext; misaligned ciphertext
        f.push(0);
        assert!(matches!(
            decrypt_with_iterations(&f, "", FAST),
            Err(FileError::WrongPassword)
        ));
        f.extend_from_slice(&[0u8; 17]);
        assert!(matches!(
            decrypt_with_iterations(&f, "", FAST),
            Err(FileError::WrongPassword)
        ));
    }

    #[test]
    fn short_plaintext_is_corrupted_and_wrong_identifier_is_wrong_password() {
        let salt = [3u8; 16];
        let key = derive_key("pw", &salt, FAST);
        let enc = |plain: &[u8]| {
            let ct =
                Aes128CbcEnc::new(&key.into(), &salt.into()).encrypt_padded_vec::<Pkcs7>(plain);
            let mut f = IS_A_WALLET_IDENTIFIER.to_vec();
            f.extend_from_slice(&salt);
            f.extend_from_slice(&ct);
            f
        };
        assert!(matches!(
            decrypt_with_iterations(&enc(b"You're a big guy."), "pw", FAST),
            Err(FileError::WalletFileCorrupted(_))
        ));
        assert!(matches!(
            decrypt_with_iterations(&enc(b"You're a big guy.\nFor me!{}"), "pw", FAST),
            Err(FileError::WrongPassword)
        ));
        // exactly the identifier: empty JSON text (the JSON parser then reports corruption)
        assert_eq!(
            decrypt_with_iterations(&enc(&IS_CORRECT_PASSWORD_IDENTIFIER), "pw", FAST)
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn wrong_password_is_rejected() {
        let file = encrypt_with_iterations(b"{\"a\":1}", "right", &[9u8; 16], FAST);
        // A wrong key gives either bad padding or a wrong identifier; both are WRONG_PASSWORD,
        // unless the garbage happens to unpad to fewer than 26 bytes (corrupted).
        for pw in ["wrong", "Right", "right ", ""] {
            assert!(matches!(
                decrypt_with_iterations(&file, pw, FAST),
                Err(FileError::WrongPassword | FileError::WalletFileCorrupted(_))
            ));
        }
    }

    /// One full-strength round trip with the real iteration count.
    #[test]
    fn full_iterations_roundtrip() {
        let file = encrypt(b"{}", "pw", &[0u8; 16]);
        assert_eq!(decrypt(&file, "pw").unwrap().as_slice(), b"{}");
    }
}
