//! Request parameters as the C++ handlers read them (`src/wallet_api/api_dispatcher.h`
//! `tryGetJsonValue`, nlohmann `json::get<T>` of the bundled nlohmann 3.2.0).
//!
//! * The body is parsed as JSON when it is JSON; any other body (or none) reads as `null`, and
//!   every lookup in a non-object finds nothing (`ApiDispatcher::middleware`, `json::find`).
//! * A missing or mistyped required parameter is [`BadParam`]: HTTP 400 with an empty body.
//! * Numbers convert like `static_cast`: a negative integer given for a `uint64_t` wraps, a
//!   fraction is cut off; a `uint16_t` (`daemonPort`) also takes `true` and `false`, a
//!   `uint64_t` does not (nlohmann picks a different `from_json` for `number_unsigned_t`).
//! * Keys and hashes are strings of exactly 64 hex characters (`common::podFromHex`).

use std::str::FromStr;

use serde_json::{Number, Value};

/// A required parameter is missing or has the wrong type (C++ `json::exception` caught in
/// `ApiDispatcher::middleware`: 400, empty body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadParam;

/// The request body as the middleware sees it: the parsed JSON, or `null`.
#[must_use]
pub fn parse_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// `body.find(key) != body.end()`.
#[must_use]
pub fn has(body: &Value, key: &str) -> bool {
    body.get(key).is_some()
}

/// The value at `key` (`tryGetJsonValue<json>`).
pub fn value<'a>(body: &'a Value, key: &str) -> Result<&'a Value, BadParam> {
    body.get(key).ok_or(BadParam)
}

/// `tryGetJsonValue<std::string>`.
pub fn string(body: &Value, key: &str) -> Result<String, BadParam> {
    value(body, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or(BadParam)
}

/// An optional string: `None` when the key is absent, an error when it is not a string.
pub fn opt_string(body: &Value, key: &str) -> Result<Option<String>, BadParam> {
    if has(body, key) {
        string(body, key).map(Some)
    } else {
        Ok(None)
    }
}

/// `static_cast<uint64_t>` of a JSON number.
fn number_to_u64(n: &Number) -> u64 {
    if let Some(u) = n.as_u64() {
        u
    } else if let Some(i) = n.as_i64() {
        i as u64
    } else {
        n.as_f64().map_or(0, |f| f as u64)
    }
}

/// `json::get<uint64_t>`.
pub fn as_u64(v: &Value) -> Result<u64, BadParam> {
    match v {
        Value::Number(n) => Ok(number_to_u64(n)),
        _ => Err(BadParam),
    }
}

/// `json::get<uint16_t>`.
pub fn as_u16(v: &Value) -> Result<u16, BadParam> {
    match v {
        Value::Number(n) => Ok(number_to_u64(n) as u16),
        Value::Bool(b) => Ok(u16::from(*b)),
        _ => Err(BadParam),
    }
}

/// `tryGetJsonValue<uint64_t>`.
pub fn u64_param(body: &Value, key: &str) -> Result<u64, BadParam> {
    as_u64(value(body, key)?)
}

/// An optional `uint64_t`: `None` when absent.
pub fn opt_u64(body: &Value, key: &str) -> Result<Option<u64>, BadParam> {
    if has(body, key) {
        u64_param(body, key).map(Some)
    } else {
        Ok(None)
    }
}

/// `tryGetJsonValue<uint16_t>`.
pub fn u16_param(body: &Value, key: &str) -> Result<u16, BadParam> {
    as_u16(value(body, key)?)
}

/// `tryGetJsonValue<crypto::SecretKey>` / `<crypto::PublicKey>`: a string of 64 hex
/// characters (`from_json` in `include/crypto_types.h`).
pub fn key<T: FromStr>(body: &Value, key: &str) -> Result<T, BadParam> {
    let text = value(body, key)?.as_str().ok_or(BadParam)?;
    if text.len() != 64 {
        return Err(BadParam);
    }
    text.parse().map_err(|_| BadParam)
}

/// `tryGetJsonValue<std::vector<std::string>>`: an array of strings.
pub fn strings(body: &Value, key: &str) -> Result<Vec<String>, BadParam> {
    value(body, key)?
        .as_array()
        .ok_or(BadParam)?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or(BadParam))
        .collect()
}

/// An optional string array: empty when absent.
pub fn opt_strings(body: &Value, key: &str) -> Result<Vec<String>, BadParam> {
    if has(body, key) {
        strings(body, key)
    } else {
        Ok(Vec::new())
    }
}

/// What a range-for over a nlohmann value visits: nothing for `null`, the elements of an array,
/// the values of an object (in key order), the value itself for anything else
/// (`sendAdvancedTransaction` iterates `destinations` this way).
#[must_use]
pub fn elements(v: &Value) -> Vec<&Value> {
    match v {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            keys.into_iter().map(|k| &map[k]).collect()
        }
        other => vec![other],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xkr_types::SecretKey;

    #[test]
    fn bodies() {
        assert_eq!(parse_body(b""), Value::Null);
        assert_eq!(parse_body(b"amount=1000"), Value::Null);
        assert_eq!(parse_body(b"{} x"), Value::Null);
        assert_eq!(parse_body(br#"{"a":1}"#), json!({"a": 1}));
        assert!(!has(&Value::Null, "a"));
        assert!(!has(&json!([1]), "a"));
        assert_eq!(string(&json!({"a": 1}), "a"), Err(BadParam));
        assert_eq!(string(&json!({"a": "x"}), "a"), Ok("x".to_owned()));
        assert_eq!(opt_string(&json!({}), "a"), Ok(None));
        assert_eq!(opt_string(&json!({"a": null}), "a"), Err(BadParam));
    }

    #[test]
    fn numbers_convert_like_static_cast() {
        assert_eq!(as_u64(&json!(5)), Ok(5));
        assert_eq!(as_u64(&json!(-5)), Ok(u64::MAX - 4));
        assert_eq!(as_u64(&json!(1.5)), Ok(1));
        assert_eq!(as_u64(&json!(true)), Err(BadParam));
        assert_eq!(as_u64(&json!("5")), Err(BadParam));
        assert_eq!(as_u16(&json!(70_000)), Ok(4464));
        assert_eq!(as_u16(&json!(-1)), Ok(u16::MAX));
        assert_eq!(as_u16(&json!(true)), Ok(1));
        assert_eq!(as_u16(&json!(null)), Err(BadParam));
    }

    #[test]
    fn keys_are_64_hex_characters() {
        let hex = "ab".repeat(32);
        let k: SecretKey = key(&json!({ "k": hex }), "k").unwrap();
        assert_eq!(k, SecretKey([0xab; 32]));
        let upper = "AB".repeat(32);
        assert!(key::<SecretKey>(&json!({ "k": upper }), "k").is_ok());
        assert!(key::<SecretKey>(&json!({"k": "zz"}), "k").is_err());
        assert!(key::<SecretKey>(&json!({"k": "ab".repeat(31)}), "k").is_err());
        assert!(key::<SecretKey>(&json!({"k": 5}), "k").is_err());
    }

    #[test]
    fn iteration_like_nlohmann() {
        assert!(elements(&Value::Null).is_empty());
        assert_eq!(elements(&json!([1, 2])).len(), 2);
        assert_eq!(
            elements(&json!({"b": 2, "a": 1})),
            vec![&json!(1), &json!(2)]
        );
        assert_eq!(elements(&json!("x")), vec![&json!("x")]);
        assert_eq!(
            strings(&json!({"s": ["a", "b"]}), "s"),
            Ok(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(strings(&json!({"s": ["a", 1]}), "s"), Err(BadParam));
        assert_eq!(opt_strings(&json!({}), "s"), Ok(Vec::new()));
    }
}
