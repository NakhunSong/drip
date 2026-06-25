//! Ports — the abstractions the application depends on; adapters in outer crates
//! implement them. Nothing here knows about HTTP, sqlite, or any concrete technology, so
//! every dependency points inward to the domain (Dependency Inversion Principle).
//!
//! The broker surface is intentionally split into small, role-focused traits
//! (Interface Segregation): an adapter implements only what it actually supports. In M1
//! the live brokers (KIS, Toss) implement [`Quotes`] + [`AccountQuery`] only — they do
//! **not** implement [`OrderGateway`], so there is no type-level path to place a real
//! order yet. That is how "read-only live integration" is guaranteed by the compiler.

use crate::error::Result;
use crate::market::{AccountId, Bar, BrokerId, MarketSnapshot, OrderId, Quote, Ticker};
use crate::money::Money;
use crate::order::OrderIntent;
use crate::position::{Fill, Holding, Position};
use crate::schedule::{Schedule, Trigger};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::Date;

/// Read-only inputs handed to a [`Strategy`] each trading day.
#[derive(Debug, Clone)]
pub struct DailyContext<'a> {
    pub position: &'a Position,
    pub market: &'a MarketSnapshot,
}

/// A trading strategy: a pure, deterministic function from (state, market) to order
/// intents. No I/O and no side effects — which is exactly what makes a strategy easy to
/// unit-test, backtest, and (later) express as a sandboxed Rhai script. This is the
/// system's primary extension point (Open/Closed Principle).
pub trait Strategy: Send + Sync {
    /// Stable identifier, e.g. `"infinite-buying"`.
    fn name(&self) -> &str;
    /// Decide today's orders. MUST be deterministic for a given context.
    fn decide(&self, ctx: &DailyContext<'_>) -> Vec<OrderIntent>;
    /// When the engine should call [`decide`](Strategy::decide). The default is a single daily
    /// schedule before the US open, which suits batch strategies like infinite-buying;
    /// streaming strategies override this once realtime triggers land (M3).
    fn triggers(&self) -> Vec<Trigger> {
        vec![Trigger::Schedule(Schedule::daily_before_open())]
    }
}

/// Optional capabilities a broker adapter may support. The engine reads these to degrade
/// gracefully (e.g. poll quotes when realtime streaming is unavailable, as on Toss today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub realtime_quotes: bool,
    pub paper_account: bool,
    pub order_placement: bool,
    pub overseas: bool,
}

/// Identity and capability introspection shared by every broker adapter.
pub trait BrokerInfo: Send + Sync {
    fn id(&self) -> BrokerId;
    fn capabilities(&self) -> Capabilities;
}

/// Pull a current quote over REST. The lowest common denominator — every broker adapter
/// can do at least this (KIS, Toss, and the paper simulator).
#[async_trait]
pub trait Quotes: BrokerInfo {
    async fn quote(&self, ticker: &Ticker) -> Result<Quote>;
}

/// Read account state. Read-only, hence safe to wire against live brokers.
#[async_trait]
pub trait AccountQuery: BrokerInfo {
    async fn holdings(&self) -> Result<Vec<Holding>>;
    async fn balance(&self) -> Result<Money>;
    /// Executed fills for `ticker` with a trade date on or after `since`, in **chronological
    /// (ascending) order** — [`Position::reconcile`] relies on it to resolve same-day cycle
    /// boundaries. Only filled quantity is reported (a [`Fill`] carries no ticker, so the
    /// adapter must scope to the one requested). `since` is a lower bound the adapter passes to
    /// the broker; callers dedup against [`Position::reconciled_through`], so returning a few
    /// already-seen days is harmless. Adapters that cannot report history return
    /// [`Unsupported`](crate::DomainError::Unsupported).
    async fn fills_since(&self, ticker: &Ticker, since: Date) -> Result<Vec<Fill>>;
}

/// Place and cancel real orders. Deliberately a separate port (Interface Segregation): in
/// M1 only the paper broker implements it; M2 adds the KIS live adapter.
#[async_trait]
pub trait OrderGateway: BrokerInfo {
    async fn place(&self, ticker: &Ticker, order: &OrderIntent) -> Result<OrderId>;
    async fn cancel(&self, id: &OrderId) -> Result<()>;
}

/// An idempotency ledger for placed orders. A driving adapter reserves a stable client
/// key *before* sending an order and records the broker id *after*, so a crash or a
/// same-day re-run never places the same order twice (at-most-once). Kept separate from
/// [`StateRepository`] by Interface Segregation, though one sqlite store implements both.
#[async_trait]
pub trait OrderJournal: Send + Sync {
    /// Reserve `key`; `true` if newly reserved, `false` if already present (the caller must
    /// then skip — the order was placed, or reserved by an earlier run today).
    async fn reserve(&self, key: &str) -> Result<bool>;
    /// Attach the broker order id to a previously reserved `key`.
    async fn record(&self, key: &str, order_id: &OrderId) -> Result<()>;
}

/// A source of historical daily bars for backtesting.
#[async_trait]
pub trait MarketDataSource: Send + Sync {
    async fn bars(&self, ticker: &Ticker, from: Date, to: Date) -> Result<Vec<Bar>>;
}

/// Persistence for positions across runs. Implemented by sqlite in `drip-infra`. Positions are
/// keyed by `(account, ticker)`: the account is the isolation namespace (see [`AccountId`]), so a
/// real position never shares a ledger row with a paper one on the same ticker.
#[async_trait]
pub trait StateRepository: Send + Sync {
    async fn load(&self, account: &AccountId, ticker: &Ticker) -> Result<Option<Position>>;
    async fn save(&self, position: &Position) -> Result<()>;
    async fn list(&self) -> Result<Vec<Position>>;
}

/// Secret storage backed by the OS keychain in `drip-infra`. Implementations must never
/// log secret values.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}
