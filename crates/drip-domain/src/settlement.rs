//! Settlement rules: how an order intent becomes a confirmed fill against a daily bar.
//!
//! Pure and deterministic, shared by the backtest engine (`drip-app`) and the paper
//! broker (`drip-brokers`) so both agree exactly on fill semantics.

use crate::market::{Bar, Side};
use crate::order::{OrderIntent, OrderKind};
use crate::position::Fill;

/// Settle an intent against a day's bar, returning a [`Fill`] if it would execute.
///
/// * **Limit-on-close** settles at the close, and only if the close is at or better than
///   the limit (buy: `close ≤ limit`, sell: `close ≥ limit`); the fill price is the close.
/// * **Limit** (a day order) fills if the day's range reaches the limit (buy: `low ≤ limit`,
///   sell: `high ≥ limit`); the fill price is the limit.
/// * **Market** fills at the close.
///
/// Returns `None` for a no-op (zero-share) intent or one that does not execute.
pub fn settle(intent: &OrderIntent, bar: &Bar) -> Option<Fill> {
    if intent.is_noop() {
        return None;
    }
    let fill_price = match (intent.kind, intent.side) {
        (OrderKind::LimitOnClose, Side::Buy) => (bar.close <= intent.limit?).then_some(bar.close),
        (OrderKind::LimitOnClose, Side::Sell) => (bar.close >= intent.limit?).then_some(bar.close),
        (OrderKind::Limit, Side::Buy) => {
            let limit = intent.limit?;
            (bar.low <= limit).then_some(limit)
        }
        (OrderKind::Limit, Side::Sell) => {
            let limit = intent.limit?;
            (bar.high >= limit).then_some(limit)
        }
        (OrderKind::Market, _) => Some(bar.close),
    }?;
    Some(Fill {
        side: intent.side,
        shares: intent.shares,
        price: fill_price,
        at: bar.date,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{Price, Shares};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use time::macros::date;

    fn bar(open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> Bar {
        Bar {
            date: date!(2026 - 01 - 15),
            open: Price::new(open).unwrap(),
            high: Price::new(high).unwrap(),
            low: Price::new(low).unwrap(),
            close: Price::new(close).unwrap(),
        }
    }

    fn price(p: Decimal) -> Price {
        Price::new(p).unwrap()
    }

    #[test]
    fn loc_buy_fills_at_close_when_close_at_or_below_limit() {
        let b = bar(dec!(100), dec!(103), dec!(99), dec!(100));
        let intent = OrderIntent::loc(Side::Buy, Shares::new(5), price(dec!(100)), "loc_low");
        let fill = settle(&intent, &b).unwrap();
        assert_eq!(fill.price, price(dec!(100)));
        assert_eq!(fill.shares, Shares::new(5));
    }

    #[test]
    fn loc_buy_does_not_fill_when_close_above_limit() {
        let b = bar(dec!(100), dec!(103), dec!(99), dec!(102));
        let intent = OrderIntent::loc(Side::Buy, Shares::new(5), price(dec!(100)), "loc_low");
        assert!(settle(&intent, &b).is_none());
    }

    #[test]
    fn loc_sell_fills_at_close_when_close_at_or_above_limit() {
        let b = bar(dec!(110), dec!(116), dec!(109), dec!(115));
        let intent = OrderIntent::loc(Side::Sell, Shares::new(2), price(dec!(113)), "tp_quarter");
        let fill = settle(&intent, &b).unwrap();
        assert_eq!(fill.price, price(dec!(115)));
    }

    #[test]
    fn limit_sell_fills_at_limit_when_high_reaches_it() {
        let b = bar(dec!(110), dec!(116), dec!(109), dec!(112));
        let intent = OrderIntent::limit(Side::Sell, Shares::new(7), price(dec!(115)), "tp_rest");
        let fill = settle(&intent, &b).unwrap();
        // fills at the limit, not the close
        assert_eq!(fill.price, price(dec!(115)));
    }

    #[test]
    fn limit_sell_does_not_fill_when_high_below_limit() {
        let b = bar(dec!(110), dec!(114), dec!(109), dec!(112));
        let intent = OrderIntent::limit(Side::Sell, Shares::new(7), price(dec!(115)), "tp_rest");
        assert!(settle(&intent, &b).is_none());
    }

    #[test]
    fn noop_intent_never_fills() {
        let b = bar(dec!(100), dec!(103), dec!(99), dec!(100));
        let intent = OrderIntent::loc(Side::Buy, Shares::ZERO, price(dec!(100)), "loc_low");
        assert!(settle(&intent, &b).is_none());
    }
}
