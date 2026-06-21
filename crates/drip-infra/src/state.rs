//! A sqlite-backed [`StateRepository`]. Positions are stored as JSON keyed by
//! (broker, ticker). sqlite is bundled into the binary (no system dependency); connections
//! are opened per call, which is ample for a single-user CLI's low-frequency state writes.

use async_trait::async_trait;
use drip_domain::{
    BrokerId, DomainError, OrderId, OrderJournal, Position, Result, StateRepository, Ticker,
};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;

/// Persists positions in a sqlite database file.
#[derive(Debug, Clone)]
pub struct SqliteStateRepository {
    path: PathBuf,
}

impl SqliteStateRepository {
    pub fn open(path: PathBuf) -> Result<SqliteStateRepository> {
        let repo = SqliteStateRepository { path };
        let conn = repo.conn()?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS positions (
                broker TEXT NOT NULL,
                ticker TEXT NOT NULL,
                data   TEXT NOT NULL,
                PRIMARY KEY (broker, ticker)
            )",
            [],
        )
        .map_err(storage_err)?;
        // Idempotency ledger for placed orders (M2). `order_id` is null between the reserve
        // and the broker's acceptance; `at` is a unix timestamp for later housekeeping.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS order_journal (
                client_key TEXT PRIMARY KEY,
                order_id   TEXT,
                at         INTEGER NOT NULL
            )",
            [],
        )
        .map_err(storage_err)?;
        Ok(repo)
    }

    fn conn(&self) -> Result<Connection> {
        Connection::open(&self.path).map_err(storage_err)
    }
}

#[async_trait]
impl StateRepository for SqliteStateRepository {
    async fn load(&self, broker: BrokerId, ticker: &Ticker) -> Result<Option<Position>> {
        let conn = self.conn()?;
        let json: Option<String> = conn
            .query_row(
                "SELECT data FROM positions WHERE broker = ?1 AND ticker = ?2",
                (broker.to_string(), ticker.as_str()),
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_err)?;
        match json {
            Some(text) => Ok(Some(deserialize(&text)?)),
            None => Ok(None),
        }
    }

    async fn save(&self, position: &Position) -> Result<()> {
        let data = serde_json::to_string(position).map_err(storage_err)?;
        self.conn()?
            .execute(
                "INSERT OR REPLACE INTO positions (broker, ticker, data) VALUES (?1, ?2, ?3)",
                (position.broker.to_string(), position.ticker.as_str(), data),
            )
            .map_err(storage_err)?;
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Position>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT data FROM positions ORDER BY broker, ticker")
            .map_err(storage_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage_err)?;
        let mut positions = Vec::new();
        for row in rows {
            positions.push(deserialize(&row.map_err(storage_err)?)?);
        }
        Ok(positions)
    }
}

#[async_trait]
impl OrderJournal for SqliteStateRepository {
    async fn reserve(&self, key: &str) -> Result<bool> {
        let at = time::OffsetDateTime::now_utc().unix_timestamp();
        let inserted = self
            .conn()?
            .execute(
                "INSERT OR IGNORE INTO order_journal (client_key, at) VALUES (?1, ?2)",
                (key, at),
            )
            .map_err(storage_err)?;
        // 1 row inserted => newly reserved; 0 => the key was already present (skip).
        Ok(inserted == 1)
    }

    async fn record(&self, key: &str, order_id: &OrderId) -> Result<()> {
        self.conn()?
            .execute(
                "UPDATE order_journal SET order_id = ?2 WHERE client_key = ?1",
                (key, order_id.as_str()),
            )
            .map_err(storage_err)?;
        Ok(())
    }
}

fn deserialize(text: &str) -> Result<Position> {
    serde_json::from_str(text).map_err(storage_err)
}

fn storage_err(e: impl std::fmt::Display) -> DomainError {
    DomainError::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::{Money, Ticker};
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteStateRepository::open(dir.path().join("state.db")).unwrap();
        let position = Position::new(
            BrokerId::Kis,
            Ticker::new("TQQQ"),
            Money::new(dec!(4000)),
            40,
        );
        repo.save(&position).await.unwrap();

        let loaded = repo
            .load(BrokerId::Kis, &Ticker::new("TQQQ"))
            .await
            .unwrap();
        assert_eq!(loaded, Some(position));
        assert_eq!(repo.list().await.unwrap().len(), 1);
        assert!(
            repo.load(BrokerId::Toss, &Ticker::new("TQQQ"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn order_journal_reserves_at_most_once_then_records_id() {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteStateRepository::open(dir.path().join("state.db")).unwrap();
        let key = "kis:TQQQ:2026-06-21:loc_low";

        assert!(repo.reserve(key).await.unwrap()); // first reservation wins
        assert!(!repo.reserve(key).await.unwrap()); // same key is refused
        assert!(repo.reserve("kis:TQQQ:2026-06-21:loc_high").await.unwrap()); // distinct key

        // Recording the broker id for a reserved key succeeds.
        repo.record(key, &OrderId::new("0000/0030")).await.unwrap();
    }
}
