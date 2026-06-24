//! `drip-brokers` — broker adapters implementing the domain ports.
//!
//! Each adapter implements only the capability ports it actually supports
//! (Interface Segregation): the [`PaperBroker`] simulator does everything, while the live
//! [`KisBroker`] and [`TossBroker`] adapters implement read-only quotes and account queries
//! but **not** [`drip_domain::OrderGateway`] — so there is no type-level path to place a
//! real order in M1.

pub mod connect;
mod http;
pub mod kis;
pub mod kis_domestic;
pub mod paper;
pub mod toss;

pub use connect::{LiveBroker, connect, parse_exchange};
pub use kis::{KisBroker, KisConfig, KisEnv, KisExchange};
pub use kis_domestic::KisDomesticBroker;
pub use paper::PaperBroker;
pub use toss::{TossBroker, TossConfig};
