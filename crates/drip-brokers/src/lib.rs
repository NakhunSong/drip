//! `drip-brokers` — broker adapters implementing the domain ports.
//!
//! Each adapter implements only the capability ports it actually supports
//! (Interface Segregation): the [`PaperBroker`] simulator and the live [`KisBroker`] (US
//! overseas) and [`KisDomesticBroker`] (KRX) adapters implement [`drip_domain::OrderGateway`];
//! [`TossBroker`] implements read-only quotes and account queries but **not** `OrderGateway`
//! (no 모의 sandbox). Going live is guarded at runtime, not by the type system (ADR-7).

pub mod connect;
mod http;
pub mod kis;
pub mod kis_domestic;
mod kis_session;
pub mod paper;
pub mod toss;

pub use connect::{LiveBroker, connect, parse_exchange};
pub use kis::{KisBroker, KisConfig, KisEnv, KisExchange};
pub use kis_domestic::KisDomesticBroker;
pub use paper::PaperBroker;
pub use toss::{TossBroker, TossConfig};
