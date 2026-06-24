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
