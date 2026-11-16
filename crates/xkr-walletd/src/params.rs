//! Reading method parameters the way C++ walletd does (`src/serialization/
//! json_input_value_serializer.cpp` `JsonInputValueSerializer` driven by the `serialize`
//! functions of `src/wallet_service/payment_service_json_rpc_messages.cpp`).
//!
//! A missing key reads as absent; a key that is present but has the wrong JSON type is an error
//! (`JsonValue::getString` and friends throw). Every such error, and every
//! `RequestSerializationError` of the message definitions, becomes the answer `-32600 "Invalid
//! Request"` ([`Invalid`]). Integers are C++ `int64_t` values cast to the field's type, so a
//! negative number wraps and a `uint32_t` field keeps the low 32 bits.

use serde_json::{Map, Value};

/// The parameters could not be read (`-32600 "Invalid Request"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// One `{address, amount}` of `transfers` (`WalletRpcOrder`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// Destination address (standard or integrated).
    pub address: String,
    /// Amount.
    pub amount: u64,
}

/// The `params` object of a request.
#[derive(Debug, Clone, Copy)]
pub struct Params<'a>(&'a Map<String, Value>);

/// `JsonValue::getInteger`: an integer (not a real number). A number beyond `int64_t` is
/// `INT64_MAX`, as `std::istream >> int64_t` leaves it.
fn integer(v: &Value) -> Result<i64, Invalid> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|_| i64::MAX))
            .ok_or(Invalid),
        _ => Err(Invalid),
    }
}

/// `JsonValue::getString`.
fn string(v: &Value) -> Result<String, Invalid> {
    v.as_str().map(str::to_owned).ok_or(Invalid)
}

impl<'a> Params<'a> {
    /// `JsonInputValueSerializer(params)`: the parameters must be an object.
    pub fn new(params: &'a Value) -> Result<Self, Invalid> {
        params.as_object().map(Self).ok_or(Invalid)
    }

    /// Whether `key` is present (with any value).
    #[must_use]
    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// A string field.
    pub fn string(&self, key: &str) -> Result<Option<String>, Invalid> {
        self.0.get(key).map(string).transpose()
    }

    /// A required string field.
    pub fn required_string(&self, key: &str) -> Result<String, Invalid> {
        self.string(key)?.ok_or(Invalid)
    }

    /// A signed 64-bit field.
    pub fn i64(&self, key: &str) -> Result<Option<i64>, Invalid> {
        self.0.get(key).map(integer).transpose()
    }

    /// A `uint64_t` field.
    pub fn u64(&self, key: &str) -> Result<Option<u64>, Invalid> {
        Ok(self.i64(key)?.map(|v| v as u64))
    }

    /// A `uint32_t` field (the low 32 bits, as C++ `static_cast<uint32_t>`).
    pub fn u32(&self, key: &str) -> Result<Option<u32>, Invalid> {
        Ok(self.i64(key)?.map(|v| v as u32))
    }

    /// A `bool` field.
    pub fn bool(&self, key: &str) -> Result<Option<bool>, Invalid> {
        self.0
            .get(key)
            .map(|v| v.as_bool().ok_or(Invalid))
            .transpose()
    }

    /// A `std::vector<std::string>` field.
    pub fn strings(&self, key: &str) -> Result<Option<Vec<String>>, Invalid> {
        self.0
            .get(key)
            .map(|v| match v {
                Value::Array(items) => items.iter().map(string).collect(),
                _ => Err(Invalid),
            })
            .transpose()
    }

    /// A `std::vector<WalletRpcOrder>` field: every order needs `address` and `amount`
    /// (`WalletRpcOrder::serialize`).
    pub fn orders(&self, key: &str) -> Result<Option<Vec<Order>>, Invalid> {
        self.0
            .get(key)
            .map(|v| match v {
                Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        let fields = Params::new(item)?;
                        let address = fields.string("address")?;
                        let amount = fields.u64("amount")?;
                        match (address, amount) {
                            (Some(address), Some(amount)) => Ok(Order { address, amount }),
                            _ => Err(Invalid),
                        }
                    })
                    .collect(),
                _ => Err(Invalid),
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn types_follow_the_cpp_serializer() {
        let v = json!({
            "s": "x", "n": 5, "neg": -1, "big": 4_294_967_301_u64, "huge": u64::MAX,
            "real": 1.5, "b": true, "list": ["a", "b"], "bad_list": ["a", 1],
            "orders": [{"address": "a", "amount": 3}], "order_no_amount": [{"address": "a"}],
            "null": null
        });
        let p = Params::new(&v).unwrap();
        assert_eq!(p.string("s").unwrap().as_deref(), Some("x"));
        assert_eq!(p.string("missing").unwrap(), None);
        assert_eq!(p.string("n"), Err(Invalid));
        assert_eq!(p.string("null"), Err(Invalid));
        assert_eq!(p.u64("n").unwrap(), Some(5));
        assert_eq!(p.u64("neg").unwrap(), Some(u64::MAX));
        assert_eq!(p.u32("big").unwrap(), Some(5));
        assert_eq!(p.i64("huge").unwrap(), Some(i64::MAX));
        assert_eq!(p.u64("real"), Err(Invalid));
        assert_eq!(p.u64("s"), Err(Invalid));
        assert_eq!(p.bool("b").unwrap(), Some(true));
        assert_eq!(p.bool("n"), Err(Invalid));
        assert_eq!(
            p.strings("list").unwrap(),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(p.strings("bad_list"), Err(Invalid));
        assert_eq!(p.strings("s"), Err(Invalid));
        assert_eq!(
            p.orders("orders").unwrap(),
            Some(vec![Order {
                address: "a".into(),
                amount: 3
            }])
        );
        assert_eq!(p.orders("order_no_amount"), Err(Invalid));
        assert!(p.has("null"));
        assert_eq!(Params::new(&json!([])).unwrap_err(), Invalid);
    }
}
