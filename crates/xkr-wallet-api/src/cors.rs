//! `OPTIONS` on any path (`src/wallet_api/api_dispatcher.cpp` `ApiDispatcher::handleOptions`).
//!
//! It skips the middleware (no `X-API-KEY` needed) and answers 200 without a body:
//! * with `Access-Control-Request-Method` in the request (a preflight):
//!   `Access-Control-Allow-Methods`, otherwise `Allow`; the value is
//!   `OPTIONS, GET, POST, PUT, DELETE` when `--enable-cors` is set and empty otherwise;
//! * with `--enable-cors`: `Access-Control-Allow-Origin: <value>` and
//!   `Access-Control-Allow-Headers: Origin, X-Requested-With, Content-Type, Accept, X-API-KEY`.
//!
//! Every other answer that passes the middleware carries `Access-Control-Allow-Origin` when
//! `--enable-cors` is set ([`crate::respond::Reply::into_response`]).

use axum::http::HeaderMap;

use crate::respond::Reply;

/// The methods `OPTIONS` lists when CORS is on.
pub const ALLOWED_METHODS: &str = "OPTIONS, GET, POST, PUT, DELETE";

/// `Access-Control-Allow-Headers`.
pub const ALLOWED_HEADERS: &str = "Origin, X-Requested-With, Content-Type, Accept, X-API-KEY";

/// The answer to `OPTIONS`.
#[must_use]
pub fn options(cors: Option<&str>, headers: &HeaderMap) -> Reply {
    let supported = if cors.is_some() { ALLOWED_METHODS } else { "" };
    let reply = Reply::empty(200);
    let reply = if headers.contains_key("access-control-request-method") {
        reply.with_header("access-control-allow-methods", supported)
    } else {
        reply.with_header("allow", supported)
    };
    match cors {
        Some(origin) => reply
            .with_header("access-control-allow-origin", origin)
            .with_header("access-control-allow-headers", ALLOWED_HEADERS),
        None => reply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn header<'a>(r: &'a Reply, name: &str) -> Option<&'a str> {
        r.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn without_cors() {
        let r = options(None, &HeaderMap::new());
        assert_eq!(r.status, 200);
        assert_eq!(r.body, None);
        assert_eq!(header(&r, "allow"), Some(""));
        assert_eq!(header(&r, "access-control-allow-origin"), None);
        let mut pre = HeaderMap::new();
        pre.insert(
            "Access-Control-Request-Method",
            HeaderValue::from_static("POST"),
        );
        let r = options(None, &pre);
        assert_eq!(header(&r, "access-control-allow-methods"), Some(""));
        assert_eq!(header(&r, "allow"), None);
    }

    #[test]
    fn with_cors() {
        let mut pre = HeaderMap::new();
        pre.insert(
            "Access-Control-Request-Method",
            HeaderValue::from_static("POST"),
        );
        let r = options(Some("https://a.example"), &pre);
        assert_eq!(
            header(&r, "access-control-allow-methods"),
            Some(ALLOWED_METHODS)
        );
        assert_eq!(
            header(&r, "access-control-allow-origin"),
            Some("https://a.example")
        );
        assert_eq!(
            header(&r, "access-control-allow-headers"),
            Some(ALLOWED_HEADERS)
        );
        let r = options(Some("*"), &HeaderMap::new());
        assert_eq!(header(&r, "allow"), Some(ALLOWED_METHODS));
    }
}
