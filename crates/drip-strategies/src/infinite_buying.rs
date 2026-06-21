//! Laoer's "Infinite Buying" method — default version **v2.2**.
//!
//! The strategy is a pure decision function: given the position state and today's market
//! snapshot, it returns the day's order intents. It performs no I/O — the engine places
//! the orders and feeds confirmed fills back into the [`Position`] accounting.
//!
//! ## v2.2 daily rules (see `FOR_ONBOARDING.md` for the full derivation)
//! * Seed is divided into `splits` tranches (40); the daily budget is `seed / splits`.
//! * `T` (tranche counter) = round-up(cum_spent / daily_budget); the seed is exhausted at `T = splits`.
//! * **Buy** (while `T < splits`), anchored on the average price `A`:
//!   * First half (`T < half_boundary`): half the budget as an LOC buy at `A`, half as an LOC
//!     buy at the variable price `A × (1 + (var_base − var_slope·T)%)`.
//!   * Second half (`T ≥ half_boundary`): the whole budget as one LOC buy at the variable price.
//! * **Sell** (while holding): ¾ as a limit sell at the fixed take-profit `A × (1 + tp%)`,
//!   and ¼ as an LOC sell at the variable price.
//! * **Quarter stop-loss**: once the seed is exhausted (`T ≥ splits`) and the position is underwater,
//!   the ¼ leg becomes an LOC sell at the market price instead of the variable take-profit,
//!   so the day's total sell quantity never exceeds the holding.

use drip_domain::{DailyContext, OrderIntent, Percent, Position, Price, Side, Strategy};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Tunable parameters of the infinite-buying strategy. All percentage-like fields are
/// expressed as plain figures (e.g. `take_profit_pct = 15` means +15%).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InfiniteBuyingConfig {
    /// Number of tranches the seed is divided into (40 = v2.2, 20 = v3.0).
    pub splits: u32,
    /// Fixed take-profit for the ¾ leg, as a percentage (TQQQ 15, SOXL 20).
    pub take_profit_pct: Decimal,
    /// `T` threshold separating the first half from the second half.
    pub half_boundary: Decimal,
    /// Intercept of the variable price ladder `(var_base − var_slope·T)%`.
    pub var_base: Decimal,
    /// Slope of the variable price ladder.
    pub var_slope: Decimal,
    /// Fraction of the holding sold on the variable ("quarter") leg.
    pub sell_quarter: Decimal,
    /// Enable quarter stop-loss de-risking once the seed is exhausted.
    pub quarter_stop: bool,
}

impl Default for InfiniteBuyingConfig {
    fn default() -> Self {
        InfiniteBuyingConfig {
            splits: 40,
            take_profit_pct: dec!(15),
            half_boundary: dec!(20),
            var_base: dec!(15),
            var_slope: dec!(1.5),
            sell_quarter: dec!(0.25),
            quarter_stop: true,
        }
    }
}

/// The infinite-buying strategy adapter.
#[derive(Debug, Clone)]
pub struct InfiniteBuying {
    config: InfiniteBuyingConfig,
}

impl InfiniteBuying {
    pub fn new(config: InfiniteBuyingConfig) -> Self {
        InfiniteBuying { config }
    }

    pub fn config(&self) -> &InfiniteBuyingConfig {
        &self.config
    }

    /// The variable price multiplier for the ladder: `(var_base − var_slope·T)%`.
    fn variable_percent(&self, t: Decimal) -> Percent {
        Percent::from_percent(self.config.var_base - self.config.var_slope * t)
    }
}

impl Strategy for InfiniteBuying {
    fn name(&self) -> &str {
        "infinite-buying"
    }

    fn decide(&self, ctx: &DailyContext<'_>) -> Vec<OrderIntent> {
        let pos: &Position = ctx.position;
        let market = ctx.market;
        let budget = pos.daily_budget();
        let t = pos.t();
        let splits = Decimal::from(self.config.splits);
        let take_profit = Percent::from_percent(self.config.take_profit_pct);

        let mut orders: Vec<OrderIntent> = Vec::new();

        // ----- SELL side: only while holding (avg price present). -----
        if let Some(avg) = pos.avg_price
            && !pos.shares.is_zero()
        {
            let quarter = pos.shares.fraction_floor(self.config.sell_quarter);
            let rest = pos.shares - quarter;

            // ¾ at the fixed take-profit.
            orders.push(OrderIntent::limit(
                Side::Sell,
                rest,
                avg.adjusted(take_profit),
                "tp_rest",
            ));

            // ¼ leg: a de-risking stop once the seed is exhausted and underwater,
            // otherwise a variable-price take-profit. Either way it is exactly ¼, so
            // total sells (¾ + ¼) never exceed the holding.
            let seed_exhausted = t >= splits;
            let underwater = market.price < avg;
            if self.config.quarter_stop && seed_exhausted && underwater {
                orders.push(OrderIntent::loc(
                    Side::Sell,
                    quarter,
                    market.price,
                    "quarter_stop",
                ));
            } else {
                orders.push(OrderIntent::loc(
                    Side::Sell,
                    quarter,
                    avg.adjusted(self.variable_percent(t)),
                    "tp_quarter",
                ));
            }
        }

        // ----- BUY side: only while the seed is not exhausted. -----
        if t < splits {
            // Anchor on the average price; on the very first day (flat) seed at market.
            let anchor: Price = pos.avg_price.unwrap_or(market.price);
            let variable_price = anchor.adjusted(self.variable_percent(t));

            if t < self.config.half_boundary {
                // First half: half the budget at the average, half at the variable price.
                let half = budget.scaled(dec!(0.5));
                orders.push(OrderIntent::loc(
                    Side::Buy,
                    half.shares_affordable(anchor),
                    anchor,
                    "loc_low",
                ));
                orders.push(OrderIntent::loc(
                    Side::Buy,
                    half.shares_affordable(variable_price),
                    variable_price,
                    "loc_high",
                ));
            } else {
                // Second half: the whole budget on the single variable-price leg.
                orders.push(OrderIntent::loc(
                    Side::Buy,
                    budget.shares_affordable(variable_price),
                    variable_price,
                    "loc_high",
                ));
            }
        }

        // Drop any zero-share legs (e.g. budget too small for one share at this price).
        orders.retain(|order| !order.is_noop());
        orders
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::{BrokerId, MarketSnapshot, Money, Shares, Ticker};
    use time::macros::date;

    const SEED: Decimal = dec!(32000); // budget = 32000 / 40 = 800

    fn strat() -> InfiniteBuying {
        InfiniteBuying::new(InfiniteBuyingConfig::default())
    }

    fn flat() -> Position {
        Position::new(BrokerId::Paper, Ticker::new("TQQQ"), Money::new(SEED), 40)
    }

    fn holding(avg: Decimal, shares: u32, cum_spent: Decimal) -> Position {
        let mut p = flat();
        p.shares = Shares::new(shares);
        p.avg_price = Price::new(avg);
        p.cum_spent = Money::new(cum_spent);
        p.cycle_start = Some(date!(2026 - 01 - 02));
        p
    }

    fn snapshot(price: Decimal) -> MarketSnapshot {
        MarketSnapshot {
            ticker: Ticker::new("TQQQ"),
            price: Price::new(price).unwrap(),
            as_of: date!(2026 - 01 - 15),
        }
    }

    fn decide(strat: &InfiniteBuying, pos: &Position, market: &MarketSnapshot) -> Vec<OrderIntent> {
        strat.decide(&DailyContext {
            position: pos,
            market,
        })
    }

    fn by_tag<'a>(orders: &'a [OrderIntent], tag: &str) -> &'a OrderIntent {
        orders
            .iter()
            .find(|o| o.tag == tag)
            .unwrap_or_else(|| panic!("missing order tag {tag}"))
    }

    fn price(p: Decimal) -> Price {
        Price::new(p).unwrap()
    }

    #[test]
    fn first_day_seeds_two_buys_no_sells() {
        let orders = decide(&strat(), &flat(), &snapshot(dec!(100)));
        assert_eq!(orders.len(), 2);
        assert!(orders.iter().all(|o| o.side == Side::Buy));
        // budget 800, half 400. low @100 -> floor(400/100)=4 ; high @115 -> floor(400/115)=3
        let low = by_tag(&orders, "loc_low");
        assert_eq!(low.limit, Some(price(dec!(100))));
        assert_eq!(low.shares, Shares::new(4));
        let high = by_tag(&orders, "loc_high");
        assert_eq!(high.limit, Some(price(dec!(115))));
        assert_eq!(high.shares, Shares::new(3));
    }

    #[test]
    fn first_half_places_two_buys_and_two_sells() {
        // cum_spent 2400 / 800 = 3.0 -> T = 3.0 (first half)
        let pos = holding(dec!(100), 24, dec!(2400));
        let orders = decide(&strat(), &pos, &snapshot(dec!(105)));
        assert_eq!(orders.len(), 4);
        // sells: ¾=18 @115 ; ¼=6 @ var(T=3)=15-4.5=10.5% -> 110.5
        assert_eq!(by_tag(&orders, "tp_rest").shares, Shares::new(18));
        assert_eq!(by_tag(&orders, "tp_rest").limit, Some(price(dec!(115))));
        assert_eq!(by_tag(&orders, "tp_quarter").shares, Shares::new(6));
        assert_eq!(
            by_tag(&orders, "tp_quarter").limit,
            Some(price(dec!(110.5)))
        );
        // buys: low @100 ; high @110.5
        assert_eq!(by_tag(&orders, "loc_low").limit, Some(price(dec!(100))));
        assert_eq!(by_tag(&orders, "loc_high").limit, Some(price(dec!(110.5))));
    }

    #[test]
    fn second_half_uses_single_full_budget_buy() {
        // cum_spent 20000 / 800 = 25.0 -> T = 25.0 (second half)
        let pos = holding(dec!(100), 200, dec!(20000));
        let orders = decide(&strat(), &pos, &snapshot(dec!(90)));
        // one buy + two sells
        assert_eq!(orders.iter().filter(|o| o.side == Side::Buy).count(), 1);
        // var(T=25) = 15 - 37.5 = -22.5% -> 77.5
        let buy = by_tag(&orders, "loc_high");
        assert_eq!(buy.limit, Some(price(dec!(77.5))));
        // budget 800 / 77.5 = 10.3 -> 10
        assert_eq!(buy.shares, Shares::new(10));
        assert_eq!(by_tag(&orders, "tp_quarter").limit, Some(price(dec!(77.5))));
    }

    #[test]
    fn exhausted_and_underwater_triggers_quarter_stop_no_buys() {
        // cum_spent 32000 / 800 = 40.0 -> T = 40 == splits (exhausted)
        let pos = holding(dec!(100), 300, dec!(32000));
        let orders = decide(&strat(), &pos, &snapshot(dec!(80)));
        assert!(
            orders.iter().all(|o| o.side == Side::Sell),
            "no buys when exhausted"
        );
        // ¼ = 75 sold at market (80) as a stop; ¾ = 225 still at +15% = 115
        let stop = by_tag(&orders, "quarter_stop");
        assert_eq!(stop.shares, Shares::new(75));
        assert_eq!(stop.limit, Some(price(dec!(80))));
        assert_eq!(by_tag(&orders, "tp_rest").shares, Shares::new(225));
        // total sells never exceed the holding
        let sold: u32 = orders.iter().map(|o| o.shares.get()).sum();
        assert_eq!(sold, 300);
    }

    #[test]
    fn exhausted_but_above_average_keeps_variable_take_profit() {
        let pos = holding(dec!(100), 300, dec!(32000));
        let orders = decide(&strat(), &pos, &snapshot(dec!(120)));
        // market above avg -> not a stop; ¼ leg stays a variable take-profit
        assert!(orders.iter().all(|o| o.side == Side::Sell));
        assert!(orders.iter().any(|o| o.tag == "tp_quarter"));
        assert!(orders.iter().all(|o| o.tag != "quarter_stop"));
    }

    #[test]
    fn tiny_quarter_drops_noop_leg() {
        // 3 shares -> quarter = floor(0.75) = 0 -> tp_quarter dropped, tp_rest kept
        let pos = holding(dec!(100), 3, dec!(800));
        let orders = decide(&strat(), &pos, &snapshot(dec!(100)));
        assert!(orders.iter().all(|o| !o.is_noop()));
        assert!(orders.iter().any(|o| o.tag == "tp_rest"));
        assert!(orders.iter().all(|o| o.tag != "tp_quarter"));
    }
}
