//! Application use cases — the operations that driving adapters (CLI, web dashboard) call.
//!
//! Each takes domain ports (trait objects) and returns serializable views, so the same
//! logic powers both `drip` commands and HTTP endpoints without duplication. Composition
//! (building a concrete broker, opening sqlite) stays in the driving adapter.

use crate::{Backtest, BacktestReport};
use drip_domain::{
    AccountQuery, BrokerId, Capabilities, DailyContext, Holding, MarketDataSource, MarketSnapshot,
    Money, OrderIntent, Position, Price, Quote, Quotes, Result, StateRepository, Strategy, Ticker,
};
use rust_decimal::Decimal;
use serde::Serialize;
use time::Date;

/// A read-only account snapshot: capabilities, holdings, and (best-effort) cash/eval.
#[derive(Debug, Serialize)]
pub struct AccountView {
    pub broker: BrokerId,
    pub capabilities: Capabilities,
    pub holdings: Vec<Holding>,
    /// `None` when the broker exposes no clean balance figure.
    pub balance: Option<Money>,
}

/// Fetch holdings and balance for a broker (read-only).
pub async fn account_snapshot(account: &dyn AccountQuery) -> Result<AccountView> {
    Ok(AccountView {
        broker: account.id(),
        capabilities: account.capabilities(),
        holdings: account.holdings().await?,
        balance: account.balance().await.ok(),
    })
}

/// Fetch a current quote for a ticker (read-only).
pub async fn fetch_quote(quotes: &dyn Quotes, ticker: &Ticker) -> Result<Quote> {
    quotes.quote(ticker).await
}

/// List all persisted positions.
pub async fn list_positions(repo: &dyn StateRepository) -> Result<Vec<Position>> {
    repo.list().await
}

/// What a position would order today, given a live quote — no orders are placed.
#[derive(Debug, Serialize)]
pub struct DryRunView {
    pub ticker: Ticker,
    pub price: Price,
    pub t: Decimal,
    pub avg_price: Option<Price>,
    pub intents: Vec<OrderIntent>,
}

/// Compute today's order intents for a position from a live quote and its persisted state.
/// `template` is the flat config position, used as the fallback when no saved state exists.
pub async fn dry_run(
    quotes: &dyn Quotes,
    repo: &dyn StateRepository,
    template: Position,
    strategy: &dyn Strategy,
) -> Result<DryRunView> {
    let broker = template.broker;
    let ticker = template.ticker.clone();
    let quote = quotes.quote(&ticker).await?;
    let state = repo.load(broker, &ticker).await?.unwrap_or(template);
    let market = MarketSnapshot {
        ticker: ticker.clone(),
        price: quote.price,
        as_of: quote.as_of,
    };
    let intents = strategy.decide(&DailyContext {
        position: &state,
        market: &market,
    });
    Ok(DryRunView {
        ticker,
        price: quote.price,
        t: state.t(),
        avg_price: state.avg_price,
        intents,
    })
}

/// Load bars from a data source and run a backtest.
pub async fn run_backtest(
    source: &dyn MarketDataSource,
    strategy: &dyn Strategy,
    position: Position,
    from: Date,
    to: Date,
) -> Result<BacktestReport> {
    let ticker = position.ticker.clone();
    let bars = source.bars(&ticker, from, to).await?;
    Backtest::run(strategy, position, &bars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_brokers::PaperBroker;
    use drip_domain::{Bar, OrderGateway, Price as DomainPrice, Shares};
    use drip_strategies::{InfiniteBuying, InfiniteBuyingConfig};
    use rust_decimal_macros::dec;
    use time::macros::date;

    fn bar(close: Decimal) -> Bar {
        Bar {
            date: date!(2026 - 01 - 15),
            open: DomainPrice::new(dec!(100)).unwrap(),
            high: DomainPrice::new(dec!(120)).unwrap(),
            low: DomainPrice::new(dec!(90)).unwrap(),
            close: DomainPrice::new(close).unwrap(),
        }
    }

    // A flat state repository for tests (always empty).
    struct FlatRepo;
    #[async_trait::async_trait]
    impl StateRepository for FlatRepo {
        async fn load(&self, _b: BrokerId, _t: &Ticker) -> Result<Option<Position>> {
            Ok(None)
        }
        async fn save(&self, _p: &Position) -> Result<()> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<Position>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn account_snapshot_reads_paper_holdings() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        pb.place(
            &Ticker::new("TQQQ"),
            &OrderIntent::loc(
                drip_domain::Side::Buy,
                Shares::new(5),
                DomainPrice::new(dec!(100)).unwrap(),
                "buy",
            ),
        )
        .await
        .unwrap();

        let view = account_snapshot(&pb).await.unwrap();
        assert_eq!(view.broker, BrokerId::Paper);
        assert_eq!(view.holdings.len(), 1);
        assert_eq!(view.balance, Some(Money::new(dec!(9500))));
    }

    #[tokio::test]
    async fn dry_run_on_flat_state_seeds_buys() {
        let pb = PaperBroker::new(Money::new(dec!(0)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        let strategy = InfiniteBuying::new(InfiniteBuyingConfig::default());

        let template = Position::new(
            BrokerId::Paper,
            Ticker::new("TQQQ"),
            Money::new(dec!(40000)),
            40,
        );
        let view = dry_run(&pb, &FlatRepo, template, &strategy).await.unwrap();

        assert_eq!(view.t, dec!(0));
        assert!(
            view.intents
                .iter()
                .all(|o| o.side == drip_domain::Side::Buy)
        );
        assert_eq!(view.intents.len(), 2);
    }
}
