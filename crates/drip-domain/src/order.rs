//! Broker-neutral order intents — what a strategy *wants* done, before any broker
//! translates it into a wire-format order.

use crate::market::Side;
use crate::money::{Price, Shares};
use serde::Serialize;

/// Execution style of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    /// Limit-on-close: settles only at the official close, and only if the close is at or
    /// better than `limit`. This is the engine of the infinite-buying cost-averaging ladder.
    LimitOnClose,
    /// A plain day limit order.
    Limit,
    /// A market order, sized in shares.
    Market,
}

/// A single action a strategy wants taken today, expressed independently of any broker.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderIntent {
    pub side: Side,
    pub kind: OrderKind,
    pub shares: Shares,
    /// Limit price; `None` only for [`OrderKind::Market`].
    pub limit: Option<Price>,
    /// Diagnostic label, e.g. `"loc_low"` / `"loc_high"` / `"tp_quarter"` / `"tp_rest"`.
    pub tag: &'static str,
}

impl OrderIntent {
    pub fn loc(side: Side, shares: Shares, limit: Price, tag: &'static str) -> OrderIntent {
        OrderIntent {
            side,
            kind: OrderKind::LimitOnClose,
            shares,
            limit: Some(limit),
            tag,
        }
    }
    pub fn limit(side: Side, shares: Shares, limit: Price, tag: &'static str) -> OrderIntent {
        OrderIntent {
            side,
            kind: OrderKind::Limit,
            shares,
            limit: Some(limit),
            tag,
        }
    }
    pub fn market(side: Side, shares: Shares, tag: &'static str) -> OrderIntent {
        OrderIntent {
            side,
            kind: OrderKind::Market,
            shares,
            limit: None,
            tag,
        }
    }
    /// True if this intent is a no-op (zero shares) and can be dropped before sending.
    pub fn is_noop(&self) -> bool {
        self.shares.is_zero()
    }
}
