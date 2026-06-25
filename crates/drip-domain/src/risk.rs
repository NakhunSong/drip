//! Pre-trade risk checks — a pure backstop applied to every order intent before it is sent
//! to a live broker. It deliberately does not second-guess the strategy; it catches only
//! gross errors (selling more than is held, an order sized or priced orders-of-magnitude
//! off the mark) that signal a bug or a fat-finger, never normal strategy variation.

use crate::error::{DomainError, Result};
use crate::market::Side;
use crate::money::Price;
use crate::order::OrderIntent;
use crate::position::Position;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A single buy order may not exceed the per-tranche budget by more than this multiple.
/// Infinite-buying sizes every buy with `shares_affordable`, so a legitimate buy never
/// exceeds one daily budget; 3× is generous headroom that still catches a 10× slip.
const MAX_BUY_BUDGET_MULTIPLE: Decimal = dec!(3);

/// A limit price must stay within this multiple of the reference quote in either
/// direction. The ladder legitimately prices well above or below the last quote, so the
/// band is wide (10×) — calibrated to catch a misplaced decimal point, not the strategy.
const MAX_PRICE_DEVIATION_MULTIPLE: Decimal = dec!(10);

/// Vet a single order intent against the position ledger and a reference price, returning
/// `Err(DomainError::Risk)` on any violation so the caller can refuse to place it.
pub fn vet(intent: &OrderIntent, position: &Position, reference: Price) -> Result<()> {
    if intent.is_noop() {
        return Err(DomainError::Risk(format!(
            "{}: zero-share order",
            intent.tag
        )));
    }

    // A sell can never exceed the held quantity (the ledger's view of the holding).
    if intent.side == Side::Sell && intent.shares.get() > position.shares.get() {
        return Err(DomainError::Risk(format!(
            "{}: sell {} exceeds holding {}",
            intent.tag, intent.shares, position.shares
        )));
    }

    if let Some(limit) = intent.limit {
        // Notional sanity for buys: never spend many tranche budgets in a single order.
        if intent.side == Side::Buy {
            let notional = limit.total(intent.shares).amount();
            let cap = position.daily_budget().amount() * MAX_BUY_BUDGET_MULTIPLE;
            if notional > cap {
                return Err(DomainError::Risk(format!(
                    "{}: buy notional {notional} exceeds {MAX_BUY_BUDGET_MULTIPLE}× tranche budget {cap}",
                    intent.tag
                )));
            }
        }

        // Price sanity: a limit must stay within a 10× band of the order's anchor — the
        // average price while holding, else the reference quote. The infinite-buying ladder
        // legitimately prices off the average (take-profits above it, buys at/below it), so
        // anchoring the band there avoids false rejections in a deep drawdown where the quote
        // has fallen far below the average, while still catching a misplaced decimal. `>=`
        // catches the classic 10.0× slip exactly.
        let anchor = position.avg_price.unwrap_or(reference).value();
        let limit = limit.value();
        if limit >= anchor * MAX_PRICE_DEVIATION_MULTIPLE
            || limit * MAX_PRICE_DEVIATION_MULTIPLE <= anchor
        {
            return Err(DomainError::Risk(format!(
                "{tag}: limit {limit} deviates {MAX_PRICE_DEVIATION_MULTIPLE}× or more from anchor {anchor}",
                tag = intent.tag
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::{AccountId, BrokerId, Ticker};
    use crate::money::{Money, Shares};

    fn position() -> Position {
        // seed 32000 / 40 = 800 daily budget; 10 shares held at avg 100.
        let mut p = Position::new(
            AccountId::new("kis-paper"),
            BrokerId::Kis,
            Ticker::new("TQQQ"),
            Money::new(dec!(32000)),
            40,
        );
        p.shares = Shares::new(10);
        p.avg_price = Price::new(dec!(100));
        p
    }
    fn price(p: Decimal) -> Price {
        Price::new(p).unwrap()
    }

    #[test]
    fn accepts_a_normal_buy() {
        let intent = OrderIntent::loc(Side::Buy, Shares::new(4), price(dec!(100)), "loc_low");
        assert!(vet(&intent, &position(), price(dec!(100))).is_ok());
    }

    #[test]
    fn accepts_a_sell_within_the_holding() {
        let intent = OrderIntent::limit(Side::Sell, Shares::new(7), price(dec!(115)), "tp_rest");
        assert!(vet(&intent, &position(), price(dec!(110))).is_ok());
    }

    #[test]
    fn rejects_a_sell_exceeding_the_holding() {
        let intent = OrderIntent::limit(Side::Sell, Shares::new(11), price(dec!(115)), "tp_rest");
        assert!(matches!(
            vet(&intent, &position(), price(dec!(110))),
            Err(DomainError::Risk(_))
        ));
    }

    #[test]
    fn rejects_an_oversized_buy_notional() {
        // 100 shares @ 100 = 10_000 notional vs 800 budget × 3 = 2_400 cap.
        let intent = OrderIntent::loc(Side::Buy, Shares::new(100), price(dec!(100)), "loc_low");
        assert!(matches!(
            vet(&intent, &position(), price(dec!(100))),
            Err(DomainError::Risk(_))
        ));
    }

    #[test]
    fn rejects_a_fat_finger_price() {
        // A misplaced decimal: limit 1000 against a 100 reference (exactly 10×) is rejected.
        let intent = OrderIntent::limit(Side::Sell, Shares::new(1), price(dec!(1000)), "tp_rest");
        assert!(matches!(
            vet(&intent, &position(), price(dec!(100))),
            Err(DomainError::Risk(_))
        ));
    }

    #[test]
    fn rejects_a_noop() {
        let intent = OrderIntent::loc(Side::Buy, Shares::ZERO, price(dec!(100)), "loc_low");
        assert!(matches!(
            vet(&intent, &position(), price(dec!(100))),
            Err(DomainError::Risk(_))
        ));
    }

    #[test]
    fn accepts_a_deep_drawdown_take_profit_anchored_on_average() {
        // avg 100 but the quote has crashed to 9 (a 3× ETF down ~90%): the take-profit at
        // avg × 1.15 = 115 deviates >10× from the quote yet is anchored on the average, so it
        // must still pass — vetting against the quote here would wrongly abort the whole tick.
        let intent = OrderIntent::limit(Side::Sell, Shares::new(7), price(dec!(115)), "tp_rest");
        assert!(vet(&intent, &position(), price(dec!(9))).is_ok());
    }
}
