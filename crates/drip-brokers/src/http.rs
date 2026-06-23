//! Shared HTTP helpers for the live broker adapters: a TTL token cache and small parsers
//! that turn broker JSON strings into domain value objects (both KIS and Toss send numeric
//! fields as strings).

use drip_domain::{DomainError, Price, Result, Shares};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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

/// Current Unix time in seconds. Centralized alongside [`today_utc`] so the token store's
/// absolute-expiry math reads the clock in one place.
fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
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

/// A two-tier OAuth token cache: L1 in-memory (monotonic expiry) plus an optional L2 on-disk
/// [`TokenStore`] so the token survives across `drip` processes. Without a store it is
/// memory-only (Toss, the web dashboard, tests); KIS attaches a store so back-to-back CLI
/// commands — and every position in `drip run` — reuse one token instead of each re-issuing and
/// tripping KIS's ~1/min token limit.
#[derive(Debug, Default)]
pub(crate) struct TokenCache {
    inner: Mutex<Option<Cached>>,
    store: Option<TokenStore>,
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
            store: None,
        }
    }

    /// A token cache backed by an on-disk [`TokenStore`] (the L2 tier).
    pub(crate) fn with_store(store: TokenStore) -> TokenCache {
        TokenCache {
            inner: Mutex::new(None),
            store: Some(store),
        }
    }

    /// Return a valid token from the cheapest tier: L1 memory, then the L2 disk store, otherwise
    /// run `refresh` for a fresh `(token, ttl)` and write both tiers. The lock is held across the
    /// refresh so concurrent callers in one process do not stampede the token endpoint.
    pub(crate) async fn get_or_refresh<Fut>(&self, refresh: impl FnOnce() -> Fut) -> Result<String>
    where
        Fut: std::future::Future<Output = Result<(String, Duration)>>,
    {
        let mut guard = self.inner.lock().await;
        // L1: in-memory.
        if let Some(cached) = guard.as_ref()
            && Instant::now() < cached.expires_at
        {
            return Ok(cached.token.clone());
        }
        // L2: on disk (shared across processes). Populate L1 from it on a hit.
        if let Some(store) = &self.store
            && let Some((token, ttl)) = store.load()
        {
            *guard = Some(Cached {
                token: token.clone(),
                expires_at: Instant::now() + ttl,
            });
            return Ok(token);
        }
        // L3: live issuance — cache in memory and persist to disk.
        let (token, ttl) = refresh().await?;
        *guard = Some(Cached {
            token: token.clone(),
            expires_at: Instant::now() + ttl,
        });
        if let Some(store) = &self.store {
            store.save(&token, ttl);
        }
        Ok(token)
    }
}

/// On-disk form of a cached token: the bearer string plus an **absolute** expiry (Unix seconds).
/// The in-memory [`Cached`] uses a monotonic `Instant`, which is meaningless across processes, so
/// persistence needs wall-clock time.
#[derive(Serialize, Deserialize)]
struct PersistedToken {
    token: String,
    expires_at_unix: i64,
}

/// L2 token cache: a single `0600` JSON file so a KIS token survives across `drip` processes.
/// KIS issues ~1 token/min per app key but a token is valid ~24h, so persisting it means at most
/// one issuance per day no matter how many commands (or daemon positions) run. The filename is
/// keyed by environment + a hash of the app key, so 모의/실전 and rotated keys never collide.
#[derive(Debug)]
pub(crate) struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub(crate) fn new(path: PathBuf) -> TokenStore {
        TokenStore { path }
    }

    /// Load the persisted token iff the file exists, parses, and has not expired. Any failure
    /// (missing, unreadable, malformed, expired) returns `None`, so a bad cache file silently
    /// falls through to a live fetch and never breaks a command.
    fn load(&self) -> Option<(String, Duration)> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let persisted: PersistedToken = serde_json::from_str(&raw).ok()?;
        let now = now_unix();
        // Guard BEFORE the u64 cast: a past expiry must be a miss. A negative remaining cast to
        // u64 would wrap to a ~584-billion-year TTL, making an expired token look valid forever.
        if persisted.expires_at_unix <= now {
            return None;
        }
        let remaining = (persisted.expires_at_unix - now) as u64;
        Some((persisted.token, Duration::from_secs(remaining)))
    }

    /// Persist the token with an absolute expiry, best-effort. Writes a per-process `0600` temp
    /// file and atomically renames it over the target, so a concurrent reader never sees a torn
    /// file. Errors are logged and swallowed — a failed write just means the next process
    /// re-fetches.
    fn save(&self, token: &str, ttl: Duration) {
        let expires_at_unix = now_unix() + ttl.as_secs() as i64;
        let persisted = PersistedToken {
            token: token.to_string(),
            expires_at_unix,
        };
        if let Err(e) = self.write_atomic(&persisted) {
            tracing::warn!(
                "could not persist KIS token to {}: {e}",
                self.path.display()
            );
        }
    }

    fn write_atomic(&self, persisted: &PersistedToken) -> std::io::Result<()> {
        use std::io::Write;
        let json = serde_json::to_string(persisted).map_err(std::io::Error::other)?;
        // Unique-per-write temp name (pid + a process-global counter), so even concurrent writers
        // in one process — e.g. two drip-web requests cold-starting separate KisBrokers against the
        // same token file — never clobber each other's temp before the atomic rename.
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

/// A simple async rate limiter that spaces successive [`acquire`](RateLimiter::acquire) calls by
/// at least `min_interval`. KIS throttles per second — 모의 strictly (≈1/s; it returns
/// `EGW00201` "초당 거래건수 초과" otherwise), 실전 ≈20/s — so every KIS request acquires this
/// first. The lock is held across the wait, which serializes callers and guarantees the spacing
/// even under concurrency.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    min_interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub(crate) fn new(min_interval: Duration) -> RateLimiter {
        RateLimiter {
            min_interval,
            last: Mutex::new(None),
        }
    }

    /// Wait until at least `min_interval` has elapsed since the previous acquire, then record the
    /// new time. A zero interval makes this a no-op (used by tests so they don't sleep).
    pub(crate) async fn acquire(&self) {
        let mut last = self.last.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("drip-tokentest-{}-{name}.json", std::process::id()))
    }

    #[test]
    fn token_store_round_trips_a_valid_token() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let store = TokenStore::new(path.clone());
        store.save("tok-abc", Duration::from_secs(3600));
        let (token, ttl) = store.load().expect("a freshly saved token loads");
        assert_eq!(token, "tok-abc");
        assert!(ttl > Duration::from_secs(3500) && ttl <= Duration::from_secs(3600));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn token_store_treats_an_expired_file_as_a_miss() {
        // The underflow trap: a past expiry must be None, not wrap to a ~584-billion-year TTL.
        let path = temp_path("expired");
        let persisted = PersistedToken {
            token: "stale".into(),
            expires_at_unix: 1_000, // 1970 — long past
        };
        std::fs::write(&path, serde_json::to_string(&persisted).unwrap()).unwrap();
        assert!(
            TokenStore::new(path.clone()).load().is_none(),
            "an expired token must be a miss"
        );
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn token_store_missing_or_corrupt_is_a_miss() {
        assert!(TokenStore::new(temp_path("absent")).load().is_none());
        let path = temp_path("corrupt");
        std::fs::write(&path, "not json").unwrap();
        assert!(TokenStore::new(path.clone()).load().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn token_cache_reuses_the_disk_token_across_instances() {
        let path = temp_path("twotier");
        let _ = std::fs::remove_file(&path);
        // First instance: cold, fetches once, writes disk.
        let cache_a = TokenCache::with_store(TokenStore::new(path.clone()));
        let first = cache_a
            .get_or_refresh(|| async { Ok(("fetched".to_string(), Duration::from_secs(3600))) })
            .await
            .unwrap();
        assert_eq!(first, "fetched");
        // Second instance (fresh memory, like a new process): must read disk, NOT call refresh.
        let cache_b = TokenCache::with_store(TokenStore::new(path.clone()));
        let second = cache_b
            .get_or_refresh(|| async { panic!("must not fetch — disk has a valid token") })
            .await
            .unwrap();
        assert_eq!(second, "fetched");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn rate_limiter_spaces_successive_acquires() {
        let limiter = RateLimiter::new(Duration::from_millis(40));
        let start = Instant::now();
        limiter.acquire().await; // first is immediate (no prior)
        limiter.acquire().await; // second waits out the interval
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "two acquires should be spaced by at least the interval"
        );
    }

    // A zero interval (used by the test brokers so the suite never sleeps) is exercised
    // implicitly by every multi-call wiremock test; a wall-clock upper-bound assertion here
    // would only add CI flakiness, so it is intentionally not tested by timing.
}
