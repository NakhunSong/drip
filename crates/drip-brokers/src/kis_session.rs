//! Shared 한국투자증권 (KIS) HTTP session — the auth/transport layer both KIS adapters build on
//! ([`crate::kis::KisBroker`] overseas, [`crate::kis_domestic::KisDomesticBroker`] domestic).
//!
//! It owns the reqwest client, the per-environment [`RateLimiter`], and the OAuth [`TokenCache`],
//! and centralizes the four things that were duplicated verbatim across both adapters: building
//! those caches ([`KisSession::new`]), the OAuth token fetch ([`KisSession::token`]), the authed
//! request preamble ([`KisSession::authed`]), and the `rt_cd` result guard ([`check_rt`]). Each
//! call site keeps its own send strategy: reads that may safely retry go through
//! [`KisSession::send_with_retry_json`], while writes and paginated fill reads use
//! [`KisSession::send_once`] (no retry — a re-sent order double-places, and a re-returned fills
//! page over-counts → over-buys). Because the caches key only off environment + app key, the
//! overseas and domestic sessions for one account share a single token and a single rate-limit
//! clock across processes.

use crate::http::{
    RateLimitStore, RateLimiter, TokenCache, TokenStore, broker_err, send_with_retry,
};
use crate::kis::{KisConfig, KisEnv};
use drip_domain::{DomainError, Result};
use reqwest::{Method, RequestBuilder, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The KIS OAuth token response.
#[derive(Debug, Deserialize)]
struct KisToken {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

/// Filename for a KIS instance's per-`kind` cache file (`kind` = `"token"` | `"ratelimit"`):
/// environment plus a hash of the app key, so 모의/실전 and rotated keys never reuse each other's
/// file. (The hash only discriminates the file; if it changes across toolchains the old file is
/// just orphaned.)
pub(crate) fn kis_cache_filename(config: &KisConfig, kind: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.app_key.hash(&mut hasher);
    let env = match config.environment {
        KisEnv::Paper => "paper",
        KisEnv::Real => "real",
    };
    format!("{kind}-kis-{env}-{:016x}.json", hasher.finish())
}

/// Guard a KIS JSON response's `rt_cd` ("0" = success). `op` names the operation in the error
/// (e.g. `"balance"`, `"domestic inquire-daily-ccld"`); it carries the full overseas/domestic
/// distinction, so keep it exact.
pub(crate) fn check_rt(op: &str, rt_cd: &str, msg1: &str) -> Result<()> {
    if rt_cd != "0" {
        return Err(DomainError::Broker(format!(
            "KIS {op} rt_cd={rt_cd} {msg1}"
        )));
    }
    Ok(())
}

/// Shared KIS HTTP session: reqwest client + rate limiter + OAuth token cache, plus the request
/// preamble and send helpers. Both KIS adapters hold one and delegate transport to it; each also
/// keeps its own `KisConfig` for the account/exchange/`tr_id` knowledge specific to that adapter.
#[derive(Debug)]
pub(crate) struct KisSession {
    base: String,
    client: reqwest::Client,
    tokens: TokenCache,
    limiter: Arc<RateLimiter>,
    app_key: String,
    app_secret: String,
    /// Base backoff between transient-5xx retries (exponential); zero in tests.
    retry_backoff: Duration,
}

impl KisSession {
    /// Build a KIS session from `config`. `cache_dir` (the drip home) enables the on-disk caches
    /// for the OAuth token and the cross-process rate-limit timestamp; `None` keeps both in-memory
    /// only (tests). The cache filenames and base URL key only off environment + app key, so the
    /// overseas and domestic adapters for the same account share a single token and rate-limit
    /// clock across processes.
    pub(crate) fn new(config: &KisConfig, cache_dir: Option<&Path>) -> Result<KisSession> {
        let base = config.environment.base_url().to_string();
        let client = reqwest::Client::builder().build().map_err(broker_err)?;
        // KIS throttles per second; 모의 strictly (≈1/s), 실전 ≈20/s. Space every request under
        // the limit so multi-call commands (tick / account) never trip EGW00201.
        let interval = match config.environment {
            KisEnv::Paper => Duration::from_millis(1100),
            KisEnv::Real => Duration::from_millis(60),
        };
        // Disk-backed caches under the drip home so the token and the rate-limit timestamp survive
        // across processes (KIS issues ~1 token/min, and throttles per second, per app key).
        // Memory-only when no dir is provided.
        let (tokens, limiter_store) = match cache_dir {
            Some(dir) => (
                TokenCache::with_store(TokenStore::new(
                    dir.join(kis_cache_filename(config, "token")),
                )),
                Some(RateLimitStore::new(
                    dir.join(kis_cache_filename(config, "ratelimit")),
                )),
            ),
            None => (TokenCache::new(), None),
        };
        Ok(KisSession {
            base,
            client,
            tokens,
            limiter: Arc::new(RateLimiter::new(interval, limiter_store)),
            app_key: config.app_key.clone(),
            app_secret: config.app_secret.clone(),
            retry_backoff: Duration::from_millis(250),
        })
    }

    /// Fetch (and cache) the OAuth bearer token. Issued at most ~once/day across processes via the
    /// on-disk [`TokenStore`]; callers fetch it **once per flow** and reuse it across pages.
    pub(crate) async fn token(&self) -> Result<String> {
        let base = self.base.clone();
        let app_key = self.app_key.clone();
        let app_secret = self.app_secret.clone();
        let client = self.client.clone();
        let limiter = self.limiter.clone();
        self.tokens
            .get_or_refresh(move || async move {
                limiter.acquire().await;
                let body = json!({
                    "grant_type": "client_credentials",
                    "appkey": app_key,
                    "appsecret": app_secret,
                });
                let token: KisToken = client
                    .post(format!("{base}/oauth2/tokenP"))
                    .json(&body)
                    .send()
                    .await
                    .map_err(broker_err)?
                    .error_for_status()
                    .map_err(broker_err)?
                    .json()
                    .await
                    .map_err(broker_err)?;
                Ok((
                    token.access_token,
                    Duration::from_secs(token.expires_in.saturating_sub(60)),
                ))
            })
            .await
    }

    /// Begin an authenticated KIS request: `{base}{path}` with the standard five-header preamble
    /// (`authorization`/`appkey`/`appsecret`/`tr_id`/`custtype`). The caller passes a `token`
    /// fetched once via [`KisSession::token`] and chains query/body/extra headers (e.g. `tr_cont`).
    ///
    /// `custtype` is `"P"`, and that is correct for every account drip drives — not a stub. The
    /// KIS REST auth reference (`koreainvestment/open-trading-api`, `backtester/kis_auth.py`)
    /// applies `"P"` to **both** 개인 (individual) and 법인 (corporate) own-account customers;
    /// `"B"` is only for a 제휴사 (an API-redistribution affiliate), which a single-user CLI is
    /// not. (#8 proposed making this configurable as "B = corporate"; that premise came from the
    /// WebSocket sample's comment, which the REST reference contradicts — so #8 was closed.)
    pub(crate) fn authed(
        &self,
        method: Method,
        path: &str,
        tr_id: &str,
        token: &str,
    ) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {token}"))
            .header("appkey", &self.app_key)
            .header("appsecret", &self.app_secret)
            .header("tr_id", tr_id)
            .header("custtype", "P")
    }

    /// Send a **retryable** request and deserialize the JSON body. Spaces under the rate limit and
    /// retries transient 5xx (exponential backoff). For single-shot reads only (quote, balance) —
    /// never for writes or paginated fill reads (use [`KisSession::send_once`]). `build` is called
    /// once per attempt because a `RequestBuilder` is single-use.
    pub(crate) async fn send_with_retry_json<T, F>(&self, build: F) -> Result<T>
    where
        T: DeserializeOwned,
        F: Fn() -> RequestBuilder,
    {
        send_with_retry(&self.limiter, self.retry_backoff, build)
            .await?
            .json()
            .await
            .map_err(broker_err)
    }

    /// Send a request **once** (no retry), returning the raw response after spacing under the rate
    /// limit and checking the HTTP status. Used by writes (an order has no client idempotency key,
    /// so a retry could double-place — #20) and by paginated fill reads (a re-returned page would
    /// be folded twice → over-count → over-buy). The caller reads any continuation header before
    /// deserializing.
    pub(crate) async fn send_once(&self, req: RequestBuilder) -> Result<Response> {
        self.limiter.acquire().await;
        req.send()
            .await
            .map_err(broker_err)?
            .error_for_status()
            .map_err(broker_err)
    }

    /// A session pointed at a test server (`base`), with no rate-limit spacing or retry backoff and
    /// an empty in-memory token cache. Mirrors the fields the adapters' wiremock tests built inline.
    #[cfg(test)]
    pub(crate) fn for_test(config: &KisConfig, base: String) -> KisSession {
        KisSession {
            base,
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
            limiter: Arc::new(RateLimiter::new(Duration::ZERO, None)),
            app_key: config.app_key.clone(),
            app_secret: config.app_secret.clone(),
            retry_backoff: Duration::ZERO,
        }
    }
}
