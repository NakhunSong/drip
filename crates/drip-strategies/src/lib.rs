//! `drip-strategies` — built-in trading strategies plus the registry that constructs
//! them by name. Adding a strategy means adding an adapter here and registering it; the
//! engine and broker layers never change (Open/Closed Principle). The same registry is
//! the seam where sandboxed Rhai user-strategies plug in later.

pub mod infinite_buying;
pub mod registry;

pub use infinite_buying::{InfiniteBuying, InfiniteBuyingConfig};
pub use registry::StrategyRegistry;
