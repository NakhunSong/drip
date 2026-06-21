//! Logging setup via `tracing`. Verbosity is controlled by the `DRIP_LOG` env var (e.g.
//! `DRIP_LOG=debug`), defaulting to `info`. Secrets are never passed to tracing — redaction
//! is by discipline, not by filter.

use tracing_subscriber::EnvFilter;

/// Initialize the global tracing subscriber. Safe to call once; later calls are ignored.
pub fn init() {
    let filter = EnvFilter::try_from_env("DRIP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .try_init();
}
