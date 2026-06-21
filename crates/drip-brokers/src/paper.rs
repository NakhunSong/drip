//! An in-memory paper broker: a fully-working simulator that implements every broker port
//! (quotes, account queries, and order placement). It settles orders with the same domain
//! [`settle`] rule the backtest uses, so paper trading and backtests agree on fills.
//!
//! The paper broker is freely substitutable for the live KIS/Toss adapters (Liskov): the
//! engine talks to the ports, not to a concrete broker.

use async_trait::async_trait;
use drip_domain::{
    AccountQuery, Bar, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money,
    OrderGateway, OrderId, OrderIntent, Quote, Quotes, Result, Shares, Side, Ticker, settle,
};
use std::collections::HashMap;
use std::sync::Mutex;
use time::Date;

#[derive(Debug, Default)]
struct PaperState {
    cash: Money,
    holdings: HashMap<Ticker, Holding>,
    quotes: HashMap<Ticker, Quote>,
    bars: HashMap<Ticker, Bar>,
    fills: Vec<Fill>,
    next_id: u64,
}

/// A paper-trading broker backed entirely by in-memory state.
#[derive(Debug)]
pub struct PaperBroker {
    state: Mutex<PaperState>,
}

impl PaperBroker {
    /// Create a paper broker seeded with starting cash.
    pub fn new(initial_cash: Money) -> PaperBroker {
        PaperBroker {
            state: Mutex::new(PaperState {
                cash: initial_cash,
                ..PaperState::default()
            }),
        }
    }

    /// Feed the current daily bar for a ticker; also updates the quote to the bar's close.
    /// Orders placed afterwards settle against this bar.
    pub fn set_market(&self, ticker: Ticker, bar: Bar) {
        let mut state = self.lock();
        state.quotes.insert(
            ticker.clone(),
            Quote {
                ticker: ticker.clone(),
                price: bar.close,
                as_of: bar.date,
            },
        );
        state.bars.insert(ticker, bar);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PaperState> {
        self.state
            .lock()
            .expect("paper broker state mutex poisoned")
    }
}

impl BrokerInfo for PaperBroker {
    fn id(&self) -> BrokerId {
        BrokerId::Paper
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            realtime_quotes: false,
            paper_account: true,
            order_placement: true,
            overseas: true,
        }
    }
}

#[async_trait]
impl Quotes for PaperBroker {
    async fn quote(&self, ticker: &Ticker) -> Result<Quote> {
        self.lock()
            .quotes
            .get(ticker)
            .cloned()
            .ok_or_else(|| DomainError::MarketData(format!("no quote for {ticker}")))
    }
}

#[async_trait]
impl AccountQuery for PaperBroker {
    async fn holdings(&self) -> Result<Vec<Holding>> {
        // Like the live adapters, don't report fully-closed (zero-share) lots.
        Ok(self
            .lock()
            .holdings
            .values()
            .filter(|h| !h.shares.is_zero())
            .cloned()
            .collect())
    }
    async fn balance(&self) -> Result<Money> {
        Ok(self.lock().cash)
    }
    async fn fills_since(&self, since: Date) -> Result<Vec<Fill>> {
        Ok(self
            .lock()
            .fills
            .iter()
            .filter(|f| f.at >= since)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl OrderGateway for PaperBroker {
    async fn place(&self, ticker: &Ticker, order: &OrderIntent) -> Result<OrderId> {
        let mut state = self.lock();
        let bar = state
            .bars
            .get(ticker)
            .cloned()
            .ok_or_else(|| DomainError::MarketData(format!("no market bar for {ticker}")))?;

        state.next_id += 1;
        let id = OrderId::new(format!("paper-{}", state.next_id));

        if let Some(mut fill) = settle(order, &bar) {
            // A sell can never exceed the held quantity, otherwise crediting cash for the
            // full requested size would invent money (the holding only saturates at zero).
            if fill.side == Side::Sell {
                let held = state
                    .holdings
                    .get(ticker)
                    .map(|h| h.shares.get())
                    .unwrap_or(0);
                if fill.shares.get() > held {
                    fill.shares = Shares::new(held);
                }
            }
            if !fill.shares.is_zero() {
                state
                    .holdings
                    .entry(ticker.clone())
                    .or_insert_with(|| Holding::empty(ticker.clone()))
                    .apply_fill(&fill);
                match fill.side {
                    Side::Buy => state.cash -= fill.value(),
                    Side::Sell => state.cash += fill.value(),
                }
                state.fills.push(fill);
            }
        }
        Ok(id)
    }

    async fn cancel(&self, _id: &OrderId) -> Result<()> {
        // Paper orders settle synchronously on placement, so there is nothing to cancel.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::Price;
    use rust_decimal_macros::dec;
    use time::macros::date;

    fn tqqq() -> Ticker {
        Ticker::new("TQQQ")
    }
    fn price(p: rust_decimal::Decimal) -> Price {
        Price::new(p).unwrap()
    }
    fn bar(close: rust_decimal::Decimal) -> Bar {
        Bar {
            date: date!(2026 - 01 - 15),
            open: price(dec!(100)),
            high: price(dec!(120)),
            low: price(dec!(90)),
            close: price(close),
        }
    }

    #[tokio::test]
    async fn capabilities_advertise_paper_and_orders_but_no_realtime() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        let caps = pb.capabilities();
        assert!(caps.paper_account && caps.order_placement);
        assert!(!caps.realtime_quotes);
        assert_eq!(pb.id(), BrokerId::Paper);
    }

    #[tokio::test]
    async fn buy_loc_fills_and_debits_cash() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        pb.set_market(tqqq(), bar(dec!(100)));
        let intent = OrderIntent::loc(Side::Buy, Shares::new(5), price(dec!(100)), "loc_low");

        pb.place(&tqqq(), &intent).await.unwrap();

        let holdings = pb.holdings().await.unwrap();
        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].shares, Shares::new(5));
        assert_eq!(holdings[0].avg_price, Some(price(dec!(100))));
        // 10000 - 5 * 100 = 9500
        assert_eq!(pb.balance().await.unwrap(), Money::new(dec!(9500)));
    }

    #[tokio::test]
    async fn loc_buy_above_close_does_not_fill() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        pb.set_market(tqqq(), bar(dec!(105))); // close 105 > limit 100 -> no fill
        let intent = OrderIntent::loc(Side::Buy, Shares::new(5), price(dec!(100)), "loc_low");

        pb.place(&tqqq(), &intent).await.unwrap();

        assert!(pb.holdings().await.unwrap().is_empty());
        assert_eq!(pb.balance().await.unwrap(), Money::new(dec!(10000)));
    }

    #[tokio::test]
    async fn sell_credits_cash_and_reduces_holding() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        pb.set_market(tqqq(), bar(dec!(100)));
        pb.place(
            &tqqq(),
            &OrderIntent::loc(Side::Buy, Shares::new(10), price(dec!(100)), "buy"),
        )
        .await
        .unwrap();
        // Sell 4 at a +15% limit; high is 120 so it fills at 115.
        pb.set_market(tqqq(), bar(dec!(118)));
        pb.place(
            &tqqq(),
            &OrderIntent::limit(Side::Sell, Shares::new(4), price(dec!(115)), "tp"),
        )
        .await
        .unwrap();

        let holdings = pb.holdings().await.unwrap();
        assert_eq!(holdings[0].shares, Shares::new(6));
        // 10000 - 1000 (buy) + 4*115 (sell) = 9460
        assert_eq!(pb.balance().await.unwrap(), Money::new(dec!(9460)));
    }

    #[tokio::test]
    async fn quote_returns_last_close_or_errors_when_unknown() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        assert!(pb.quote(&tqqq()).await.is_err());
        pb.set_market(tqqq(), bar(dec!(100)));
        assert_eq!(pb.quote(&tqqq()).await.unwrap().price, price(dec!(100)));
    }

    #[tokio::test]
    async fn sell_cannot_exceed_holding_or_invent_cash() {
        let pb = PaperBroker::new(Money::new(dec!(10000)));
        pb.set_market(tqqq(), bar(dec!(100)));
        pb.place(
            &tqqq(),
            &OrderIntent::loc(Side::Buy, Shares::new(3), price(dec!(100)), "buy"),
        )
        .await
        .unwrap();
        // Attempt to sell 10 while holding only 3 (limit fills at 105 since high is 120).
        pb.set_market(tqqq(), bar(dec!(120)));
        pb.place(
            &tqqq(),
            &OrderIntent::limit(Side::Sell, Shares::new(10), price(dec!(105)), "tp"),
        )
        .await
        .unwrap();
        assert!(pb.holdings().await.unwrap().is_empty()); // sold exactly the 3 held
        // 10000 - 3*100 (buy) + 3*105 (sell) = 10015 — no phantom cash from the extra 7.
        assert_eq!(pb.balance().await.unwrap(), Money::new(dec!(10015)));
    }
}
