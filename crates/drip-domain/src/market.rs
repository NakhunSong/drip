//! Market identifiers and observations.

use crate::error::DomainError;
use crate::money::Price;
use serde::{Deserialize, Serialize};
use std::fmt;
use time::Date;

/// A tradable symbol, normalized to uppercase (e.g. `TQQQ`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ticker(String);

impl Ticker {
    pub fn new(symbol: impl Into<String>) -> Ticker {
        Ticker(symbol.into().trim().to_uppercase())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Ticker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which configured account a position trades under — the isolation namespace for its ledger,
/// its order-journal keys, and its stored credentials. For KIS this separates 모의 from 실전
/// (e.g. `kis-paper` vs `kis-real`): different accounts, different money. Keeping it in the
/// state and journal keys is what stops a real position from inheriting a paper ledger, or a
/// paper order key from suppressing the real order on the same ticker. The broker (adapter) is
/// orthogonal — one account can trade both the overseas and the domestic KIS adapter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(name: impl Into<String>) -> AccountId {
        AccountId(name.into().trim().to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The secret-store key for one of an account's credential fields — e.g.
    /// (`kis-paper`, `app_key`) → `kis-paper_app_key`. The single owner of the credential-key
    /// namespacing scheme, so the broker connection (the reader) and the CLI / migration (the
    /// writers) cannot drift on the separator across crates. Underscore, never a dot — dots nest
    /// in TOML, which would break the flat secret store.
    pub fn secret_key(account: &str, field: &str) -> String {
        format!("{account}_{field}")
    }
}
impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which broker an order or position belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerId {
    Kis,
    Toss,
    Paper,
}
impl fmt::Display for BrokerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BrokerId::Kis => "kis",
            BrokerId::Toss => "toss",
            BrokerId::Paper => "paper",
        })
    }
}
impl std::str::FromStr for BrokerId {
    type Err = DomainError;
    fn from_str(s: &str) -> Result<BrokerId, DomainError> {
        match s {
            "kis" => Ok(BrokerId::Kis),
            // The domestic KIS adapter is the same broker (and shares the journal id) as overseas
            // KIS; the position's broker *string* ("kis-domestic") is what routes `connect` to it.
            "kis-domestic" => Ok(BrokerId::Kis),
            "toss" => Ok(BrokerId::Toss),
            "paper" => Ok(BrokerId::Paper),
            other => Err(DomainError::Config(format!(
                "unknown broker '{other}' (use kis|kis-domestic|toss|paper)"
            ))),
        }
    }
}

/// An opaque, broker-assigned order identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(String);

impl OrderId {
    pub fn new(id: impl Into<String>) -> OrderId {
        OrderId(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Buy or sell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

/// A latest-price observation, used to size orders before the close.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub ticker: Ticker,
    pub price: Price,
    pub as_of: Date,
}

/// One daily OHLC bar — the unit of backtest data; the `close` settles LOC orders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub date: Date,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
}

/// What a strategy observes for a ticker on a given trading day.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketSnapshot {
    pub ticker: Ticker,
    /// Reference price for sizing (a latest quote intraday, or a bar's close in backtest).
    pub price: Price,
    pub as_of: Date,
}
