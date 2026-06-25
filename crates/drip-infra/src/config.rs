//! Application configuration: the set of configured positions, persisted as TOML. Secrets
//! (API keys) live separately in the secret store, never in this file.

use drip_domain::{AccountId, BrokerId, DomainError, Money, Position, Result, Ticker};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// The accounts positions trade under — each a credential + environment bundle. Credentials
    /// live in the secret store (keyed `{account}_{field}`); only the non-secret name/env are here.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub positions: Vec<PositionConfig>,
}

/// One trading account: a named (environment, credentials) bundle. For KIS the `env` separates
/// 모의 (`paper`) from 실전 (`real`); it is ignored for brokers without that distinction.
/// Credentials are not stored here — they live in the secret store under `{name}_{field}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Unique account name, e.g. `kis-paper` | `kis-real` | `toss`.
    pub name: String,
    /// `paper` | `real` (KIS only; ignored otherwise).
    pub env: String,
}

/// One configured position: which strategy trades which ticker, on which account + broker adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionConfig {
    /// Unique name, e.g. `tqqq-kis`.
    pub name: String,
    /// The account (credentials + environment) this position trades under, e.g. `kis-paper`.
    /// `#[serde(default)]` keeps pre-account configs loadable; [`crate::migrate`] fills it in.
    #[serde(default)]
    pub account: String,
    /// Broker adapter key: `kis` | `kis-domestic` | `toss` | `paper`.
    pub broker: String,
    pub ticker: String,
    pub seed: Decimal,
    pub splits: u32,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Optional take-profit override in percent (TQQQ 15, SOXL 20); defaults to the
    /// strategy's own default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit_pct: Option<Decimal>,
}

fn default_strategy() -> String {
    "infinite-buying".to_string()
}

impl PositionConfig {
    /// The flat starting [`Position`] this config describes.
    pub fn to_position(&self) -> Result<Position> {
        let broker: BrokerId = self.broker.parse()?;
        Ok(Position::new(
            AccountId::new(&self.account),
            broker,
            Ticker::new(&self.ticker),
            Money::new(self.seed),
            self.splits,
        ))
    }

    /// The strategy configuration value (splits + optional take-profit override) passed to
    /// the strategy registry.
    pub fn strategy_params(&self) -> serde_json::Value {
        let mut params = serde_json::Map::new();
        params.insert("splits".to_string(), serde_json::Value::from(self.splits));
        if let Some(take_profit) = self.take_profit_pct
            && let Ok(value) = serde_json::to_value(take_profit)
        {
            params.insert("take_profit_pct".to_string(), value);
        }
        serde_json::Value::Object(params)
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<AppConfig> {
        if !path.exists() {
            return Ok(AppConfig::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| DomainError::Config(format!("read {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| DomainError::Config(format!("parse config: {e}")))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string(self)
            .map_err(|e| DomainError::Config(format!("serialize config: {e}")))?;
        std::fs::write(path, text)
            .map_err(|e| DomainError::Config(format!("write {}: {e}", path.display())))
    }

    pub fn find(&self, name: &str) -> Option<&PositionConfig> {
        self.positions.iter().find(|p| p.name == name)
    }

    /// Look up a configured account by name (its environment + credential namespace).
    pub fn find_account(&self, name: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.name == name)
    }

    /// The configured environment (`paper`|`real`) for `account`, defaulting to `paper` when the
    /// account isn't registered. The single home for this safety default: the broker connection is
    /// account-scoped, so the 모의/실전 choice must resolve identically for every driving adapter
    /// (CLI and web). `status` reads [`find_account`](AppConfig::find_account) directly because it
    /// must distinguish a missing account from a paper one.
    pub fn env_for(&self, account: &str) -> String {
        self.find_account(account)
            .map(|a| a.env.clone())
            .unwrap_or_else(|| "paper".to_string())
    }

    /// Insert a position, replacing any existing one with the same name.
    pub fn upsert(&mut self, position: PositionConfig) {
        match self.positions.iter_mut().find(|p| p.name == position.name) {
            Some(existing) => *existing = position,
            None => self.positions.push(position),
        }
    }

    /// Insert an account, replacing any existing one with the same name.
    pub fn upsert_account(&mut self, account: AccountConfig) {
        match self.accounts.iter_mut().find(|a| a.name == account.name) {
            Some(existing) => *existing = account,
            None => self.accounts.push(account),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_trips_through_toml() {
        let mut config = AppConfig::default();
        config.upsert_account(AccountConfig {
            name: "kis-paper".into(),
            env: "paper".into(),
        });
        config.upsert(PositionConfig {
            name: "tqqq-kis".into(),
            account: "kis-paper".into(),
            broker: "kis".into(),
            ticker: "TQQQ".into(),
            seed: dec!(4000),
            splits: 40,
            strategy: "infinite-buying".into(),
            take_profit_pct: Some(dec!(15)),
        });
        let text = toml::to_string(&config).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.positions.len(), 1);
        assert_eq!(parsed.find("tqqq-kis").unwrap().seed, dec!(4000));
        assert_eq!(parsed.find("tqqq-kis").unwrap().account, "kis-paper");
        assert_eq!(parsed.find_account("kis-paper").unwrap().env, "paper");
    }

    #[test]
    fn upsert_replaces_same_name() {
        let mut config = AppConfig::default();
        let mut p = PositionConfig {
            name: "x".into(),
            account: "paper".into(),
            broker: "paper".into(),
            ticker: "TQQQ".into(),
            seed: dec!(1000),
            splits: 40,
            strategy: "infinite-buying".into(),
            take_profit_pct: None,
        };
        config.upsert(p.clone());
        p.seed = dec!(2000);
        config.upsert(p);
        assert_eq!(config.positions.len(), 1);
        assert_eq!(config.find("x").unwrap().seed, dec!(2000));
    }
}
