//! Broker connection factory: builds a live (read-only) broker from a [`SecretStore`].
//!
//! This is shared composition logic used by every driving adapter (the CLI and the web
//! dashboard), so neither has to know how to assemble a `KisBroker`/`TossBroker` from
//! stored secrets. The returned [`LiveBroker`] exposes the read-only ports as trait
//! objects so use cases can stay broker-agnostic.

use crate::{KisBroker, KisConfig, KisEnv, KisExchange, TossBroker, TossConfig};
use drip_domain::{AccountQuery, DomainError, OrderGateway, Quotes, Result, SecretStore};
use std::path::Path;

/// A connected live broker, dispatching the read-only ports to the concrete adapter.
pub enum LiveBroker {
    Kis(KisBroker),
    Toss(TossBroker),
}

impl LiveBroker {
    pub fn as_quotes(&self) -> &dyn Quotes {
        match self {
            LiveBroker::Kis(b) => b,
            LiveBroker::Toss(b) => b,
        }
    }
    pub fn as_account(&self) -> &dyn AccountQuery {
        match self {
            LiveBroker::Kis(b) => b,
            LiveBroker::Toss(b) => b,
        }
    }

    /// The order-placement port, if this broker supports it. KIS does (M2); Toss does not —
    /// it has no paper sandbox, so live Toss placement is deferred to a later, separately
    /// gated increment. Returning `None` keeps Toss read-only.
    pub fn as_order_gateway(&self) -> Option<&dyn OrderGateway> {
        match self {
            LiveBroker::Kis(b) => Some(b),
            LiveBroker::Toss(_) => None,
        }
    }
}

/// Build a live broker by name (`kis` | `toss`) from stored secrets. `cache_dir` (the drip home)
/// is where KIS persists its OAuth token and rate-limit timestamp across processes; `None` keeps
/// them in-memory only.
pub fn connect(
    broker: &str,
    secrets: &dyn SecretStore,
    cache_dir: Option<&Path>,
) -> Result<LiveBroker> {
    match broker {
        "kis" => {
            let environment = match require(secrets, "kis_env")?.as_str() {
                "real" => KisEnv::Real,
                "paper" => KisEnv::Paper,
                other => {
                    return Err(DomainError::Config(format!(
                        "kis_env must be real|paper, got '{other}'"
                    )));
                }
            };
            let config = KisConfig {
                environment,
                app_key: require(secrets, "kis_app_key")?,
                app_secret: require(secrets, "kis_app_secret")?,
                cano: require(secrets, "kis_cano")?,
                product_code: require(secrets, "kis_product_code")?,
                exchange: parse_exchange(&require(secrets, "kis_exchange")?)?,
            };
            Ok(LiveBroker::Kis(KisBroker::new(config, cache_dir)?))
        }
        "toss" => {
            let account_seq = require(secrets, "toss_account_seq")?.parse().map_err(|e| {
                DomainError::Config(format!("toss_account_seq must be an integer: {e}"))
            })?;
            let config = TossConfig {
                app_key: require(secrets, "toss_app_key")?,
                app_secret: require(secrets, "toss_app_secret")?,
                account_seq,
            };
            Ok(LiveBroker::Toss(TossBroker::new(config)?))
        }
        other => Err(DomainError::Config(format!(
            "broker '{other}' has no live adapter (use kis|toss)"
        ))),
    }
}

/// Parse an exchange name into a [`KisExchange`].
pub fn parse_exchange(raw: &str) -> Result<KisExchange> {
    match raw.to_lowercase().as_str() {
        "nasdaq" | "nas" => Ok(KisExchange::Nasdaq),
        "nyse" | "nys" => Ok(KisExchange::Nyse),
        "amex" | "ams" => Ok(KisExchange::Amex),
        other => Err(DomainError::Config(format!(
            "unknown exchange '{other}' (use nasdaq|nyse|amex)"
        ))),
    }
}

fn require(secrets: &dyn SecretStore, key: &str) -> Result<String> {
    secrets.get(key)?.ok_or_else(|| {
        DomainError::Config(format!("missing secret '{key}' — run `drip keys` first"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MapSecrets(Mutex<HashMap<String, String>>);
    impl SecretStore for MapSecrets {
        fn get(&self, key: &str) -> Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn connect_kis_requires_all_secrets() {
        let secrets = MapSecrets::default();
        assert!(connect("kis", &secrets, None).is_err()); // nothing stored yet
        for (k, v) in [
            ("kis_env", "paper"),
            ("kis_app_key", "k"),
            ("kis_app_secret", "s"),
            ("kis_cano", "12345678"),
            ("kis_product_code", "01"),
            ("kis_exchange", "nasdaq"),
        ] {
            secrets.set(k, v).unwrap();
        }
        assert!(matches!(
            connect("kis", &secrets, None).unwrap(),
            LiveBroker::Kis(_)
        ));
    }

    #[test]
    fn unknown_broker_is_an_error() {
        assert!(connect("nope", &MapSecrets::default(), None).is_err());
    }
}
