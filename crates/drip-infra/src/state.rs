//! A sqlite-backed [`StateRepository`]. Positions are stored as JSON keyed by
//! (broker, ticker). sqlite is bundled into the binary (no system dependency); connections
//! are opened per call, which is ample for a single-user CLI's low-frequency state writes.

use async_trait::async_trait;
use drip_domain::{BrokerId, DomainError, Position, Result, StateRepository, Ticker};
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
        repo.conn()?
            .execute(
                "CREATE TABLE IF NOT EXISTS positions (
                    broker TEXT NOT NULL,
                    ticker TEXT NOT NULL,
                    data   TEXT NOT NULL,
                    PRIMARY KEY (broker, ticker)
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
}
