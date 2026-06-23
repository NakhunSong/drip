//! Shared HTTP helpers for the live broker adapters: a TTL token cache and small parsers
//! that turn broker JSON strings into domain value objects (both KIS and Toss send numeric
//! fields as strings).

use drip_domain::{DomainError, Price, Result, Shares};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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

/// Current Unix time in milliseconds. Sub-second precision is needed for the rate limiter's
/// cross-process spacing (the 실전 interval is 60ms).
fn now_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
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

    /// Persist the token with an absolute expiry, best-effort. Errors are logged and swallowed —
    /// a failed write just means the next process re-fetches.
    fn save(&self, token: &str, ttl: Duration) {
        let expires_at_unix = now_unix() + ttl.as_secs() as i64;
        let persisted = PersistedToken {
            token: token.to_string(),
            expires_at_unix,
        };
        let json = match serde_json::to_string(&persisted) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("could not serialize KIS token: {e}");
                return;
            }
        };
        if let Err(e) = write_atomic_0600(&self.path, json.as_bytes()) {
            tracing::warn!(
                "could not persist KIS token to {}: {e}",
                self.path.display()
            );
        }
    }
}

/// Write `bytes` to `path` atomically with mode `0600`: a unique-per-write temp file (pid + a
/// process-global counter, so concurrent writers never collide) is created `0600`, written,
/// fsync'd, and renamed over `path`. Shared by [`TokenStore`] and [`RateLimitStore`].
fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
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
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// On-disk record of the last KIS request time (Unix millis).
#[derive(Serialize, Deserialize)]
struct PersistedRateLimit {
    last_request_ms: i64,
}

/// L2 for the rate limiter: a single `0600` JSON file holding the last KIS request time, so the
/// per-second spacing holds across separate `drip` processes (each command is its own process).
/// Best-effort — a missing/corrupt file or a failed write silently degrades to per-process spacing.
#[derive(Debug)]
pub(crate) struct RateLimitStore {
    path: PathBuf,
}

impl RateLimitStore {
    pub(crate) fn new(path: PathBuf) -> RateLimitStore {
        RateLimitStore { path }
    }

    /// The last recorded request time (Unix millis), or `None` if the file is absent/unreadable.
    fn load(&self) -> Option<i64> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        let persisted: PersistedRateLimit = serde_json::from_str(&raw).ok()?;
        Some(persisted.last_request_ms)
    }

    /// Record the last-request time. Best-effort and **silent** on failure — this writes on every
    /// KIS request, so logging per call would spam; degrading to per-process spacing is acceptable.
    fn save(&self, last_request_ms: i64) {
        if let Ok(json) = serde_json::to_string(&PersistedRateLimit { last_request_ms }) {
            let _ = write_atomic_0600(&self.path, json.as_bytes());
        }
    }
}

/// An async rate limiter that spaces successive [`acquire`](RateLimiter::acquire) calls by at
/// least `min_interval`. KIS throttles per second — 모의 strictly (≈1/s; it returns `EGW00201`
/// "초당 거래건수 초과" otherwise), 실전 ≈20/s — so every KIS request acquires this first.
/// Spacing is **exact within a process** (the held lock serializes callers) and **best-effort
/// across processes** via a shared on-disk timestamp ([`RateLimitStore`]): each `drip` command is
/// its own process, so without it two commands launched within a second would not coordinate (#17).
#[derive(Debug)]
pub(crate) struct RateLimiter {
    min_interval: Duration,
    last_ms: Mutex<Option<i64>>,
    store: Option<RateLimitStore>,
}

impl RateLimiter {
    pub(crate) fn new(min_interval: Duration, store: Option<RateLimitStore>) -> RateLimiter {
        RateLimiter {
            min_interval,
            last_ms: Mutex::new(None),
            store,
        }
    }

    /// Wait until at least `min_interval` has elapsed since the previous acquire — in this process
    /// or, via the store, any other — then record the new time. A zero interval is a no-op (tests).
    pub(crate) async fn acquire(&self) {
        let mut last = self.last_ms.lock().await;
        let disk = self.store.as_ref().and_then(|store| store.load());
        let prev = [*last, disk].into_iter().flatten().max();
        if let Some(prev) = prev {
            let interval_ms = self.min_interval.as_millis() as i64;
            let elapsed = now_millis() - prev;
            if elapsed < 0 {
                // Wall clock moved backward (e.g. NTP); be conservative and wait a full interval.
                tokio::time::sleep(self.min_interval).await;
            } else if elapsed < interval_ms {
                tokio::time::sleep(Duration::from_millis((interval_ms - elapsed) as u64)).await;
            }
        }
        let fired = now_millis();
        *last = Some(fired);
        if let Some(store) = &self.store {
            store.save(fired);
        }
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
        // 50ms interval with a 40ms assertion: margin for millisecond quantization, since the
        // limiter now measures wall-clock millis rather than a sub-ms `Instant`.
        let limiter = RateLimiter::new(Duration::from_millis(50), None);
        let start = Instant::now();
        limiter.acquire().await; // first is immediate (no prior)
        limiter.acquire().await; // second waits out the interval
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "two acquires should be spaced by at least the interval"
        );
    }

    #[tokio::test]
    async fn rate_limiter_spaces_across_processes_via_the_store() {
        // Deterministic: assert the *recorded* timestamps are spaced by the interval, rather than
        // measuring a wall-clock sleep (which flakes if the scheduler delays the second acquire
        // past the interval). The second limiter is a fresh instance whose only shared state is
        // the file, so its spacing can only come from the first's on-disk timestamp.
        let path = temp_path("ratelimit");
        let _ = std::fs::remove_file(&path);
        let interval = Duration::from_millis(50);
        RateLimiter::new(interval, Some(RateLimitStore::new(path.clone())))
            .acquire()
            .await;
        let first = RateLimitStore::new(path.clone())
            .load()
            .expect("first acquire records a timestamp");
        RateLimiter::new(interval, Some(RateLimitStore::new(path.clone())))
            .acquire()
            .await;
        let second = RateLimitStore::new(path.clone())
            .load()
            .expect("second acquire records a timestamp");
        assert!(
            second - first >= interval.as_millis() as i64,
            "the second acquire must record at least one interval past the first (got {}ms)",
            second - first
        );
        std::fs::remove_file(&path).unwrap();
    }

    // A zero interval (used by the test brokers so the suite never sleeps) is exercised
    // implicitly by every multi-call wiremock test; a wall-clock upper-bound assertion here
    // would only add CI flakiness, so it is intentionally not tested by timing.
}
