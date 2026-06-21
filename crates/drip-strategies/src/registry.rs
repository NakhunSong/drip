//! The strategy registry — the Open/Closed seam. The CLI builds a strategy by name from
//! a JSON config value; to add a new strategy you register one builder here and nothing
//! downstream changes. This is also where Rhai user-strategies will register later.

use crate::infinite_buying::{InfiniteBuying, InfiniteBuyingConfig};
use drip_domain::{DomainError, Result, Strategy};
use serde_json::Value;
use std::collections::HashMap;

/// Builds a boxed [`Strategy`] from a JSON configuration value.
type Builder = fn(&Value) -> Result<Box<dyn Strategy>>;

/// Maps strategy names to their builders.
#[derive(Default)]
pub struct StrategyRegistry {
    builders: HashMap<&'static str, Builder>,
}

impl std::fmt::Debug for StrategyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyRegistry")
            .field("names", &self.names())
            .finish()
    }
}

impl StrategyRegistry {
    /// A registry pre-populated with the built-in strategies.
    pub fn with_builtins() -> Self {
        let mut registry = StrategyRegistry::default();
        registry.register("infinite-buying", build_infinite_buying);
        registry
    }

    pub fn register(&mut self, name: &'static str, builder: Builder) {
        self.builders.insert(name, builder);
    }

    /// Sorted list of known strategy names.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.builders.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Build a strategy by name. `config` may be [`Value::Null`] to use defaults.
    pub fn build(&self, name: &str, config: &Value) -> Result<Box<dyn Strategy>> {
        let builder = self
            .builders
            .get(name)
            .ok_or_else(|| DomainError::Strategy(format!("unknown strategy: {name}")))?;
        builder(config)
    }
}

fn build_infinite_buying(config: &Value) -> Result<Box<dyn Strategy>> {
    let cfg: InfiniteBuyingConfig = if config.is_null() {
        InfiniteBuyingConfig::default()
    } else {
        serde_json::from_value(config.clone())
            .map_err(|e| DomainError::Config(format!("infinite-buying config: {e}")))?
    };
    Ok(Box::new(InfiniteBuying::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_infinite_buying_with_defaults_from_null() {
        let registry = StrategyRegistry::with_builtins();
        let strategy = registry.build("infinite-buying", &Value::Null).unwrap();
        assert_eq!(strategy.name(), "infinite-buying");
    }

    #[test]
    fn builds_infinite_buying_with_overrides() {
        let registry = StrategyRegistry::with_builtins();
        let strategy = registry
            .build(
                "infinite-buying",
                &json!({ "splits": 20, "take_profit_pct": 20 }),
            )
            .unwrap();
        assert_eq!(strategy.name(), "infinite-buying");
    }

    #[test]
    fn unknown_strategy_is_an_error() {
        let registry = StrategyRegistry::with_builtins();
        assert!(registry.build("nope", &Value::Null).is_err());
    }

    #[test]
    fn lists_builtin_names() {
        assert_eq!(
            StrategyRegistry::with_builtins().names(),
            vec!["infinite-buying"]
        );
    }
}
