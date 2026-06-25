//! Broker-neutral order intents — what a strategy *wants* done, before any broker
//! translates it into a wire-format order.

use crate::market::{AccountId, Side, Ticker};
use crate::money::{Price, Shares};
use serde::Serialize;
use time::Date;

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

    /// A stable idempotency key for this intent on a given trading day. It relies on the
    /// strategy emitting at most one intent per `tag` per day — true for infinite-buying,
    /// where each of `loc_low`/`loc_high`/`tp_rest`/`tp_quarter`/`quarter_stop` appears at
    /// most once. Re-running `drip tick` the same day reproduces identical keys, which the
    /// [`crate::OrderJournal`] uses to guarantee at-most-once placement.
    ///
    /// Keyed by [`AccountId`], not broker: 모의 and 실전 are different accounts on the same
    /// broker and ticker, so a paper order must not suppress the real order for the same leg
    /// (and vice versa). The account namespace keeps their keys distinct.
    pub fn client_key(&self, account: &AccountId, ticker: &Ticker, date: Date) -> String {
        format!("{account}:{ticker}:{date}:{}", self.tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{Price, Shares};
    use rust_decimal_macros::dec;
    use time::macros::date;

    #[test]
    fn client_key_is_scoped_by_account() {
        let intent = OrderIntent::loc(
            Side::Buy,
            Shares::new(1),
            Price::new(dec!(188.475)).unwrap(),
            "loc_low",
        );
        let ticker = Ticker::new("122630");
        let day = date!(2026 - 06 - 25);
        let paper = intent.client_key(&AccountId::new("kis-paper"), &ticker, day);
        let real = intent.client_key(&AccountId::new("kis-real"), &ticker, day);
        // Different accounts on the same broker / ticker / day / leg → different keys, so a 모의
        // placement never suppresses the 실전 order for the same leg (read as "already placed"),
        // nor the reverse. This is the over-buy guard's twin for the paper/real boundary.
        assert_ne!(paper, real);
        assert_eq!(paper, "kis-paper:122630:2026-06-25:loc_low");
        assert_eq!(real, "kis-real:122630:2026-06-25:loc_low");
    }
}
