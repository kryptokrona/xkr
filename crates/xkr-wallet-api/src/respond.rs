//! Answers as the C++ wallet-api writes them (`src/wallet_api/api_dispatcher.cpp`).
//!
//! * JSON bodies are `j.dump(4) + "\n"` with `Content-Type: application/json`
//!   (`res.set_content(j.dump(4) + "\n", "application/json")` in every handler): nlohmann
//!   objects keep their keys sorted, four spaces of indentation, `": "` after a key, empty arrays
//!   and objects as `[]` and `{}`, and only control characters escaped ([`dump`]).
//! * A handler error is HTTP 400 with `{"errorCode": n, "errorMessage": "..."}`
//!   (`ApiDispatcher::middleware`).
//! * Everything else without a body: `Content-Length: 0` and no `Content-Type`.

use std::fmt::Write as _;

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode, header};
use serde_json::{Value, json};
use xkr_wallet::WalletError;

/// An answer before it becomes an HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The formatted JSON text, `None` for an empty body.
    pub body: Option<String>,
    /// Headers the handler sets itself (`ApiDispatcher::handleOptions`).
    pub headers: Vec<(&'static str, String)>,
}

impl Reply {
    /// Status only, no body (`res.status = n` without `set_content`).
    #[must_use]
    pub const fn empty(status: u16) -> Self {
        Self {
            status,
            body: None,
            headers: Vec::new(),
        }
    }

    /// `res.set_content(j.dump(4) + "\n", "application/json")` with `status`.
    #[must_use]
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            body: Some(dump(value)),
            headers: Vec::new(),
        }
    }

    /// 400 with `{"errorCode", "errorMessage"}` (`ApiDispatcher::middleware`).
    #[must_use]
    pub fn error(error: &WalletError) -> Self {
        Self::json(400, &error_body(error))
    }

    /// Adds a header.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// The HTTP response; `cors` is the `--enable-cors` value the middleware adds as
    /// `Access-Control-Allow-Origin` (C++ sets it before the authentication check, so 401 and
    /// 403 answers carry it too).
    #[must_use]
    pub fn into_response(self, cors: Option<&str>) -> Response<Body> {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = match self.body {
            Some(text) => {
                let mut r = Response::new(Body::from(text));
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                r
            }
            None => Response::new(Body::empty()),
        };
        *response.status_mut() = status;
        let headers = response.headers_mut();
        for (name, value) in self.headers {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(HeaderName::from_static(name), value);
            }
        }
        if let Some(origin) = cors
            && let Ok(value) = HeaderValue::from_str(origin)
        {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
        response
    }
}

/// `{"errorCode": n, "errorMessage": "..."}` with the C++ number and text.
#[must_use]
pub fn error_body(error: &WalletError) -> Value {
    json!({
        "errorCode": error.number(),
        "errorMessage": error.message(),
    })
}

/// nlohmann `json::dump(4)` followed by `"\n"`.
#[must_use]
pub fn dump(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out.push('\n');
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                write_value(out, item, depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            // nlohmann objects are a std::map: keys in byte order
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push_str("{\n");
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                indent(out, depth + 1);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, &map[key], depth + 1);
            }
            out.push('\n');
            indent(out, depth);
            out.push('}');
        }
    }
}

/// nlohmann `serializer::dump_escaped` without `ensure_ascii`: `"`, `\`, the five short escapes,
/// other control characters as `\u00xx`; everything else as is.
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if u32::from(c) <= 0x1f => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use xkr_wallet::ErrorCode;

    #[test]
    fn dump_matches_nlohmann_dump_4() {
        let v = json!({
            "transactions": [],
            "b": {},
            "a": [1, -2, {"z": true, "y": null}],
            "s": "x\"\\\n\u{1}é/",
        });
        let want = "{\n    \"a\": [\n        1,\n        -2,\n        {\n            \"y\": null,\n            \"z\": true\n        }\n    ],\n    \"b\": {},\n    \"s\": \"x\\\"\\\\\\n\\u0001é/\",\n    \"transactions\": []\n}\n";
        assert_eq!(dump(&v), want);
        assert_eq!(dump(&Value::Null), "null\n");
        assert_eq!(dump(&json!([])), "[]\n");
        assert_eq!(dump(&json!(u64::MAX)), "18446744073709551615\n");
    }

    #[test]
    fn error_bodies() {
        let e = WalletError::new(ErrorCode::WrongPassword);
        let r = Reply::error(&e);
        assert_eq!(r.status, 400);
        assert_eq!(
            r.body.as_deref(),
            Some(
                "{\n    \"errorCode\": 5,\n    \"errorMessage\": \"The password given for this wallet is incorrect.\"\n}\n"
            )
        );
    }

    #[test]
    fn responses_carry_type_and_cors() {
        let r = Reply::json(201, &json!({"a": 1})).into_response(Some("*"));
        assert_eq!(r.status(), 201);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(r.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let r = Reply::empty(403).into_response(None);
        assert!(r.headers().get(header::CONTENT_TYPE).is_none());
        assert!(
            r.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }
}
