//! The `X-API-KEY` check (`src/wallet_api/api_dispatcher.cpp` `ApiDispatcher::checkAuthenticated`).
//!
//! Every route except `OPTIONS` needs the header `X-API-KEY` equal to `--rpc-password`; a
//! missing or different key is HTTP 401 with an empty body. C++ compares PBKDF2 hashes of the
//! two keys; here both sides are hashed with SHA-256 and the digests compared in constant time,
//! which gives the same answers.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The configured key, kept only as its digest.
#[derive(Clone)]
pub struct ApiKey {
    digest: [u8; 32],
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKey").finish_non_exhaustive()
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let d = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&d[..]);
    out
}

impl ApiKey {
    /// The key `--rpc-password` sets (`ApiDispatcher::ApiDispatcher`, `hashPassword`).
    #[must_use]
    pub fn new(password: &str) -> Self {
        Self {
            digest: digest(password.as_bytes()),
        }
    }

    /// Whether `headers` carry the right `X-API-KEY` (the first one if there are several).
    #[must_use]
    pub fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some(given) = headers.get("x-api-key") else {
            return false;
        };
        bool::from(digest(given.as_bytes())[..].ct_eq(&self.digest[..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn with(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-API-KEY", HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn checks_the_header() {
        let key = ApiKey::new("secret");
        assert!(key.accepts(&with("secret")));
        assert!(!key.accepts(&with("Secret")));
        assert!(!key.accepts(&with("secret ")));
        assert!(!key.accepts(&with("")));
        assert!(!key.accepts(&HeaderMap::new()));
        let mut lower = HeaderMap::new();
        lower.insert("x-api-key", HeaderValue::from_static("secret"));
        assert!(key.accepts(&lower));
        // an empty --rpc-password is a valid key, matched by an empty header
        assert!(ApiKey::new("").accepts(&with("")));
        assert!(!ApiKey::new("").accepts(&HeaderMap::new()));
        assert!(!format!("{key:?}").contains("secret"));
    }
}
