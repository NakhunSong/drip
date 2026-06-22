//! Shared HTTP helpers for the live broker adapters: a TTL token cache and small parsers
//! that turn broker JSON strings into domain value objects (both KIS and Toss send numeric
//! fields as strings).

use drip_domain::{DomainError, Price, Result, Shares};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Map any displayable error (reqwest, JSON, parse) into a domain broker error.
pub(crate) fn broker_err(error: impl std::fmt::Display) -> DomainError {
    DomainError::Broker(error.to_string())
}

/// Today's date in UTC, used to stamp quote `as_of`. Centralized so the KIS and Toss
/// adapters don't each inline the system-clock read.
pub(crate) fn today_utc() -> time::Date {
    time::OffsetDateTime::now_utc().date()
}

pub(crate) fn parse_decimal(raw: &str) -> Result<Decimal> {
    Decimal::from_str(raw.trim())
        .map_err(|e| DomainError::Broker(format!("invalid number '{raw}': {e}")))
}

/// Format a date as KIS's `YYYYMMDD` (no separators), used for order/execution date ranges.
pub(crate) fn yyyymmdd(date: time::Date) -> String {
    format!(
        "{:04}{:02}{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Parse KIS's `YYYYMMDD` date string into a [`time::Date`].
pub(crate) fn parse_yyyymmdd(raw: &str) -> Result<time::Date> {
    let s = raw.trim();
    let bad = || DomainError::Broker(format!("invalid KIS date '{raw}'"));
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let year: i32 = s[0..4].parse().map_err(|_| bad())?;
    let month =
        time::Month::try_from(s[4..6].parse::<u8>().map_err(|_| bad())?).map_err(|_| bad())?;
    let day: u8 = s[6..8].parse().map_err(|_| bad())?;
    time::Date::from_calendar_date(year, month, day).map_err(|_| bad())
}

/// Parse a positive price; empty or non-positive input becomes `None` (e.g. a flat lot).
pub(crate) fn parse_price_opt(raw: &str) -> Result<Option<Price>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Price::new(parse_decimal(trimmed)?))
}

pub(crate) fn parse_shares(raw: &str) -> Result<Shares> {
    let whole = parse_decimal(raw)?.trunc();
    let count = whole
        .to_u32()
        .ok_or_else(|| DomainError::Broker(format!("invalid quantity '{raw}'")))?;
    Ok(Shares::new(count))
}

/// A cached OAuth bearer token with a monotonic expiry.
#[derive(Debug, Default)]
pub(crate) struct TokenCache {
    inner: Mutex<Option<Cached>>,
}

#[derive(Debug, Clone)]
struct Cached {
    token: String,
    expires_at: Instant,
}

impl TokenCache {
    pub(crate) fn new() -> TokenCache {
        TokenCache {
            inner: Mutex::new(None),
        }
    }

    /// Return the cached token if still valid, otherwise run `refresh` to obtain a fresh
    /// `(token, ttl)` and cache it. The lock is held across the refresh so concurrent callers
    /// do not stampede the token endpoint.
    pub(crate) async fn get_or_refresh<Fut>(&self, refresh: impl FnOnce() -> Fut) -> Result<String>
    where
        Fut: std::future::Future<Output = Result<(String, Duration)>>,
    {
        let mut guard = self.inner.lock().await;
        if let Some(cached) = guard.as_ref()
            && Instant::now() < cached.expires_at
        {
            return Ok(cached.token.clone());
        }
        let (token, ttl) = refresh().await?;
        *guard = Some(Cached {
            token: token.clone(),
            expires_at: Instant::now() + ttl,
        });
        Ok(token)
    }
}
