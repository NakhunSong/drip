//! Broker connection factory: builds a live (read-only) broker from a [`SecretStore`].
//!
//! This is shared composition logic used by every driving adapter (the CLI and the web
//! dashboard), so neither has to know how to assemble a `KisBroker`/`TossBroker` from
//! stored secrets. The returned [`LiveBroker`] exposes the read-only ports as trait
//! objects so use cases can stay broker-agnostic.

use crate::{KisBroker, KisConfig, KisDomesticBroker, KisEnv, KisExchange, TossBroker, TossConfig};
use drip_domain::{
    AccountId, AccountQuery, DomainError, OrderGateway, Quotes, Result, SecretStore,
};
use std::path::Path;

/// A connected live broker, dispatching the read-only ports to the concrete adapter.
pub enum LiveBroker {
    Kis(KisBroker),
    KisDomestic(KisDomesticBroker),
    Toss(TossBroker),
}

impl LiveBroker {
    pub fn as_quotes(&self) -> &dyn Quotes {
        match self {
            LiveBroker::Kis(b) => b,
            LiveBroker::KisDomestic(b) => b,
            LiveBroker::Toss(b) => b,
        }
    }
    pub fn as_account(&self) -> &dyn AccountQuery {
        match self {
            LiveBroker::Kis(b) => b,
            LiveBroker::KisDomestic(b) => b,
            LiveBroker::Toss(b) => b,
        }
    }

    /// The order-placement port, if this broker supports it. Both KIS adapters (overseas and
    /// domestic) do; Toss has no paper sandbox, so it stays read-only and returns `None`.
    pub fn as_order_gateway(&self) -> Option<&dyn OrderGateway> {
        match self {
            LiveBroker::Kis(b) => Some(b),
            LiveBroker::KisDomestic(b) => Some(b),
            LiveBroker::Toss(_) => None,
        }
    }
}

/// Build a live broker adapter for `account` from its stored secrets. `account` is the credential
/// namespace (secrets are keyed `{account}_{field}`) and `env` (`paper` | `real`, from the
/// account's config) selects the KIS server; `broker` (`kis` | `kis-domestic` | `toss`) picks the
/// adapter. `cache_dir` (the drip home) is where KIS persists its OAuth token and rate-limit
/// timestamp across processes; `None` keeps them in-memory only.
pub fn connect(
    broker: &str,
    account: &str,
    env: &str,
    secrets: &dyn SecretStore,
    cache_dir: Option<&Path>,
) -> Result<LiveBroker> {
    match broker {
        "kis" => Ok(LiveBroker::Kis(KisBroker::new(
            kis_config(secrets, account, env)?,
            cache_dir,
        )?)),
        "kis-domestic" => Ok(LiveBroker::KisDomestic(KisDomesticBroker::new(
            kis_config(secrets, account, env)?,
            cache_dir,
        )?)),
        "toss" => {
            let account_seq = require(secrets, account, "account_seq")?
                .parse()
                .map_err(|e| {
                    DomainError::Config(format!("{account}_account_seq must be an integer: {e}"))
                })?;
            let config = TossConfig {
                app_key: require(secrets, account, "app_key")?,
                app_secret: require(secrets, account, "app_secret")?,
                account_seq,
            };
            Ok(LiveBroker::Toss(TossBroker::new(config)?))
        }
        other => Err(DomainError::Config(format!(
            "broker '{other}' has no live adapter (use kis|kis-domestic|toss)"
        ))),
    }
}

/// Build a [`KisConfig`] from `account`'s stored secrets and its `env` — shared by the overseas
/// (`kis`) and domestic (`kis-domestic`) adapters, which use the same account credentials.
fn kis_config(secrets: &dyn SecretStore, account: &str, env: &str) -> Result<KisConfig> {
    let environment = match env {
        "real" => KisEnv::Real,
        "paper" => KisEnv::Paper,
        other => {
            return Err(DomainError::Config(format!(
                "account '{account}' env must be real|paper, got '{other}'"
            )));
        }
    };
    Ok(KisConfig {
        environment,
        app_key: require(secrets, account, "app_key")?,
        app_secret: require(secrets, account, "app_secret")?,
        cano: require(secrets, account, "cano")?,
        product_code: require(secrets, account, "product_code")?,
        exchange: parse_exchange(&require(secrets, account, "exchange")?)?,
    })
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

/// Fetch a required account-scoped secret, keyed by [`AccountId::secret_key`].
fn require(secrets: &dyn SecretStore, account: &str, field: &str) -> Result<String> {
    let key = AccountId::secret_key(account, field);
    secrets.get(&key)?.ok_or_else(|| {
        DomainError::Config(format!(
            "missing secret '{key}' — run `drip account add` first"
        ))
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

    fn seed_kis(secrets: &MapSecrets, account: &str) {
        for (field, v) in [
            ("app_key", "k"),
            ("app_secret", "s"),
            ("cano", "12345678"),
            ("product_code", "01"),
            ("exchange", "nasdaq"),
        ] {
            secrets.set(&format!("{account}_{field}"), v).unwrap();
        }
    }

    #[test]
    fn connect_kis_requires_all_account_secrets() {
        let secrets = MapSecrets::default();
        assert!(connect("kis", "kis-paper", "paper", &secrets, None).is_err()); // nothing stored
        seed_kis(&secrets, "kis-paper");
        assert!(matches!(
            connect("kis", "kis-paper", "paper", &secrets, None).unwrap(),
            LiveBroker::Kis(_)
        ));
    }

    #[test]
    fn connect_resolves_creds_by_account_not_broker() {
        // Two KIS accounts hold different credentials under their own prefixes; `connect` reads
        // the one named, so 모의 and 실전 never cross. Distinct exchanges prove which was read.
        let secrets = MapSecrets::default();
        seed_kis(&secrets, "kis-paper");
        secrets.set("kis-real_app_key", "real").unwrap();
        secrets.set("kis-real_app_secret", "rs").unwrap();
        secrets.set("kis-real_cano", "99999999").unwrap();
        secrets.set("kis-real_product_code", "01").unwrap();
        secrets.set("kis-real_exchange", "nyse").unwrap();
        assert!(matches!(
            connect("kis-domestic", "kis-real", "real", &secrets, None).unwrap(),
            LiveBroker::KisDomestic(_)
        ));
        // The paper account's creds alone don't satisfy a real-account connect on another name.
        assert!(connect("kis", "kis-missing", "real", &secrets, None).is_err());
    }

    #[test]
    fn connect_kis_rejects_a_bad_env() {
        let secrets = MapSecrets::default();
        seed_kis(&secrets, "kis-paper");
        assert!(connect("kis", "kis-paper", "vts", &secrets, None).is_err());
    }

    #[test]
    fn unknown_broker_is_an_error() {
        assert!(connect("nope", "any", "paper", &MapSecrets::default(), None).is_err());
    }
}
