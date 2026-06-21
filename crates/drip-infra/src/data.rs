//! A CSV [`MarketDataSource`] for backtests. The file is one ticker's daily series with a
//! header row `date,open,high,low,close` (date as `YYYY-MM-DD`).

use async_trait::async_trait;
use drip_domain::{Bar, DomainError, MarketDataSource, Price, Result, Ticker};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::PathBuf;
use std::str::FromStr;
use time::Date;

/// Reads daily bars from a CSV file.
#[derive(Debug, Clone)]
pub struct CsvMarketData {
    path: PathBuf,
}

impl CsvMarketData {
    pub fn new(path: PathBuf) -> CsvMarketData {
        CsvMarketData { path }
    }
}

#[derive(Debug, Deserialize)]
struct Row {
    date: String,
    open: String,
    high: String,
    low: String,
    close: String,
}

#[async_trait]
impl MarketDataSource for CsvMarketData {
    async fn bars(&self, _ticker: &Ticker, from: Date, to: Date) -> Result<Vec<Bar>> {
        let mut reader = csv::Reader::from_path(&self.path)
            .map_err(|e| DomainError::MarketData(format!("open {}: {e}", self.path.display())))?;
        let mut bars = Vec::new();
        for record in reader.deserialize() {
            let row: Row = record.map_err(|e| DomainError::MarketData(format!("csv row: {e}")))?;
            let date = crate::parse_date(&row.date)
                .map_err(|_| DomainError::MarketData(format!("invalid date '{}'", row.date)))?;
            if date < from || date > to {
                continue;
            }
            bars.push(Bar {
                date,
                open: price(&row.open)?,
                high: price(&row.high)?,
                low: price(&row.low)?,
                close: price(&row.close)?,
            });
        }
        bars.sort_by_key(|bar| bar.date);
        Ok(bars)
    }
}

fn price(raw: &str) -> Result<Price> {
    let value = Decimal::from_str(raw.trim())
        .map_err(|e| DomainError::MarketData(format!("number '{raw}': {e}")))?;
    Price::new(value).ok_or_else(|| DomainError::MarketData(format!("non-positive price '{raw}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use time::macros::date;

    #[tokio::test]
    async fn parses_and_filters_bars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tqqq.csv");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "date,open,high,low,close").unwrap();
        writeln!(file, "2026-01-02,100,101,99,100").unwrap();
        writeln!(file, "2026-01-09,110,116,109,115").unwrap();
        writeln!(file, "2026-02-01,120,121,119,120").unwrap();
        drop(file);

        let source = CsvMarketData::new(path);
        let bars = source
            .bars(
                &Ticker::new("TQQQ"),
                date!(2026 - 01 - 01),
                date!(2026 - 01 - 31),
            )
            .await
            .unwrap();
        assert_eq!(bars.len(), 2); // the February bar is filtered out
        assert_eq!(
            bars[0].close,
            Price::new(rust_decimal_macros::dec!(100)).unwrap()
        );
    }
}
