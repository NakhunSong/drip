//! 한국투자증권 (KIS) adapter — **domestic** Korean-stock quotes and balance.
//!
//! Read-only: quotes ([`Quotes`]) and account state ([`AccountQuery`]) only. It does **not**
//! implement [`drip_domain::OrderGateway`], so there is no type-level path to place a domestic
//! order yet — live domestic placement and execution history are a later phase. Endpoints follow
//! the official `koreainvestment/open-trading-api` reference (`domestic_stock/inquire_price` and
//! `inquire_balance`).
//!
//! The account, app key/secret, OAuth token, and per-second rate limiter are **shared with the
//! overseas [`KisBroker`]**: same app key → same on-disk token and rate-limit files (the cache
//! filename helper and `base_url` are reused from [`crate::kis`]), so the domestic and overseas
//! adapters coordinate token issuance and request spacing across processes exactly as a single
//! KIS app key requires (KIS issues ~1 token/min and throttles per second per app key).

use crate::http::{
    RateLimitStore, RateLimiter, TokenCache, TokenStore, broker_err, parse_decimal,
    parse_price_opt, parse_shares, send_with_retry, today_utc,
};
use crate::kis::{KisConfig, KisEnv, kis_cache_filename};
use async_trait::async_trait;
use drip_domain::{
    AccountQuery, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money, Quote,
    Quotes, Result, Ticker,
};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// A KIS adapter for domestic (KRX) Korean stocks. Reuses [`KisConfig`] — the domestic and
/// overseas adapters share one account and one app key; `KisConfig.exchange` is a US-exchange
/// field and is irrelevant here, so it is unused.
#[derive(Debug)]
pub struct KisDomesticBroker {
    config: KisConfig,
    base: String,
    client: reqwest::Client,
    tokens: TokenCache,
    limiter: Arc<RateLimiter>,
    /// Base backoff between transient-5xx retries (exponential); zero in tests.
    retry_backoff: Duration,
}

impl KisDomesticBroker {
    /// Build a domestic KIS adapter. `cache_dir` (the drip home) enables the on-disk caches for
    /// the OAuth token and the cross-process rate-limit timestamp; `None` keeps both in-memory
    /// only (tests). The cache filenames and base URL are the same as the overseas adapter for the
    /// same app key, so a single token and a single rate-limit clock are shared between them.
    pub fn new(config: KisConfig, cache_dir: Option<&Path>) -> Result<KisDomesticBroker> {
        let base = config.environment.base_url().to_string();
        let client = reqwest::Client::builder().build().map_err(broker_err)?;
        // KIS throttles per second; 모의 strictly (≈1/s), 실전 ≈20/s. Space every request under the
        // limit so multi-call commands never trip EGW00201.
        let interval = match config.environment {
            KisEnv::Paper => Duration::from_millis(1100),
            KisEnv::Real => Duration::from_millis(60),
        };
        // Disk-backed caches under the drip home, keyed identically to the overseas adapter so the
        // token and the rate-limit timestamp are shared across both adapters and across processes.
        // Memory-only when no dir is provided.
        let (tokens, limiter_store) = match cache_dir {
            Some(dir) => (
                TokenCache::with_store(TokenStore::new(
                    dir.join(kis_cache_filename(&config, "token")),
                )),
                Some(RateLimitStore::new(
                    dir.join(kis_cache_filename(&config, "ratelimit")),
                )),
            ),
            None => (TokenCache::new(), None),
        };
        Ok(KisDomesticBroker {
            config,
            base,
            client,
            tokens,
            limiter: Arc::new(RateLimiter::new(interval, limiter_store)),
            retry_backoff: Duration::from_millis(250),
        })
    }

    async fn token(&self) -> Result<String> {
        let base = self.base.clone();
        let app_key = self.config.app_key.clone();
        let app_secret = self.config.app_secret.clone();
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

    /// The balance-inquiry `tr_id` differs by environment (`T*` real, `V*` paper).
    fn balance_tr_id(&self) -> &'static str {
        match self.config.environment {
            KisEnv::Real => "TTTC8434R",
            KisEnv::Paper => "VTTC8434R",
        }
    }

    async fn fetch_balance(&self) -> Result<KisBalanceResp> {
        let token = self.token().await?;
        let body: KisBalanceResp = send_with_retry(&self.limiter, self.retry_backoff, || {
            self.client
                .get(format!(
                    "{}/uapi/domestic-stock/v1/trading/inquire-balance",
                    self.base
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("appkey", &self.config.app_key)
                .header("appsecret", &self.config.app_secret)
                .header("tr_id", self.balance_tr_id())
                .header("custtype", "P")
                .query(&[
                    ("CANO", self.config.cano.as_str()),
                    ("ACNT_PRDT_CD", self.config.product_code.as_str()),
                    ("AFHR_FLPR_YN", "N"),
                    ("OFL_YN", ""),
                    ("INQR_DVSN", "02"),
                    ("UNPR_DVSN", "01"),
                    ("FUND_STTL_ICLD_YN", "N"),
                    ("FNCG_AMT_AUTO_RDPT_YN", "N"),
                    ("PRCS_DVSN", "00"),
                    ("CTX_AREA_FK100", ""),
                    ("CTX_AREA_NK100", ""),
                ])
        })
        .await?
        .json()
        .await
        .map_err(broker_err)?;
        if body.rt_cd != "0" {
            return Err(DomainError::Broker(format!(
                "KIS domestic balance rt_cd={} {}",
                body.rt_cd, body.msg1
            )));
        }
        Ok(body)
    }
}

impl BrokerInfo for KisDomesticBroker {
    fn id(&self) -> BrokerId {
        BrokerId::Kis
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            realtime_quotes: false,
            paper_account: matches!(self.config.environment, KisEnv::Paper),
            order_placement: false,
            overseas: false,
        }
    }
}

#[async_trait]
impl Quotes for KisDomesticBroker {
    async fn quote(&self, ticker: &Ticker) -> Result<Quote> {
        let token = self.token().await?;
        let body: KisQuoteResp = send_with_retry(&self.limiter, self.retry_backoff, || {
            self.client
                .get(format!(
                    "{}/uapi/domestic-stock/v1/quotations/inquire-price",
                    self.base
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("appkey", &self.config.app_key)
                .header("appsecret", &self.config.app_secret)
                .header("tr_id", "FHKST01010100")
                .header("custtype", "P")
                .query(&[
                    ("FID_COND_MRKT_DIV_CODE", "J"),
                    ("FID_INPUT_ISCD", ticker.as_str()),
                ])
        })
        .await?
        .json()
        .await
        .map_err(broker_err)?;
        if body.rt_cd != "0" {
            return Err(DomainError::Broker(format!(
                "KIS domestic quote rt_cd={} {}",
                body.rt_cd, body.msg1
            )));
        }
        let price = parse_price_opt(&body.output.stck_prpr)?
            .ok_or_else(|| DomainError::Broker(format!("KIS returned no price for {ticker}")))?;
        Ok(Quote {
            ticker: ticker.clone(),
            price,
            as_of: today_utc(),
        })
    }
}

#[async_trait]
impl AccountQuery for KisDomesticBroker {
    async fn holdings(&self) -> Result<Vec<Holding>> {
        let body = self.fetch_balance().await?;
        let mut holdings = Vec::new();
        for lot in &body.output1 {
            let shares = parse_shares(&lot.hldg_qty)?;
            if shares.is_zero() {
                continue;
            }
            holdings.push(Holding {
                ticker: Ticker::new(&lot.pdno),
                shares,
                avg_price: parse_price_opt(&lot.pchs_avg_pric)?,
            });
        }
        Ok(holdings)
    }

    async fn balance(&self) -> Result<Money> {
        // Domestic inquire-balance carries a clean deposit figure (`dnca_tot_amt`, 예수금총금액,
        // KRW) in the account-summary block. `output2` is an array; an account with no summary row
        // (or the field absent) means no cash to report, so an empty value reads as 0, not a
        // failure (#14).
        let body = self.fetch_balance().await?;
        let raw = body
            .output2
            .first()
            .map(|summary| summary.dnca_tot_amt.trim())
            .unwrap_or("");
        let amount = if raw.is_empty() { "0" } else { raw };
        Ok(Money::new(parse_decimal(amount)?))
    }

    async fn fills_since(&self, _ticker: &Ticker, _since: time::Date) -> Result<Vec<Fill>> {
        // Phase 2: domestic execution history via inquire-daily-ccld
        Ok(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
struct KisToken {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct KisQuoteResp {
    #[serde(default)]
    rt_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output: KisQuoteOutput,
}

#[derive(Debug, Default, Deserialize)]
struct KisQuoteOutput {
    #[serde(default)]
    stck_prpr: String,
}

#[derive(Debug, Deserialize)]
struct KisBalanceResp {
    #[serde(default)]
    rt_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output1: Vec<KisHolding>,
    /// Account summary. The domestic balance endpoint returns `output2` as an **array** (verified
    /// against `koreainvestment/open-trading-api`); the deposit lives in its first element.
    #[serde(default)]
    output2: Vec<KisSummary>,
}

#[derive(Debug, Default, Deserialize)]
struct KisSummary {
    #[serde(default)]
    dnca_tot_amt: String,
}

#[derive(Debug, Deserialize)]
struct KisHolding {
    #[serde(default)]
    pdno: String,
    #[serde(default)]
    hldg_qty: String,
    #[serde(default)]
    pchs_avg_pric: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kis::KisExchange;
    use drip_domain::{Price, Shares};
    use rust_decimal_macros::dec;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config() -> KisConfig {
        KisConfig {
            environment: KisEnv::Paper,
            app_key: "key".into(),
            app_secret: "secret".into(),
            cano: "12345678".into(),
            product_code: "01".into(),
            exchange: KisExchange::Nasdaq,
        }
    }

    fn broker(base: String) -> KisDomesticBroker {
        KisDomesticBroker {
            config: config(),
            base,
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
            limiter: Arc::new(RateLimiter::new(Duration::ZERO, None)),
            retry_backoff: Duration::ZERO,
        }
    }

    async fn mock_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/oauth2/tokenP"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token": "tok", "token_type": "Bearer", "expires_in": 86400}),
            ))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn quote_parses_current_price() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/domestic-stock/v1/quotations/inquire-price"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"rt_cd": "0", "msg1": "ok", "output": {"stck_prpr": "12345"}}),
            ))
            .mount(&server)
            .await;

        let quote = broker(server.uri())
            .quote(&Ticker::new("122630"))
            .await
            .unwrap();
        assert_eq!(quote.price, Price::new(dec!(12345)).unwrap());
        assert_eq!(quote.ticker, Ticker::new("122630"));
    }

    #[tokio::test]
    async fn balance_parses_the_deposit_field() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/domestic-stock/v1/trading/inquire-balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0",
                "msg1": "ok",
                "output1": [],
                "output2": [{"dnca_tot_amt": "1500000"}]
            })))
            .mount(&server)
            .await;

        let balance = broker(server.uri()).balance().await.unwrap();
        assert_eq!(balance, Money::new(dec!(1500000)));
    }

    #[tokio::test]
    async fn holdings_skip_zero_quantity_lots() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/domestic-stock/v1/trading/inquire-balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0",
                "msg1": "ok",
                "output1": [
                    {"pdno": "122630", "hldg_qty": "10", "pchs_avg_pric": "20000"},
                    {"pdno": "252670", "hldg_qty": "0", "pchs_avg_pric": "0"}
                ],
                "output2": [{"dnca_tot_amt": "500000"}]
            })))
            .mount(&server)
            .await;

        let broker = broker(server.uri());
        let holdings = broker.holdings().await.unwrap();
        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].ticker, Ticker::new("122630"));
        assert_eq!(holdings[0].shares, Shares::new(10));
        assert_eq!(holdings[0].avg_price, Price::new(dec!(20000)));
    }

    #[tokio::test]
    async fn balance_is_zero_when_the_deposit_field_is_absent() {
        // A 모의 (or summary-less) account returns rt_cd "0" but an empty `output2` array — there
        // is no cash row to read. `balance()` must read that as 0, not fail or panic (#14).
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/domestic-stock/v1/trading/inquire-balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0",
                "msg_cd": "70070000",
                "msg1": "모의투자 조회할 내역(자료)이 없습니다.",
                "output1": [],
                "output2": []
            })))
            .mount(&server)
            .await;

        let broker = broker(server.uri());
        assert!(broker.holdings().await.unwrap().is_empty());
        assert_eq!(broker.balance().await.unwrap(), Money::new(dec!(0)));
    }
}
