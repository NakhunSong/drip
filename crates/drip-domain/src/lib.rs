//! `drip-domain` — the pure hexagonal core: domain model and ports.
//!
//! This crate has no I/O, no async runtime, and no framework dependencies. Outer crates
//! (`drip-strategies`, `drip-brokers`, `drip-infra`, `drip-app`) implement the traits in
//! [`ports`]; dependencies always point inward to here. See `ARCHITECTURE.md`.

pub mod calendar;
pub mod error;
pub mod market;
pub mod money;
pub mod order;
pub mod ports;
pub mod position;
pub mod risk;
pub mod schedule;
pub mod settlement;

pub use error::{DomainError, Result};
pub use market::{Bar, BrokerId, MarketSnapshot, OrderId, Quote, Side, Ticker};
pub use money::{Money, Percent, Price, Shares};
pub use order::{OrderIntent, OrderKind};
pub use ports::{
    AccountQuery, BrokerInfo, Capabilities, DailyContext, MarketDataSource, OrderGateway,
    OrderJournal, Quotes, SecretStore, StateRepository, Strategy,
};
pub use position::{Fill, Holding, Position, ReconcileOutcome};
pub use schedule::{Schedule, Trigger};
pub use settlement::settle;
