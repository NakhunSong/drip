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

/// Fetch a quote for `state`'s ticker and run the strategy on it — the decide half shared by
/// `compute_intents` (which loads `state` from the repo) and `place_orders` (which gets it
/// from the reconcile step).
async fn decide_on(
    quotes: &dyn Quotes,
    state: &Position,
    strategy: &dyn Strategy,
) -> Result<(Quote, Vec<OrderIntent>)> {
    let quote = quotes.quote(&state.ticker).await?;
    let market = MarketSnapshot {
        ticker: state.ticker.clone(),
        price: quote.price,
        as_of: quote.as_of,
    };
    let intents = strategy.decide(&DailyContext {
        position: state,
        market: &market,
    });
    Ok((quote, intents))
}

/// Load state and run the strategy — the first half of `dry_run`. Returns the effective
/// position state (saved state, or `template` when none is persisted), the quote, and intents.
async fn compute_intents(
    quotes: &dyn Quotes,
    repo: &dyn StateRepository,
    template: Position,
    strategy: &dyn Strategy,
) -> Result<(Position, Quote, Vec<OrderIntent>)> {
    let broker = template.broker;
    let ticker = template.ticker.clone();
    let state = repo.load(broker, &ticker).await?.unwrap_or(template);
    let (quote, intents) = decide_on(quotes, &state, strategy).await?;
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

/// How far back to scan for fills on a position's first reconcile (before any watermark
/// exists). KIS overseas execution history is bounded (~3 months); 90 days comfortably covers
/// an infinite-buying cost-averaging cycle.
const RECONCILE_LOOKBACK_DAYS: i64 = 90;

fn earliest_reconcile_window(today: Date) -> Date {
    today.saturating_sub(time::Duration::days(RECONCILE_LOOKBACK_DAYS))
}

/// What a [`reconcile`] did to a position's ledger.
#[derive(Debug, Serialize)]
pub struct ReconcileView {
    pub ticker: Ticker,
    pub applied_fills: usize,
    pub cycles_completed: u32,
    pub t_before: Decimal,
    pub t_after: Decimal,
    pub through: Option<Date>,
    /// Set only when the broker cannot report fills — the ledger was left unchanged.
    pub note: Option<String>,
}

/// Pull settled fills from the broker and fold them into the position's ledger, advancing the
/// tranche counter as fills accumulate and banking completed cycles. Idempotent: only fills on
/// completed days not yet reconciled are applied (see [`Position::reconcile`]), so running it
/// repeatedly — or right after a crash — never double-counts. It is read-only at the broker
/// (no orders are placed), so it needs no real-account consent. A broker that cannot report
/// execution history yields a note and leaves the ledger untouched, rather than failing.
pub async fn reconcile(
    account: &dyn AccountQuery,
    repo: &dyn StateRepository,
    template: Position,
    today: Date,
) -> Result<ReconcileView> {
    let (_, view) = reconcile_into(account, repo, template, today, true).await?;
    Ok(view)
}

/// Reconcile into an in-memory `Position`, persisting the advance only when `persist`. This lets
/// a `drip tick` preview show an up-to-date ledger without mutating stored state, while
/// `--execute` (and the standalone [`reconcile`]) commit it. Returns the reconciled position so
/// the caller can decide on it without a reload.
async fn reconcile_into(
    account: &dyn AccountQuery,
    repo: &dyn StateRepository,
    template: Position,
    today: Date,
    persist: bool,
) -> Result<(Position, ReconcileView)> {
    let broker = template.broker;
    let ticker = template.ticker.clone();
    let mut state = repo.load(broker, &ticker).await?.unwrap_or(template);
    let t_before = state.t();
    let since = state
        .reconciled_through
        .unwrap_or_else(|| earliest_reconcile_window(today));

    let view = match account.fills_since(&ticker, since).await {
        Ok(fills) => {
            let outcome = state.reconcile(&fills, today);
            if persist && outcome.applied > 0 {
                repo.save(&state).await?;
            }
            ReconcileView {
                ticker: ticker.clone(),
                applied_fills: outcome.applied,
                cycles_completed: outcome.cycles_completed,
                t_before,
                t_after: state.t(),
                through: outcome.through,
                note: None,
            }
        }
        // A broker without execution history can't auto-advance; surface it but don't fail the
        // caller (a tick should still place today's orders from the stored ledger).
        Err(DomainError::Unsupported(msg)) => ReconcileView {
            ticker: ticker.clone(),
            applied_fills: 0,
            cycles_completed: 0,
            t_before,
            t_after: t_before,
            through: state.reconciled_through,
            note: Some(format!("fills not reconciled: {msg}")),
        },
        Err(e) => return Err(e),
    };
    Ok((state, view))
}

/// The ports a tick drives. Bundling them keeps `place_orders` to a small signature and
/// lets a driving adapter wire one sqlite store as both `repo` and `journal`.
pub struct TickPorts<'a> {
    pub quotes: &'a dyn Quotes,
    pub gateway: &'a dyn OrderGateway,
    pub account: &'a dyn AccountQuery,
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
    /// Fills folded into the ledger by the reconcile step that runs before deciding.
    pub reconciled_fills: usize,
    pub orders: Vec<TickOrder>,
    /// Informational notes, e.g. reconcile being unavailable or KIS 모의 degrading LOC orders.
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
    // Bring the ledger up to date from settled fills *before* deciding today's tranche, so T
    // reflects reality, then decide on that in-memory state. Persist the advance only when
    // executing — a preview reconciles in-memory (accurate) but must not mutate stored state.
    // Read-only at the broker, so it runs regardless of --live; every caller (CLI, future
    // scheduler) inherits a fresh ledger — no placement on a stale T.
    let (state, recon) =
        reconcile_into(ports.account, ports.repo, template, today, execute).await?;
    let reconciled_fills = recon.applied_fills;

    let (quote, intents) = decide_on(ports.quotes, &state, strategy).await?;
    let intents: Vec<OrderIntent> = intents.into_iter().filter(|i| !i.is_noop()).collect();

    // Pre-flight: vet every intent first; any violation aborts the entire tick.
    for intent in &intents {
        risk::vet(intent, &state, quote.price)?;
    }

    let broker = state.broker;
    let ticker = state.ticker.clone();
    let mut notes: Vec<String> = Vec::new();
    if let Some(n) = recon.note {
        notes.push(n);
    }
    if ports.gateway.capabilities().paper_account
        && intents.iter().any(|i| i.kind == OrderKind::LimitOnClose)
    {
        notes.push(
            "KIS 모의(paper) has no true LOC: LOC legs are sent as plain day-limits at the \
             leg's limit price, so they may fill intraday, not at the close"
                .to_string(),
        );
    }
    let note = (!notes.is_empty()).then(|| notes.join(" | "));

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
        reconciled_fills,
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
        Bar, BrokerInfo, Capabilities, Fill, OrderGateway, OrderId, OrderJournal,
        Price as DomainPrice, Shares, Side,
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
            account: &pb,
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
            account: &pb,
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
            account: &pb,
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

    // A state repository that persists in memory — exercises reconcile's save/reload (FlatRepo
    // is always empty, so it can't show the ledger advancing).
    #[derive(Default)]
    struct MemRepo(std::sync::Mutex<std::collections::HashMap<(BrokerId, String), Position>>);
    #[async_trait::async_trait]
    impl StateRepository for MemRepo {
        async fn load(&self, b: BrokerId, t: &Ticker) -> Result<Option<Position>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(b, t.as_str().to_string()))
                .cloned())
        }
        async fn save(&self, p: &Position) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert((p.broker, p.ticker.as_str().to_string()), p.clone());
            Ok(())
        }
        async fn list(&self) -> Result<Vec<Position>> {
            Ok(self.0.lock().unwrap().values().cloned().collect())
        }
    }

    // An account that replays canned fills, filtered by `since` like a real adapter.
    struct CannedAccount {
        fills: Vec<Fill>,
    }
    impl BrokerInfo for CannedAccount {
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
    #[async_trait::async_trait]
    impl AccountQuery for CannedAccount {
        async fn holdings(&self) -> Result<Vec<Holding>> {
            Ok(vec![])
        }
        async fn balance(&self) -> Result<Money> {
            Ok(Money::ZERO)
        }
        async fn fills_since(&self, _ticker: &Ticker, since: time::Date) -> Result<Vec<Fill>> {
            Ok(self
                .fills
                .iter()
                .filter(|f| f.at >= since)
                .cloned()
                .collect())
        }
    }

    // An account whose broker cannot report execution history (e.g. Toss).
    struct NoHistoryAccount;
    impl BrokerInfo for NoHistoryAccount {
        fn id(&self) -> BrokerId {
            BrokerId::Toss
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
    }
    #[async_trait::async_trait]
    impl AccountQuery for NoHistoryAccount {
        async fn holdings(&self) -> Result<Vec<Holding>> {
            Ok(vec![])
        }
        async fn balance(&self) -> Result<Money> {
            Ok(Money::ZERO)
        }
        async fn fills_since(&self, _ticker: &Ticker, _since: time::Date) -> Result<Vec<Fill>> {
            Err(DomainError::Unsupported("no history".into()))
        }
    }

    fn paper_template() -> Position {
        Position::new(
            BrokerId::Paper,
            Ticker::new("TQQQ"),
            Money::new(dec!(32000)),
            40,
        )
    }
    fn buy_fill(shares: u32, price: Decimal, at: time::Date) -> Fill {
        Fill {
            side: Side::Buy,
            shares: Shares::new(shares),
            price: DomainPrice::new(price).unwrap(),
            at,
        }
    }

    #[tokio::test]
    async fn reconcile_applies_fills_persists_and_is_idempotent() {
        let repo = MemRepo::default();
        repo.save(&paper_template()).await.unwrap();
        let account = CannedAccount {
            fills: vec![
                buy_fill(8, dec!(100), date!(2026 - 06 - 18)),
                buy_fill(8, dec!(100), date!(2026 - 06 - 19)),
            ],
        };
        let today = date!(2026 - 06 - 21);

        let view = reconcile(&account, &repo, paper_template(), today)
            .await
            .unwrap();
        assert_eq!(view.applied_fills, 2);
        assert_eq!(view.through, Some(date!(2026 - 06 - 19)));
        assert!(view.t_after > view.t_before);
        assert!(view.note.is_none());

        // Persisted: a reloaded position reflects the fills and the watermark.
        let saved = repo
            .load(BrokerId::Paper, &Ticker::new("TQQQ"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.shares, Shares::new(16));
        assert_eq!(saved.reconciled_through, Some(date!(2026 - 06 - 19)));

        // Idempotent: a second run applies nothing new.
        let again = reconcile(&account, &repo, paper_template(), today)
            .await
            .unwrap();
        assert_eq!(again.applied_fills, 0);
    }

    #[tokio::test]
    async fn reconcile_notes_and_skips_when_history_unsupported() {
        let repo = MemRepo::default();
        let view = reconcile(
            &NoHistoryAccount,
            &repo,
            paper_template(),
            date!(2026 - 06 - 21),
        )
        .await
        .unwrap();
        assert_eq!(view.applied_fills, 0);
        assert!(view.note.is_some());
    }

    #[tokio::test]
    async fn place_orders_reconciles_in_memory_for_preview_persists_on_execute() {
        let pb = PaperBroker::new(Money::new(dec!(0)));
        pb.set_market(Ticker::new("TQQQ"), bar(dec!(100)));
        let strategy = InfiniteBuying::new(InfiniteBuyingConfig::default());
        let journal = MemJournal::default();
        let repo = MemRepo::default();
        repo.save(&paper_template()).await.unwrap();
        // A completed-day buy the tick folds in before deciding today's tranche.
        let account = CannedAccount {
            fills: vec![buy_fill(8, dec!(100), date!(2026 - 06 - 18))],
        };
        let ports = TickPorts {
            quotes: &pb,
            gateway: &pb,
            account: &account,
            repo: &repo,
            journal: &journal,
        };
        let today = date!(2026 - 06 - 21);

        // Preview (execute = false): the reconcile shows in the view (accurate T) but is NOT
        // written to the repo — a preview must not mutate the stored ledger.
        let preview = place_orders(&ports, paper_template(), &strategy, false, false, today)
            .await
            .unwrap();
        assert_eq!(preview.reconciled_fills, 1);
        assert_eq!(preview.t, dec!(1)); // 8 * 100 = 800 against an 800 budget -> T = 1.0
        let after_preview = repo
            .load(BrokerId::Paper, &Ticker::new("TQQQ"))
            .await
            .unwrap()
            .unwrap();
        assert!(after_preview.is_flat()); // preview left the stored ledger untouched
        assert_eq!(after_preview.reconciled_through, None);

        // Execute: the same reconcile is now persisted.
        let executed = place_orders(&ports, paper_template(), &strategy, true, false, today)
            .await
            .unwrap();
        assert_eq!(executed.reconciled_fills, 1);
        let after_execute = repo
            .load(BrokerId::Paper, &Ticker::new("TQQQ"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_execute.shares, Shares::new(8)); // execute persisted the advance
        assert_eq!(
            after_execute.reconciled_through,
            Some(date!(2026 - 06 - 18))
        );
    }
}
