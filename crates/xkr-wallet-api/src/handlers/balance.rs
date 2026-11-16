//! Balances (`ApiDispatcher::getBalance`, `getBalanceForAddress`, `getBalances`).

use serde_json::{Value, json};
use xkr_daemon_client::Transport;
use xkr_wallet::{AddressBalance, Wallet};

use super::Answer;
use crate::respond::Reply;

/// GET `/balance`: `{unlocked, locked}` of the whole wallet.
pub fn total<T: Transport + 'static>(wallet: &Wallet<T>) -> Answer {
    let b = wallet.balance(None)?;
    Ok(Reply::json(
        200,
        &json!({ "unlocked": b.unlocked, "locked": b.locked }),
    ))
}

/// GET `/balance/{address}`: `{unlocked, locked}` of one subwallet; the address must be one of
/// the wallet's (`ADDRESS_NOT_IN_WALLET` names it).
pub fn for_address<T: Transport + 'static>(wallet: &Wallet<T>, address: &str) -> Answer {
    let b = wallet.balance(Some(address))?;
    Ok(Reply::json(
        200,
        &json!({ "unlocked": b.unlocked, "locked": b.locked }),
    ))
}

/// GET `/balances`: a bare array `[{address, unlocked, locked}]`.
#[must_use]
pub fn balances<T: Transport + 'static>(wallet: &Wallet<T>) -> Reply {
    Reply::json(200, &balances_json(&wallet.balances()))
}

/// The `/balances` body. C++ pushes onto a default (`null`) nlohmann value, so an empty list is
/// `null`, not `[]`.
#[must_use]
pub fn balances_json(balances: &[AddressBalance]) -> Value {
    if balances.is_empty() {
        return Value::Null;
    }
    Value::Array(
        balances
            .iter()
            .map(|b| json!({ "address": b.address, "unlocked": b.unlocked, "locked": b.locked }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::respond::dump;

    #[test]
    fn an_empty_list_is_null() {
        assert_eq!(dump(&balances_json(&[])), "null\n");
        let one = [AddressBalance {
            address: "SEKRx".into(),
            unlocked: 5,
            locked: 1,
        }];
        assert_eq!(
            dump(&balances_json(&one)),
            "[\n    {\n        \"address\": \"SEKRx\",\n        \"locked\": 1,\n        \"unlocked\": 5\n    }\n]\n"
        );
    }
}
