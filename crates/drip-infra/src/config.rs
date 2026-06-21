//! Application configuration: the set of configured positions, persisted as TOML. Secrets
//! (API keys) live separately in the secret store, never in this file.

use drip_domain::{BrokerId, DomainError, Money, Position, Result, Ticker};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub positions: Vec<PositionConfig>,
}

/// One configured position: which strategy trades which ticker on which broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionConfig {
    /// Unique name, e.g. `tqqq-kis`.
    pub name: String,
    /// Broker key: `kis` | `toss` | `paper`.
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

    /// Insert a position, replacing any existing one with the same name.
    pub fn upsert(&mut self, position: PositionConfig) {
        match self.positions.iter_mut().find(|p| p.name == position.name) {
            Some(existing) => *existing = position,
            None => self.positions.push(position),
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
        config.upsert(PositionConfig {
            name: "tqqq-kis".into(),
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
    }

    #[test]
    fn upsert_replaces_same_name() {
        let mut config = AppConfig::default();
        let mut p = PositionConfig {
            name: "x".into(),
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
