//! Position, cycle accounting, and [`Fill`] — the persisted state of an averaging
//! strategy on one ticker.

use crate::market::{BrokerId, Side, Ticker};
use crate::money::{Money, Price, Shares};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use time::Date;

/// A broker-confirmed execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub side: Side,
    pub shares: Shares,
    pub price: Price,
    pub at: Date,
}
impl Fill {
    /// Cash value of the fill (`price × shares`).
    pub fn value(&self) -> Money {
        self.price.total(self.shares)
    }
}

/// Live state of an infinite-buying position on one ticker.
///
/// `avg_price` is `None` exactly when the position is flat (between cycles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub broker: BrokerId,
    pub ticker: Ticker,
    pub seed: Money,
    pub splits: u32,
    pub shares: Shares,
    pub avg_price: Option<Price>,
    pub cum_spent: Money,
    pub realized_pnl: Money,
    pub cycle_index: u32,
    pub cycle_start: Option<Date>,
    /// The latest trading date whose fills have been reconciled into this ledger. Reconcile
    /// applies only fills dated *after* this watermark (and before today), then advances it,
    /// so a re-run never double-counts. `None` until the first reconcile. `#[serde(default)]`
    /// keeps positions stored before this field existed loadable.
    #[serde(default)]
    pub reconciled_through: Option<Date>,
}

impl Position {
    /// A fresh, flat position ready to start cycle 0.
    pub fn new(broker: BrokerId, ticker: Ticker, seed: Money, splits: u32) -> Position {
        Position {
            broker,
            ticker,
            seed,
            splits: splits.max(1),
            shares: Shares::ZERO,
            avg_price: None,
            cum_spent: Money::ZERO,
            realized_pnl: Money::ZERO,
            cycle_index: 0,
            cycle_start: None,
            reconciled_through: None,
        }
    }

    /// Per-tranche budget = seed / splits.
    pub fn daily_budget(&self) -> Money {
        self.seed.split(self.splits)
    }

    /// T (tranche counter) = round-up(`cum_spent / daily_budget`) to one decimal place.
    pub fn t(&self) -> Decimal {
        let budget = self.daily_budget().amount();
        if budget.is_zero() {
            return Decimal::ZERO;
        }
        let raw = self.cum_spent.amount() / budget;
        (raw * Decimal::from(10)).ceil() / Decimal::from(10)
    }

    pub fn is_flat(&self) -> bool {
        self.shares.is_zero()
    }

    /// Apply a confirmed fill, updating holdings, weighted average, cumulative spend, and
    /// realized P&L. A sell that empties the position leaves it flat.
    pub fn apply_fill(&mut self, fill: &Fill) {
        match fill.side {
            Side::Buy => {
                update_average(&mut self.shares, &mut self.avg_price, fill);
                self.cum_spent += fill.value();
            }
            Side::Sell => {
                let cost_basis = self
                    .avg_price
                    .map(|avg| avg.total(fill.shares))
                    .unwrap_or(Money::ZERO);
                self.realized_pnl += fill.value() - cost_basis;
                update_average(&mut self.shares, &mut self.avg_price, fill);
            }
        }
    }

    /// Apply a trading day's fills in order, then resolve cycle boundaries: stamp the cycle
    /// start on first entry, and on a full sell-out bank the cycle and reset the ladder.
    /// Returns `true` if the day completed a cycle. Keeping the cycle-boundary rule here (not
    /// in the engine) means the backtest and a future live engine stay in agreement.
    pub fn apply_day(&mut self, fills: &[Fill], date: Date) -> bool {
        // Resolve cycle boundaries per fill, not once at end of day: a sell can empty the
        // position mid-day and a later buy re-enter it, which must still bank the completed
        // cycle and reset the ladder for the fresh one.
        let mut completed_cycle = false;
        for fill in fills {
            let had_shares = !self.is_flat();
            self.apply_fill(fill);
            if self.cycle_start.is_none() && !self.is_flat() {
                self.cycle_start = Some(date);
            }
            if had_shares && self.is_flat() {
                self.start_new_cycle(date);
                completed_cycle = true;
            }
        }
        completed_cycle
    }

    /// Restart a fresh cycle after a full sell-out — the "infinite" in infinite buying.
    /// Banked `realized_pnl` and `seed` carry over; the ladder state resets.
    pub fn start_new_cycle(&mut self, on: Date) {
        self.shares = Shares::ZERO;
        self.avg_price = None;
        self.cum_spent = Money::ZERO;
        self.cycle_index += 1;
        self.cycle_start = Some(on);
    }

    /// Fold broker-confirmed `fills` into the ledger, advancing the tranche counter and the
    /// `reconciled_through` watermark. Idempotent by construction: only fills dated strictly
    /// after the current watermark *and* strictly before `today` are applied — completed days
    /// only — so a re-run, or a day's not-yet-final fills, never double-counts. Fills are
    /// grouped by date and replayed through [`Position::apply_day`], so cycle boundaries are
    /// resolved by the same rule the backtest uses. The watermark advances only to the latest
    /// date actually applied; an empty result leaves the position untouched.
    ///
    /// Two assumptions hold for drip's day orders (LOC / day-limit, which fill at the close or
    /// expire) and are what let a per-day watermark be exact without a per-execution id:
    /// (1) a day's fills are *final once that day is complete*, so banking it only after
    /// `today` moves past it captures them all at once; (2) `fills` arrive in chronological
    /// order — `apply_day` relies on it to resolve a same-day sell-out before any re-entry.
    /// Intraday partial-fill precision for resting/GTC orders is out of scope until streaming.
    pub fn reconcile(&mut self, fills: &[Fill], today: Date) -> ReconcileOutcome {
        let watermark = self.reconciled_through;
        let mut pending: Vec<&Fill> = fills
            .iter()
            .filter(|f| watermark.is_none_or(|w| f.at > w) && f.at < today)
            .collect();
        // Stable sort by date keeps each day's fills in their reported (chronological) order.
        pending.sort_by_key(|f| f.at);

        let mut applied = 0;
        let mut cycles_completed = 0;
        for day in pending.chunk_by(|a, b| a.at == b.at) {
            let date = day[0].at;
            let day_fills: Vec<Fill> = day.iter().map(|&f| f.clone()).collect();
            if self.apply_day(&day_fills, date) {
                cycles_completed += 1;
            }
            applied += day_fills.len();
            self.reconciled_through = Some(date); // chunks ascend, so the last write is the max
        }
        ReconcileOutcome {
            applied,
            cycles_completed,
            through: self.reconciled_through,
        }
    }
}

/// What a [`Position::reconcile`] applied: the number of fills folded in, how many cycles
/// they completed, and the watermark the ledger advanced to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub applied: usize,
    pub cycles_completed: u32,
    pub through: Option<Date>,
}

/// A broker-reported holding: what the account actually owns right now, independent of any
/// strategy state. Distinct from [`Position`], which is drip's own strategy ledger (seed,
/// splits, cycle). The engine reconciles a broker's holdings against its local positions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holding {
    pub ticker: Ticker,
    pub shares: Shares,
    pub avg_price: Option<Price>,
}

impl Holding {
    pub fn empty(ticker: Ticker) -> Holding {
        Holding {
            ticker,
            shares: Shares::ZERO,
            avg_price: None,
        }
    }

    /// Apply a fill to the holding (weighted average on buy; reduce on sell).
    pub fn apply_fill(&mut self, fill: &Fill) {
        update_average(&mut self.shares, &mut self.avg_price, fill);
    }
}

/// Update `(shares, avg_price)` for a fill: weighted-average on a buy, reduce (clearing the
/// average when flat) on a sell. Shared by [`Position`] and [`Holding`] so the averaging
/// invariant lives in exactly one place.
fn update_average(shares: &mut Shares, avg_price: &mut Option<Price>, fill: &Fill) {
    match fill.side {
        Side::Buy => {
            let old_shares = Decimal::from(shares.get());
            let add_shares = Decimal::from(fill.shares.get());
            let new_shares = old_shares + add_shares;
            if !new_shares.is_zero() {
                let old_avg = avg_price.map(Price::value).unwrap_or(Decimal::ZERO);
                let weighted = old_avg * old_shares + fill.price.value() * add_shares;
                *avg_price = Price::new(weighted / new_shares);
            }
            *shares = *shares + fill.shares;
        }
        Side::Sell => {
            *shares = *shares - fill.shares;
            if shares.is_zero() {
                *avg_price = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use time::macros::date;

    fn tqqq() -> Position {
        Position::new(
            BrokerId::Paper,
            Ticker::new("TQQQ"),
            Money::new(dec!(32000)),
            40,
        )
    }

    fn buy(shares: u32, price: Decimal, at: Date) -> Fill {
        Fill {
            side: Side::Buy,
            shares: Shares::new(shares),
            price: Price::new(price).unwrap(),
            at,
        }
    }
    fn sell(shares: u32, price: Decimal, at: Date) -> Fill {
        Fill {
            side: Side::Sell,
            shares: Shares::new(shares),
            price: Price::new(price).unwrap(),
            at,
        }
    }

    #[test]
    fn daily_budget_is_seed_over_splits() {
        assert_eq!(tqqq().daily_budget(), Money::new(dec!(800)));
    }

    #[test]
    fn t_rounds_up_to_one_decimal() {
        let mut p = tqqq();
        // $850 spent against an $800 budget: 850/800 = 1.0625 -> round up 0.1 -> 1.1
        p.cum_spent = Money::new(dec!(850));
        assert_eq!(p.t(), dec!(1.1));
    }

    #[test]
    fn t_is_zero_when_nothing_spent() {
        assert_eq!(tqqq().t(), dec!(0));
    }

    #[test]
    fn buy_sets_weighted_average() {
        let mut p = tqqq();
        p.apply_fill(&buy(10, dec!(100), date!(2026 - 01 - 02)));
        p.apply_fill(&buy(5, dec!(90), date!(2026 - 01 - 03)));
        assert_eq!(p.shares, Shares::new(15));
        // (100*10 + 90*5) / 15 = 96.6667
        assert_eq!(p.avg_price.unwrap().value().round_dp(4), dec!(96.6667));
        assert_eq!(p.cum_spent, Money::new(dec!(1450)));
    }

    #[test]
    fn sell_realizes_pnl_and_flattens() {
        let mut p = tqqq();
        p.apply_fill(&buy(10, dec!(100), date!(2026 - 01 - 02)));
        // Sell all 10 at +15%: pnl = (115 - 100) * 10 = 150
        p.apply_fill(&sell(10, dec!(115), date!(2026 - 01 - 10)));
        assert!(p.is_flat());
        assert_eq!(p.avg_price, None);
        assert_eq!(p.realized_pnl, Money::new(dec!(150)));
    }

    #[test]
    fn start_new_cycle_resets_ladder_keeps_pnl() {
        let mut p = tqqq();
        p.realized_pnl = Money::new(dec!(150));
        p.cum_spent = Money::new(dec!(800));
        p.start_new_cycle(date!(2026 - 01 - 11));
        assert_eq!(p.cycle_index, 1);
        assert_eq!(p.cum_spent, Money::ZERO);
        assert_eq!(p.realized_pnl, Money::new(dec!(150)));
        assert_eq!(p.cycle_start, Some(date!(2026 - 01 - 11)));
    }

    #[test]
    fn apply_day_banks_cycle_on_same_day_sellout_then_rebuy() {
        let mut p = tqqq();
        p.apply_fill(&buy(10, dec!(100), date!(2026 - 01 - 02)));
        p.cycle_start = Some(date!(2026 - 01 - 02));
        // Same day: sell the whole holding (cycle completes) then a buy re-enters.
        let fills = vec![
            sell(10, dec!(115), date!(2026 - 01 - 10)),
            buy(5, dec!(90), date!(2026 - 01 - 10)),
        ];
        assert!(p.apply_day(&fills, date!(2026 - 01 - 10)));
        assert_eq!(p.cycle_index, 1); // the completed cycle was banked
        assert_eq!(p.realized_pnl, Money::new(dec!(150))); // (115 - 100) * 10
        assert_eq!(p.shares, Shares::new(5)); // rebuy belongs to the fresh cycle
        assert_eq!(p.cum_spent, Money::new(dec!(450))); // ladder reset, then 5 * 90
        assert_eq!(p.cycle_start, Some(date!(2026 - 01 - 10))); // fresh cycle date
    }

    #[test]
    fn reconcile_applies_completed_days_and_advances_t() {
        let mut p = tqqq(); // seed 32000 / 40 splits => 800/tranche budget
        let fills = vec![
            buy(8, dec!(100), date!(2026 - 06 - 18)),
            buy(8, dec!(100), date!(2026 - 06 - 19)),
        ];
        let out = p.reconcile(&fills, date!(2026 - 06 - 21));

        assert_eq!(out.applied, 2);
        assert_eq!(out.cycles_completed, 0);
        assert_eq!(out.through, Some(date!(2026 - 06 - 19)));
        assert_eq!(p.shares, Shares::new(16));
        assert_eq!(p.cum_spent, Money::new(dec!(1600))); // 16 * 100
        assert_eq!(p.reconciled_through, Some(date!(2026 - 06 - 19)));
        assert_eq!(p.t(), dec!(2)); // ceil(1600/800 * 10)/10
        assert_eq!(p.cycle_start, Some(date!(2026 - 06 - 18)));
    }

    #[test]
    fn reconcile_is_idempotent() {
        let mut p = tqqq();
        let fills = vec![buy(8, dec!(100), date!(2026 - 06 - 18))];
        p.reconcile(&fills, date!(2026 - 06 - 21));
        let snapshot = p.clone();
        // Re-running with the same fills applies nothing and changes nothing.
        let again = p.reconcile(&fills, date!(2026 - 06 - 21));
        assert_eq!(again.applied, 0);
        assert_eq!(p, snapshot);
    }

    #[test]
    fn reconcile_excludes_todays_not_yet_final_fills() {
        let mut p = tqqq();
        let fills = vec![buy(8, dec!(100), date!(2026 - 06 - 21))]; // dated today
        let out = p.reconcile(&fills, date!(2026 - 06 - 21));
        assert_eq!(out.applied, 0);
        assert!(p.is_flat());
        assert_eq!(p.reconciled_through, None); // watermark untouched
    }

    #[test]
    fn reconcile_skips_fills_at_or_before_watermark() {
        let mut p = tqqq();
        p.reconciled_through = Some(date!(2026 - 06 - 18));
        let fills = vec![
            buy(8, dec!(100), date!(2026 - 06 - 17)), // before watermark
            buy(5, dec!(100), date!(2026 - 06 - 18)), // exactly at watermark
            buy(3, dec!(100), date!(2026 - 06 - 19)), // after watermark
        ];
        let out = p.reconcile(&fills, date!(2026 - 06 - 21));
        assert_eq!(out.applied, 1); // only 06-19 is fresh
        assert_eq!(p.shares, Shares::new(3));
        assert_eq!(p.reconciled_through, Some(date!(2026 - 06 - 19)));
    }

    #[test]
    fn reconcile_applies_same_day_fills_in_chronological_order() {
        // A same-day buy-then-sellout must bank the cycle. This relies on fills arriving in
        // chronological order (the `fills_since` contract): reconcile's stable date-sort cannot
        // reorder within a day, so a sell-first ordering would wrongly realize P&L on a flat
        // position and skip the cycle. Locks the contract the KIS `SORT_SQN=DS` choice rests on.
        let mut p = tqqq();
        let fills = vec![
            buy(10, dec!(100), date!(2026 - 06 - 18)),
            sell(10, dec!(115), date!(2026 - 06 - 18)), // same day, chronologically after the buy
        ];
        let out = p.reconcile(&fills, date!(2026 - 06 - 21));
        assert_eq!(out.applied, 2);
        assert_eq!(out.cycles_completed, 1); // buy → sellout banks exactly one cycle
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl, Money::new(dec!(150))); // (115-100)*10, not a sell-first phantom
    }

    #[test]
    fn reconcile_counts_cycle_on_sellout() {
        let mut p = tqqq();
        // Day 1: enter 10 @ 100. Day 2: sell all 10 @ 115 -> the cycle completes.
        let fills = vec![
            buy(10, dec!(100), date!(2026 - 06 - 18)),
            sell(10, dec!(115), date!(2026 - 06 - 19)),
        ];
        let out = p.reconcile(&fills, date!(2026 - 06 - 21));
        assert_eq!(out.cycles_completed, 1);
        assert_eq!(p.cycle_index, 1);
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl, Money::new(dec!(150))); // (115 - 100) * 10
    }

    #[test]
    fn reconciled_through_defaults_to_none_for_legacy_json() {
        // A Position serialized before `reconciled_through` existed must still deserialize.
        let p = tqqq();
        let mut value = serde_json::to_value(&p).unwrap();
        value.as_object_mut().unwrap().remove("reconciled_through"); // simulate a legacy row
        let restored: Position = serde_json::from_value(value).unwrap();
        assert_eq!(restored.reconciled_through, None);
        assert_eq!(restored, p);
    }
}
