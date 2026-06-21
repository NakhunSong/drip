//! Application use cases — the operations that driving adapters (CLI, web dashboard) call.
//!
//! Each takes domain ports (trait objects) and returns serializable views, so the same
//! logic powers both `drip` commands and HTTP endpoints without duplication. Composition
//! (building a concrete broker, opening sqlite) stays in the driving adapter.

use crate::{Backtest, BacktestReport};
use drip_domain::{
    AccountQuery, BrokerId, Capabilities, DailyContext, DomainError, Holding, MarketDataSource,
    MarketSnapshot, Money, OrderGateway, OrderIntent, OrderJournal, OrderKind, Position, Price,
    Quote, Quotes, Result, StateRepository, Strategy, Ticker, risk,
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

/// Load state, fetch a quote, and run the strategy — the shared first half of `dry_run`
/// and `place_orders`. Returns the effective position state (saved state, or `template`
/// when none is persisted), the quote, and the decided intents.
async fn compute_intents(
    quotes: &dyn Quotes,
    repo: &dyn StateRepository,
    template: Position,
    strategy: &dyn Strategy,
) -> Result<(Position, Quote, Vec<OrderIntent>)> {
    let broker = template.broker;
    let ticker = template.ticker.clone();
    let quote = quotes.quote(&ticker).await?;
    let state = repo.load(broker, &ticker).await?.unwrap_or(template);
    let market = MarketSnapshot {
        ticker,
        price: quote.price,
        as_of: quote.as_of,
    };
    let intents = strategy.decide(&DailyContext {
        position: &state,
        market: &market,
    });
    Ok((state, quote, intents))
}

/// Compute today's order intents for a position from a live quote and its persisted state.
/// `template` is the flat config position, used as the fallback when no saved state exists.
pub async fn dry_run(
    quotes: &dyn Quotes,
    repo: &dyn StateRepository,
    template: Position,
    strategy: &dyn Strategy,
) -> Result<DryRunView> {
    let (state, quote, intents) = compute_intents(quotes, repo, template, strategy).await?;
    Ok(DryRunView {
        ticker: state.ticker.clone(),
        price: quote.price,
        t: state.t(),
        avg_price: state.avg_price,
        intents,
    })
}

/// The ports a tick drives. Bundling them keeps `place_orders` to a small signature and
/// lets a driving adapter wire one sqlite store as both `repo` and `journal`.
pub struct TickPorts<'a> {
    pub quotes: &'a dyn Quotes,
    pub gateway: &'a dyn OrderGateway,
    pub repo: &'a dyn StateRepository,
    pub journal: &'a dyn OrderJournal,
}

/// The outcome of one order intent within a tick.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TickStatus {
    /// Sent to the broker and accepted.
    Placed,
    /// Already reserved or placed earlier today — skipped for at-most-once.
    SkippedIdempotent,
    /// Preview only; `--execute` was not given.
    WouldPlace,
    /// Reserved, but the broker rejected it (see `error`).
    Failed,
}

#[derive(Debug, Serialize)]
pub struct TickOrder {
    pub intent: OrderIntent,
    pub status: TickStatus,
    pub order_id: Option<String>,
    pub error: Option<String>,
}

/// What a `drip tick` did (or, in preview, would do) for a position.
#[derive(Debug, Serialize)]
pub struct TickView {
    pub ticker: Ticker,
    pub price: Price,
    pub t: Decimal,
    pub executed: bool,
    pub orders: Vec<TickOrder>,
    /// An informational note, e.g. KIS 모의 degrading LOC orders to day-limits.
    pub note: Option<String>,
}

/// Compute today's order intents for a position and, when `execute` is set, place them
/// through the gateway. Dry-run by default: with `execute = false` nothing is sent.
///
/// Safety: executing against a real (non-paper) account requires explicit `allow_real`
/// consent — the gate lives here, not in a driving adapter, so every caller inherits it.
/// Every intent is then risk-vetted *before* any order is placed, and a single failed check
/// aborts the whole tick (no partial strategy reaches the market). Placement is at-most-once
/// — each intent is reserved in the journal *before* it is sent, so a crash or a same-day
/// re-run never double-places (at worst it skips a reserved-but-unsent order, which is
/// surfaced and far safer than a double buy).
pub async fn place_orders(
    ports: &TickPorts<'_>,
    template: Position,
    strategy: &dyn Strategy,
    execute: bool,
    allow_real: bool,
    today: Date,
) -> Result<TickView> {
    // Fail fast, before any I/O: refuse to trade a real account without explicit consent.
    if execute && !allow_real && !ports.gateway.capabilities().paper_account {
        return Err(DomainError::Risk(
            "refusing to place orders against a real (non-paper) account without explicit \
             real-account consent"
                .into(),
        ));
    }
    let (state, quote, intents) =
        compute_intents(ports.quotes, ports.repo, template, strategy).await?;
    let intents: Vec<OrderIntent> = intents.into_iter().filter(|i| !i.is_noop()).collect();

    // Pre-flight: vet every intent first; any violation aborts the entire tick.
    for intent in &intents {
        risk::vet(intent, &state, quote.price)?;
    }

    let broker = state.broker;
    let ticker = state.ticker.clone();
    let note = (ports.gateway.capabilities().paper_account
        && intents.iter().any(|i| i.kind == OrderKind::LimitOnClose))
    .then(|| {
        "KIS 모의(paper) accepts only limit orders; LOC legs are sent as plain day-limits at \
         the leg's limit price (not a true LOC), so they may fill intraday, not at the close"
            .to_string()
    });

    let mut orders = Vec::with_capacity(intents.len());
    for intent in intents {
        if !execute {
            orders.push(TickOrder {
                intent,
                status: TickStatus::WouldPlace,
                order_id: None,
                error: None,
            });
            continue;
        }
        let key = intent.client_key(broker, &ticker, today);
        if !ports.journal.reserve(&key).await? {
            orders.push(TickOrder {
                intent,
                status: TickStatus::SkippedIdempotent,
                order_id: None,
                error: None,
            });
            continue;
        }
        match ports.gateway.place(&ticker, &intent).await {
            Ok(id) => {
                // The order is live at the broker. A journal-record failure must not abort
                // the tick or lose the id (the reservation already prevents a re-run from
                // double-placing); surface it on the order instead of propagating.
                let record_err = ports
                    .journal
                    .record(&key, &id)
                    .await
                    .err()
                    .map(|e| format!("order is live but journal record failed: {e}"));
                orders.push(TickOrder {
                    intent,
                    status: TickStatus::Placed,
                    order_id: Some(id.as_str().to_string()),
                    error: record_err,
                });
            }
            Err(e) => orders.push(TickOrder {
                intent,
                status: TickStatus::Failed,
                order_id: None,
                error: Some(e.to_string()),
            }),
        }
    }

    Ok(TickView {
        ticker,
        price: quote.price,
        t: state.t(),
        executed: execute,
        orders,
        note,
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
    use drip_domain::{
        Bar, BrokerInfo, Capabilities, OrderGateway, OrderId, OrderJournal, Price as DomainPrice,
        Shares,
    };
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

    // An in-memory order journal for tests: reserve is at-most-once, record is a no-op.
    #[derive(Default)]
    struct MemJournal(std::sync::Mutex<std::collections::HashSet<String>>);
    #[async_trait::async_trait]
    impl OrderJournal for MemJournal {
        async fn reserve(&self, key: &str) -> Result<bool> {
            Ok(self.0.lock().unwrap().insert(key.to_string()))
        }
        async fn record(&self, _key: &str, _id: &OrderId) -> Result<()> {
            Ok(())
        }
    }

    // A non-paper, order-capable gateway: exercises the real-account consent gate.
    struct RealGateway;
    impl BrokerInfo for RealGateway {
        fn id(&self) -> BrokerId {
            BrokerId::Kis
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                realtime_quotes: false,
                paper_account: false,
                order_placement: true,
                overseas: true,
            }
        }
    }
    #[async_trait::async_trait]
    impl OrderGateway for RealGateway {
        async fn place(&self, _ticker: &Ticker, _order: &OrderIntent) -> Result<OrderId> {
            Ok(OrderId::new("real-1"))
        }
        async fn cancel(&self, _id: &OrderId) -> Result<()> {
            Ok(())
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

    #[tokio::test]
    async fn place_orders_executes_then_is_idempotent_same_day() {
        let pb = PaperBroker::new(Money::new(dec!(0)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        let strategy = InfiniteBuying::new(InfiniteBuyingConfig::default());
        let journal = MemJournal::default();
        let ports = TickPorts {
            quotes: &pb,
            gateway: &pb,
            repo: &FlatRepo,
            journal: &journal,
        };
        let template = || {
            Position::new(
                BrokerId::Paper,
                Ticker::new("TQQQ"),
                Money::new(dec!(40000)),
                40,
            )
        };
        let today = date!(2026 - 06 - 21);

        let view = place_orders(&ports, template(), &strategy, true, false, today)
            .await
            .unwrap();
        assert!(view.executed);
        assert!(view.note.is_some()); // paper degrades LOC -> limit, surfaced as a note
        assert_eq!(view.orders.len(), 2); // first-day loc_low + loc_high
        assert!(
            view.orders
                .iter()
                .all(|o| matches!(o.status, TickStatus::Placed))
        );

        // Re-running the same day reserves nothing new — every leg is skipped.
        let again = place_orders(&ports, template(), &strategy, true, false, today)
            .await
            .unwrap();
        assert!(
            again
                .orders
                .iter()
                .all(|o| matches!(o.status, TickStatus::SkippedIdempotent))
        );
    }

    #[tokio::test]
    async fn place_orders_preview_reserves_nothing() {
        let pb = PaperBroker::new(Money::new(dec!(0)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        let strategy = InfiniteBuying::new(InfiniteBuyingConfig::default());
        let journal = MemJournal::default();
        let ports = TickPorts {
            quotes: &pb,
            gateway: &pb,
            repo: &FlatRepo,
            journal: &journal,
        };
        let template = Position::new(
            BrokerId::Paper,
            Ticker::new("TQQQ"),
            Money::new(dec!(40000)),
            40,
        );

        let view = place_orders(
            &ports,
            template,
            &strategy,
            false,
            false,
            date!(2026 - 06 - 21),
        )
        .await
        .unwrap();
        assert!(!view.executed);
        assert!(
            view.orders
                .iter()
                .all(|o| matches!(o.status, TickStatus::WouldPlace))
        );
        assert!(journal.0.lock().unwrap().is_empty()); // preview touches no journal
    }

    #[tokio::test]
    async fn place_orders_refuses_real_account_without_consent() {
        let pb = PaperBroker::new(Money::new(dec!(0)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        let strategy = InfiniteBuying::new(InfiniteBuyingConfig::default());
        let journal = MemJournal::default();
        let gateway = RealGateway;
        let ports = TickPorts {
            quotes: &pb,
            gateway: &gateway,
            repo: &FlatRepo,
            journal: &journal,
        };
        let template = || {
            Position::new(
                BrokerId::Paper,
                Ticker::new("TQQQ"),
                Money::new(dec!(40000)),
                40,
            )
        };
        let today = date!(2026 - 06 - 21);

        // Executing against a real (non-paper) account without consent is refused outright.
        let denied = place_orders(&ports, template(), &strategy, true, false, today).await;
        assert!(matches!(denied, Err(DomainError::Risk(_))));

        // With explicit consent the same tick proceeds and places.
        let allowed = place_orders(&ports, template(), &strategy, true, true, today)
            .await
            .unwrap();
        assert!(
            allowed
                .orders
                .iter()
                .all(|o| matches!(o.status, TickStatus::Placed))
        );
    }
}
