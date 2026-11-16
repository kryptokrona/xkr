//! The route handlers (`src/wallet_api/api_dispatcher.cpp`, one function per route).
//!
//! A handler answers with a [`Reply`] or fails with a [`Fail`]: a wallet error (400 with
//! `{errorCode, errorMessage}`), a missing or mistyped parameter (400, empty body) or an
//! unexpected condition (500, empty body), as `ApiDispatcher::middleware` maps C++ `Error`s,
//! `json::exception`s and other exceptions.

pub mod addresses;
pub mod balance;
pub mod keys;
pub mod node;
pub mod status;
pub mod transactions;
pub mod wallet;

use serde_json::Value;
use xkr_wallet::{ErrorCode, Wallet, WalletError};

use crate::params::BadParam;
use crate::respond::Reply;
use crate::server::{App, NodeFactory, Route};

/// Why a handler did not answer normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// A C++ `Error`: 400 with the error body.
    Wallet(WalletError),
    /// A missing or mistyped parameter: 400, empty body.
    Param,
    /// Any other exception: 500, empty body.
    Internal,
}

impl Fail {
    /// The answer.
    #[must_use]
    pub fn reply(self) -> Reply {
        match self {
            Self::Wallet(e) => Reply::error(&e),
            Self::Param => Reply::empty(400),
            Self::Internal => Reply::empty(500),
        }
    }
}

impl From<WalletError> for Fail {
    fn from(e: WalletError) -> Self {
        Self::Wallet(e)
    }
}

impl From<ErrorCode> for Fail {
    fn from(code: ErrorCode) -> Self {
        Self::Wallet(code.into())
    }
}

impl From<BadParam> for Fail {
    fn from(_: BadParam) -> Self {
        Self::Param
    }
}

/// A handler's result.
pub type Answer = Result<Reply, Fail>;

/// `ADDRESS_NOT_IN_WALLET` with the default text, as `SubWallets::getPrivateSpendKey` and
/// `SubWallets::deleteSubWallet` return it (the engine's address check adds the address to the
/// text, as `validateOurAddresses` does for the other routes).
pub(crate) fn plain_not_in_wallet(e: WalletError) -> WalletError {
    if e.code() == ErrorCode::AddressNotInWallet {
        WalletError::new(ErrorCode::AddressNotInWallet)
    } else {
        e
    }
}

/// Runs the handler of a route that needs an open wallet.
pub(crate) async fn run<N: NodeFactory>(
    app: &App<N>,
    route: &Route,
    wallet: &Wallet<N::Transport>,
    body: &Value,
) -> Reply {
    let answer = match route {
        Route::AddressCreate => addresses::create(wallet).await,
        Route::AddressImport => addresses::import(wallet, body).await,
        Route::AddressImportView => addresses::import_view(wallet, body).await,
        Route::AddressDelete(address) => addresses::delete(wallet, address).await,
        Route::Addresses => Ok(addresses::list(wallet)),
        Route::PrimaryAddress => Ok(addresses::primary(wallet)),
        Route::IntegratedAddress(address, payment_id) => addresses::integrated(address, payment_id),
        Route::SendBasic => transactions::send_basic(wallet, body).await,
        Route::SendAdvanced => transactions::send_advanced(wallet, body).await,
        Route::FusionBasic => transactions::fusion_basic(wallet).await,
        Route::FusionAdvanced => transactions::fusion_advanced(wallet, body).await,
        Route::Transactions => transactions::all(wallet),
        Route::Unconfirmed => transactions::unconfirmed(wallet),
        Route::UnconfirmedFor(address) => transactions::unconfirmed_for(wallet, address),
        Route::TransactionsFrom(start) => transactions::from_height(wallet, None, start),
        Route::TransactionsRange(start, end) => transactions::range(wallet, None, start, end),
        Route::AddressTransactionsFrom(address, start) => {
            transactions::from_height(wallet, Some(address), start)
        }
        Route::AddressTransactionsRange(address, start, end) => {
            transactions::range(wallet, Some(address), start, end)
        }
        Route::TxPrivateKey(hash) => transactions::private_key(wallet, hash),
        Route::TransactionByHash(hash) => transactions::by_hash(wallet, hash),
        Route::Balance => balance::total(wallet),
        Route::BalanceFor(address) => balance::for_address(wallet, address),
        Route::Balances => Ok(balance::balances(wallet)),
        Route::ViewKey => Ok(keys::view_key(wallet)),
        Route::SpendKeys(address) => keys::spend_keys(wallet, address),
        Route::Mnemonic(address) => keys::mnemonic(wallet, address),
        Route::Status => Ok(status::status(wallet)),
        Route::NodeGet => Ok(node::get(wallet)),
        Route::NodeSet => node::set(app, wallet, body).await,
        Route::Save => Ok(node::save(wallet).await),
        Route::Reset => node::reset(wallet, body).await,
        // routes of the other access kinds never get here
        Route::WalletOpen
        | Route::ImportKey
        | Route::ImportSeed
        | Route::ImportView
        | Route::WalletCreate
        | Route::WalletClose
        | Route::Options => Err(Fail::Internal),
    };
    answer.unwrap_or_else(Fail::reply)
}
