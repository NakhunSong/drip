//! One-time, idempotent migration of an existing drip home to the account model.
//!
//! Pre-account drip kept a single global KIS environment (`kis_env`) with one flat set of `kis_*`
//! secrets, and keyed the positions ledger by `(broker, ticker)`. The account model makes the
//! account (e.g. `kis-paper` vs `kis-real`) the isolation namespace for credentials, config, and
//! the ledger, so 모의 and 실전 never share state. This module rewrites an old home in place:
//!
//!   * secrets — `kis_*` → `kis-{env}_*`, then the flat keys are removed;
//!   * config  — each position gains an `account`, and an `[[accounts]]` table is synthesized;
//!   * state   — the `positions` table is rebuilt keyed by `(account, ticker)`, data-preserving.
//!
//! Every step is idempotent: a home already on the account model is left untouched. The state
//! rewrite backs up `state.db` first (SQLite cannot alter a primary key in place). The order
//! captures `kis_env` before the secrets step deletes it, so a crash before the (last) secrets
//! step leaves the env signal intact for a clean re-run.

use crate::config::{AccountConfig, AppConfig};
use crate::state::POSITIONS_COLUMNS;
use drip_domain::{AccountId, DomainError, Position, Result, SecretStore};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What [`migrate_to_accounts`] changed.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Accounts synthesized from pre-account positions (e.g. `kis-paper`).
    pub accounts: Vec<String>,
    /// Where `state.db` was backed up before the ledger rewrite, if it was rewritten.
    pub state_backup: Option<PathBuf>,
}

impl MigrationReport {
    /// True when the migration actually changed something (so the CLI can stay quiet otherwise).
    pub fn did_anything(&self) -> bool {
        !self.accounts.is_empty() || self.state_backup.is_some()
    }
}

/// Migrate `config.toml`, the secret store, and `state.db` from the pre-account layout to the
/// account model. Idempotent; safe to call on every startup.
pub fn migrate_to_accounts(
    config_path: &Path,
    secrets: &dyn SecretStore,
    state_path: &Path,
) -> Result<MigrationReport> {
    // The old global environment drives every derived account name. Captured before the secrets
    // step deletes `kis_env`; defaults to `paper` (the historical default) when never set.
    let env = secrets
        .get("kis_env")?
        .unwrap_or_else(|| "paper".to_string());

    let state_backup = migrate_state(state_path, &env)?;
    let accounts = migrate_config(config_path, &env)?;
    migrate_secrets(secrets, &env)?;
    Ok(MigrationReport {
        accounts,
        state_backup,
    })
}

/// The account a pre-account position belongs to, from its broker and the old global env. Both
/// KIS adapters (`kis`, `kis-domestic`) share one account per environment; other brokers map to a
/// single account named after the broker.
fn account_for(broker: &str, env: &str) -> String {
    match broker {
        "kis" | "kis-domestic" => format!("kis-{env}"),
        other => other.to_string(),
    }
}

/// Rebuild the `positions` table keyed by `(account, ticker)`, assigning each row's account from
/// its broker and `env`. Returns the backup path if a rewrite happened, `None` if already
/// migrated (or no database yet). Data-preserving: each position is round-tripped through the
/// domain type with only its `account` set, so every other ledger field is carried over verbatim.
fn migrate_state(state_path: &Path, env: &str) -> Result<Option<PathBuf>> {
    if !state_path.exists() {
        return Ok(None); // fresh install — `open` will create the account-keyed schema
    }
    let conn = Connection::open(state_path).map_err(storage_err)?;
    if !is_legacy_positions(&conn)? {
        return Ok(None); // already account-keyed (or no positions table) — nothing to do
    }

    // Read every legacy row before touching the schema.
    let legacy: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT broker, data FROM positions")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_err)?
    };

    // Back up before the destructive rebuild (SQLite can't alter a primary key in place).
    let backup = state_path.with_extension("db.pre-account.bak");
    std::fs::copy(state_path, &backup)
        .map_err(|e| DomainError::Storage(format!("back up state.db: {e}")))?;

    conn.execute_batch(&format!("CREATE TABLE positions_new ({POSITIONS_COLUMNS})"))
        .map_err(storage_err)?;

    for (broker, data) in legacy {
        // Legacy JSON has no `account` (serde default ""): set it from broker + env, re-serialize,
        // and re-key the row. Every other field deserializes and serializes unchanged.
        let mut position: Position = serde_json::from_str(&data).map_err(storage_err)?;
        position.account = AccountId::new(account_for(&broker, env));
        let data = serde_json::to_string(&position).map_err(storage_err)?;
        // Plain INSERT (not OR REPLACE): the broker→account mapping can't collide two source rows
        // onto one (account, ticker) today, but if that ever changed this fails loud rather than
        // silently dropping a ledger row.
        conn.execute(
            "INSERT INTO positions_new (account, ticker, data) VALUES (?1, ?2, ?3)",
            (position.account.as_str(), position.ticker.as_str(), data),
        )
        .map_err(storage_err)?;
    }

    conn.execute_batch(
        "DROP TABLE positions;
         ALTER TABLE positions_new RENAME TO positions;",
    )
    .map_err(storage_err)?;

    rekey_order_journal(&conn, env)?;
    Ok(Some(backup))
}

/// Re-prefix `order_journal` client keys from broker- to account-namespaced, the same mapping the
/// positions re-key uses. A client key is `{prefix}:ticker:date:tag`; the prefix was the broker
/// (`kis`/`toss`/`paper`) and is now the account. Without this, a same-day re-tick across the
/// upgrade would compute an account-prefixed key that the broker-prefixed journal doesn't hold,
/// re-reserve it, and place the order again — an over-buy. Idempotent: an already-account prefix
/// maps to itself, so re-running changes nothing.
fn rekey_order_journal(conn: &Connection, env: &str) -> Result<()> {
    if !table_exists(conn, "order_journal")? {
        return Ok(()); // pre-M2 db, or none placed yet — nothing to re-key
    }
    let keys: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT client_key FROM order_journal")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_err)?
    };
    for key in keys {
        if let Some((prefix, rest)) = key.split_once(':') {
            let account = account_for(prefix, env);
            if account != prefix {
                conn.execute(
                    "UPDATE order_journal SET client_key = ?1 WHERE client_key = ?2",
                    (format!("{account}:{rest}"), &key),
                )
                .map_err(storage_err)?;
            }
        }
    }
    Ok(())
}

/// Whether a table exists in the database.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .map_err(storage_err)?
        .exists([name])
        .map_err(storage_err)
}

/// Whether the `positions` table is the pre-account schema (a `broker` column, no `account`). A
/// missing table (fresh or partially-built db) is not legacy — there is nothing to migrate.
fn is_legacy_positions(conn: &Connection) -> Result<bool> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info('positions')")
        .map_err(storage_err)?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(storage_err)?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(storage_err)?;
    Ok(cols.iter().any(|c| c == "broker") && cols.iter().all(|c| c != "account"))
}

/// Give every un-accounted position an account and synthesize the `[[accounts]]` table. Returns
/// the accounts created (empty when already migrated, or there are no positions).
fn migrate_config(config_path: &Path, env: &str) -> Result<Vec<String>> {
    let mut config = AppConfig::load(config_path)?;
    if config.positions.iter().all(|p| !p.account.is_empty()) {
        return Ok(Vec::new()); // already migrated, or nothing to migrate
    }
    // Assign each un-accounted position its account, remembering the env each new account needs.
    let mut needed: BTreeMap<String, String> = BTreeMap::new();
    for p in &mut config.positions {
        if p.account.is_empty() {
            let name = account_for(&p.broker, env);
            // env is meaningful only for KIS accounts; others get the historical default.
            let acct_env = if name.starts_with("kis-") {
                env
            } else {
                "paper"
            };
            needed
                .entry(name.clone())
                .or_insert_with(|| acct_env.to_string());
            p.account = name;
        }
    }
    // Register any account not already present.
    let mut created = Vec::new();
    for (name, acct_env) in needed {
        if config.find_account(&name).is_none() {
            config.upsert_account(AccountConfig {
                name: name.clone(),
                env: acct_env,
            });
            created.push(name);
        }
    }
    config.save(config_path)?;
    Ok(created)
}

/// Move the flat `kis_*` secrets under the `kis-{env}` account prefix and drop the old keys
/// (including `kis_env`). Toss secrets (`toss_*`) already match the `{account}_{field}` shape for
/// the `toss` account, so they need no rename. Idempotent: a no-op once `kis_app_key` is gone.
fn migrate_secrets(secrets: &dyn SecretStore, env: &str) -> Result<()> {
    if secrets.get("kis_app_key")?.is_none() {
        return Ok(()); // already migrated, or KIS was never configured
    }
    let account = format!("kis-{env}");
    for field in ["app_key", "app_secret", "cano", "product_code", "exchange"] {
        let old = format!("kis_{field}"); // the legacy flat key, read for the last time
        if let Some(value) = secrets.get(&old)? {
            secrets.set(&AccountId::secret_key(&account, field), &value)?;
            secrets.delete(&old)?;
        }
    }
    secrets.delete("kis_env")?;
    Ok(())
}

fn storage_err(e: impl std::fmt::Display) -> DomainError {
    DomainError::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStateRepository;
    use drip_domain::{BrokerId, Money, StateRepository, Ticker};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // An in-memory secret store for migration tests.
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

    /// Seed the full flat KIS secret set at the given environment.
    fn seed_legacy_kis(secrets: &MapSecrets, env: &str) {
        for (k, v) in [
            ("kis_env", env),
            ("kis_app_key", "legacy-key"),
            ("kis_app_secret", "legacy-secret"),
            ("kis_cano", "12345678"),
            ("kis_product_code", "01"),
            ("kis_exchange", "nasdaq"),
        ] {
            secrets.set(k, v).unwrap();
        }
    }

    /// Build a legacy `(broker, ticker)` positions table holding one kodex-like row, with the
    /// position JSON stripped of its `account` key to mimic a genuinely pre-account row.
    fn write_legacy_state(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "CREATE TABLE positions (
                broker TEXT NOT NULL,
                ticker TEXT NOT NULL,
                data   TEXT NOT NULL,
                PRIMARY KEY (broker, ticker)
            )",
            [],
        )
        .unwrap();
        let mut kodex = Position::new(
            AccountId::new(""), // legacy rows had no account
            BrokerId::Kis,
            Ticker::new("122630"),
            Money::new(dec!(24000000)),
            40,
        );
        kodex.shares = drip_domain::Shares::new(4);
        kodex.avg_price = drip_domain::Price::new(dec!(194101.25));
        kodex.cum_spent = Money::new(dec!(776405));
        kodex.reconciled_through = Some(time::macros::date!(2026 - 06 - 24));
        let mut value = serde_json::to_value(&kodex).unwrap();
        value.as_object_mut().unwrap().remove("account"); // a true pre-account row has no key
        let data = serde_json::to_string(&value).unwrap();
        conn.execute(
            "INSERT INTO positions (broker, ticker, data) VALUES ('kis', '122630', ?1)",
            [data],
        )
        .unwrap();
        // A legacy order_journal with a broker-prefixed client key, as the pre-account binary wrote.
        conn.execute(
            "CREATE TABLE order_journal (client_key TEXT PRIMARY KEY, order_id TEXT, at INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO order_journal (client_key, at) VALUES ('kis:122630:2026-06-24:loc_low', 0)",
            [],
        )
        .unwrap();
    }

    fn journal_keys(state_path: &Path) -> Vec<String> {
        let conn = Connection::open(state_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT client_key FROM order_journal")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[tokio::test]
    async fn state_migration_preserves_the_ledger_by_value() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.db");
        write_legacy_state(&state_path);
        let secrets = MapSecrets::default();
        seed_legacy_kis(&secrets, "paper");

        let report = migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        assert!(report.state_backup.is_some());
        assert!(report.state_backup.as_ref().unwrap().exists());

        // The kodex row now lives under `kis-paper` with every other field intact, by value.
        let repo = SqliteStateRepository::open(state_path.clone()).unwrap();
        let kodex = repo
            .load(&AccountId::new("kis-paper"), &Ticker::new("122630"))
            .await
            .unwrap()
            .expect("kodex migrated under kis-paper");
        assert_eq!(kodex.account, AccountId::new("kis-paper"));
        assert_eq!(kodex.broker, BrokerId::Kis);
        assert_eq!(kodex.shares, drip_domain::Shares::new(4));
        assert_eq!(kodex.avg_price, drip_domain::Price::new(dec!(194101.25)));
        assert_eq!(kodex.cum_spent, Money::new(dec!(776405)));
        assert_eq!(kodex.seed, Money::new(dec!(24000000)));
        assert_eq!(
            kodex.reconciled_through,
            Some(time::macros::date!(2026 - 06 - 24))
        );

        // Idempotent: a second run is a no-op (schema already account-keyed → no new backup).
        let again = migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        assert!(again.state_backup.is_none());
        let still = repo
            .load(&AccountId::new("kis-paper"), &Ticker::new("122630"))
            .await
            .unwrap();
        assert_eq!(still, Some(kodex));
    }

    #[test]
    fn migration_rekeys_the_order_journal() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.db");
        write_legacy_state(&state_path);
        let secrets = MapSecrets::default();
        seed_legacy_kis(&secrets, "paper");

        migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        // The broker-prefixed reservation is re-keyed to the account, so a same-day re-tick across
        // the upgrade finds it and won't double-place (the over-buy guard, preserved through the
        // schema change).
        assert_eq!(
            journal_keys(&state_path),
            vec!["kis-paper:122630:2026-06-24:loc_low".to_string()]
        );

        // Idempotent: a second migration leaves the already-account-keyed entry untouched.
        migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        assert_eq!(
            journal_keys(&state_path),
            vec!["kis-paper:122630:2026-06-24:loc_low".to_string()]
        );
    }

    #[test]
    fn config_and_secrets_migrate_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.db"); // never created — fresh state
        // A pre-account config: a domestic position with no account, no [[accounts]].
        std::fs::write(
            &config_path,
            "[[positions]]\nname = \"kodex\"\nbroker = \"kis-domestic\"\n\
             ticker = \"122630\"\nseed = \"24000000\"\nsplits = 40\n",
        )
        .unwrap();
        let secrets = MapSecrets::default();
        seed_legacy_kis(&secrets, "real");

        let report = migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        assert_eq!(report.accounts, vec!["kis-real".to_string()]);

        // Config: the position is now on `kis-real`, and the account is registered with env=real.
        let config = AppConfig::load(&config_path).unwrap();
        assert_eq!(config.find("kodex").unwrap().account, "kis-real");
        assert_eq!(config.find_account("kis-real").unwrap().env, "real");

        // Secrets: creds moved under the account prefix; the flat keys (and kis_env) are gone.
        assert_eq!(
            secrets.get("kis-real_app_key").unwrap().as_deref(),
            Some("legacy-key")
        );
        assert_eq!(
            secrets.get("kis-real_exchange").unwrap().as_deref(),
            Some("nasdaq")
        );
        assert!(secrets.get("kis_app_key").unwrap().is_none());
        assert!(secrets.get("kis_env").unwrap().is_none());

        // Idempotent: re-running changes nothing more.
        let again = migrate_to_accounts(&config_path, &secrets, &state_path).unwrap();
        assert!(!again.did_anything());
        let config2 = AppConfig::load(&config_path).unwrap();
        assert_eq!(config2.accounts.len(), 1);
        assert_eq!(config2.find("kodex").unwrap().account, "kis-real");
    }
}
