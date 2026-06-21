//! `drip-app` — application use cases that orchestrate the domain ports. Driving adapters
//! (the CLI and the web dashboard) call these; composition (building a broker, opening
//! sqlite) stays in the adapter. This keeps business logic in one place, reused by both.

pub mod backtest;
pub mod usecases;

pub use backtest::{Backtest, BacktestReport};
pub use usecases::{
    AccountView, DryRunView, TickOrder, TickPorts, TickStatus, TickView, account_snapshot, dry_run,
    fetch_quote, list_positions, place_orders, run_backtest,
};
