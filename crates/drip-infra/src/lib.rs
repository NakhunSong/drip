//! `drip-infra` — concrete adapters for the domain ports: filesystem config and secrets,
//! sqlite state, CSV market data, and logging setup. Everything here is
//! a leaf that the CLI composes; nothing in the inner layers depends on it.

pub mod config;
pub mod data;
pub mod logging;
pub mod secrets;
pub mod state;

use drip_domain::{DomainError, Result};
use std::path::PathBuf;

pub use config::{AppConfig, PositionConfig};
pub use data::CsvMarketData;
pub use secrets::FileSecretStore;
pub use state::SqliteStateRepository;

/// The drip home directory (`~/.drip`), holding config, secrets, and state.
pub fn drip_home() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| DomainError::Config("cannot resolve home directory".into()))?;
    Ok(home.join(".drip"))
}

/// Ensure `~/.drip` exists and return it.
pub fn ensure_home() -> Result<PathBuf> {
    let home = drip_home()?;
    std::fs::create_dir_all(&home)
        .map_err(|e| DomainError::Config(format!("create {}: {e}", home.display())))?;
    Ok(home)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(drip_home()?.join("config.toml"))
}
pub fn secrets_path() -> Result<PathBuf> {
    Ok(drip_home()?.join("secrets.toml"))
}
pub fn state_path() -> Result<PathBuf> {
    Ok(drip_home()?.join("state.db"))
}

/// Parse a `YYYY-MM-DD` date.
pub fn parse_date(raw: &str) -> Result<time::Date> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(raw.trim(), &format)
        .map_err(|e| DomainError::Config(format!("invalid date '{raw}': {e}")))
}
