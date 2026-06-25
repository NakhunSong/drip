//! The backtest engine: a deterministic, broker-free replay of daily bars through a
//! strategy. It uses the same domain [`settle`] rule as live paper trading, and marks the
//! position to market each day to build an equity curve and summary metrics.

use drip_domain::{
    Bar, DailyContext, DomainError, Fill, MarketSnapshot, Money, Position, Price, Result, Strategy,
    Ticker, settle,
};
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use time::Date;

/// The outcome of a backtest run.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestReport {
    pub ticker: Ticker,
    pub start: Date,
    pub end: Date,
    pub initial_seed: Money,
    pub final_equity: Money,
    pub realized_pnl: Money,
    pub cycles_completed: u32,
    pub total_days: usize,
    /// Days on which at least one order filled.
    pub trading_days: usize,
    pub equity_curve: Vec<(Date, f64)>,
    /// Maximum peak-to-trough drawdown as a fraction (0.25 == 25%).
    pub max_drawdown: f64,
    /// Compound annual growth rate as a fraction.
    pub cagr: f64,
}

/// The backtest runner.
#[derive(Debug)]
pub struct Backtest;

impl Backtest {
    /// Replay `bars` (chronological) through `strategy`, starting from `position`.
    pub fn run(
        strategy: &dyn Strategy,
        mut position: Position,
        bars: &[Bar],
    ) -> Result<BacktestReport> {
        let (Some(first), Some(last)) = (bars.first(), bars.last()) else {
            return Err(DomainError::MarketData(
                "backtest needs at least one bar".into(),
            ));
        };
        let start = first.date;
        let end = last.date;
        let initial_seed = position.seed;
        let ticker = position.ticker.clone();

        let mut equity_curve: Vec<(Date, f64)> = Vec::with_capacity(bars.len());
        let mut trading_days = 0usize;
        let mut cycles_completed = 0u32;
        // Reuse one snapshot so the loop-invariant ticker isn't re-cloned every bar.
        let mut market = MarketSnapshot {
            ticker: ticker.clone(),
            price: first.close,
            as_of: first.date,
        };

        for bar in bars {
            market.price = bar.close;
            market.as_of = bar.date;
            let intents = strategy.decide(&DailyContext {
                position: &position,
                market: &market,
            });

            let day_fills: Vec<Fill> = intents
                .iter()
                .filter_map(|intent| settle(intent, bar))
                .collect();
            if !day_fills.is_empty() {
                trading_days += 1;
            }
            if position.apply_day(&day_fills, bar.date) {
                cycles_completed += 1;
            }

            equity_curve.push((bar.date, money_to_f64(equity_value(&position, bar.close))));
        }

        let final_equity = equity_value(&position, last.close);
        Ok(BacktestReport {
            ticker,
            start,
            end,
            initial_seed,
            final_equity,
            realized_pnl: position.realized_pnl,
            cycles_completed,
            total_days: bars.len(),
            trading_days,
            max_drawdown: max_drawdown(&equity_curve),
            cagr: cagr(initial_seed, final_equity, start, end),
            equity_curve,
        })
    }
}

/// Account equity = seed + banked profit + current unrealized P&L.
fn equity_value(position: &Position, close: Price) -> Money {
    let unrealized = match position.avg_price {
        Some(avg) if !position.is_flat() => {
            close.total(position.shares) - avg.total(position.shares)
        }
        _ => Money::ZERO,
    };
    position.seed + position.realized_pnl + unrealized
}

fn money_to_f64(amount: Money) -> f64 {
    amount.amount().to_f64().unwrap_or(0.0)
}

fn max_drawdown(curve: &[(Date, f64)]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst = 0.0_f64;
    for &(_, equity) in curve {
        if equity > peak {
            peak = equity;
        }
        if peak > 0.0 {
            worst = worst.max((peak - equity) / peak);
        }
    }
    worst
}

fn cagr(initial: Money, final_equity: Money, start: Date, end: Date) -> f64 {
    let init = money_to_f64(initial);
    let fin = money_to_f64(final_equity);
    let days = (end - start).whole_days();
    if init <= 0.0 || days <= 0 {
        return 0.0;
    }
    if fin <= 0.0 {
        return -1.0; // total loss; avoid NaN from powf of a non-positive base
    }
    let years = days as f64 / 365.25;
    (fin / init).powf(1.0 / years) - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::{AccountId, BrokerId};
    use drip_strategies::{InfiniteBuying, InfiniteBuyingConfig};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use time::macros::date;

    fn price(p: Decimal) -> Price {
        Price::new(p).unwrap()
    }
    fn bar(d: Date, o: Decimal, h: Decimal, l: Decimal, c: Decimal) -> Bar {
        Bar {
            date: d,
            open: price(o),
            high: price(h),
            low: price(l),
            close: price(c),
        }
    }
    fn position(seed: Decimal) -> Position {
        Position::new(
            AccountId::new("paper"),
            BrokerId::Paper,
            Ticker::new("TQQQ"),
            Money::new(seed),
            40,
        )
    }
    fn strat() -> InfiniteBuying {
        InfiniteBuying::new(InfiniteBuyingConfig::default())
    }

    #[test]
    fn completes_one_cycle_and_realizes_profit() {
        // Day 1 seeds 9 shares at 100; day 2 closes at +15% so the whole holding sells out.
        let bars = vec![
            bar(
                date!(2026 - 01 - 02),
                dec!(100),
                dec!(101),
                dec!(99),
                dec!(100),
            ),
            bar(
                date!(2026 - 01 - 09),
                dec!(110),
                dec!(116),
                dec!(109),
                dec!(115),
            ),
        ];
        let report = Backtest::run(&strat(), position(dec!(40000)), &bars).unwrap();
        assert_eq!(report.cycles_completed, 1);
        assert_eq!(report.realized_pnl, Money::new(dec!(135)));
        assert_eq!(report.final_equity, Money::new(dec!(40135)));
        assert_eq!(report.total_days, 2);
        assert_eq!(report.trading_days, 2);
        assert_eq!(report.equity_curve.len(), 2);
        assert!(report.cagr.is_finite());
    }

    #[test]
    fn accumulates_and_marks_to_market_without_sellout() {
        // Day 1 buys 9 @100; day 2 dips to 95 so it averages down to 18 @97.5, no sell-out.
        let bars = vec![
            bar(
                date!(2026 - 01 - 02),
                dec!(100),
                dec!(101),
                dec!(99),
                dec!(100),
            ),
            bar(
                date!(2026 - 01 - 05),
                dec!(96),
                dec!(97),
                dec!(94),
                dec!(95),
            ),
        ];
        let report = Backtest::run(&strat(), position(dec!(40000)), &bars).unwrap();
        assert_eq!(report.cycles_completed, 0);
        // unrealized at 95 vs avg 97.5 over 18 shares = -45 -> equity 39955
        assert_eq!(report.final_equity, Money::new(dec!(39955)));
        assert!(report.max_drawdown > 0.0);
    }

    #[test]
    fn empty_bars_is_an_error() {
        assert!(Backtest::run(&strat(), position(dec!(40000)), &[]).is_err());
    }
}
