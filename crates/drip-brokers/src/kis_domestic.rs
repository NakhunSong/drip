//! 한국투자증권 (KIS) adapter — **domestic** Korean-stock quotes, balance, and orders.
//!
//! Quotes ([`Quotes`]), account state ([`AccountQuery`]), and live order placement
//! ([`drip_domain::OrderGateway`]) with execution-history reconcile via `inquire-daily-ccld`
//! (#22 P2). Domestic 모의 orders are placed as 지정가 (limit) at the leg's limit price, rounded to
//! the KRX ETF tick — a 모의 placement path on a KRW-funded account (overseas USD orders can't be
//! placed there). A *real* domestic account stays fenced (a deliberate go-live step — #22).
//! Endpoints follow the official `koreainvestment/open-trading-api` reference
//! (`domestic_stock/{inquire_price, inquire_balance, order_cash, inquire_daily_ccld}`).
//!
//! The account, app key/secret, OAuth token, and per-second rate limiter are **shared with the
//! overseas [`KisBroker`]**: same app key → same on-disk token and rate-limit files (the cache
//! filename helper and `base_url` are reused from [`crate::kis`]), so the domestic and overseas
//! adapters coordinate token issuance and request spacing across processes exactly as a single
//! KIS app key requires (KIS issues ~1 token/min and throttles per second per app key).

use crate::http::{
    RateLimitStore, RateLimiter, TokenCache, TokenStore, broker_err, parse_decimal,
    parse_price_opt, parse_shares, parse_yyyymmdd, send_with_retry, today_utc, yyyymmdd,
};
use crate::kis::{KisConfig, KisEnv, kis_cache_filename};
use async_trait::async_trait;
use drip_domain::calendar::{Market, trading_date};
use drip_domain::{
    AccountQuery, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money,
    OrderGateway, OrderId, OrderIntent, Price, Quote, Quotes, Result, Side, Ticker,
};
use rust_decimal::Decimal;
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

    /// Domestic cash-order `tr_id`, by side and environment (`T*` real, `V*` paper).
    fn order_tr_id(&self, side: Side) -> &'static str {
        match (self.config.environment, side) {
            (KisEnv::Real, Side::Buy) => "TTTC0012U",
            (KisEnv::Real, Side::Sell) => "TTTC0011U",
            (KisEnv::Paper, Side::Buy) => "VTTC0012U",
            (KisEnv::Paper, Side::Sell) => "VTTC0011U",
        }
    }

    /// Daily-execution-inquiry (`inquire-daily-ccld`, 3개월 이내) `tr_id`, by environment.
    fn daily_ccld_tr_id(&self) -> &'static str {
        match self.config.environment {
            KisEnv::Real => "TTTC0081R",
            KisEnv::Paper => "VTTC0081R",
        }
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
            order_placement: true,
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

    async fn fills_since(&self, ticker: &Ticker, since: time::Date) -> Result<Vec<Fill>> {
        let token = self.token().await?;
        // Query range in the KRX (KST) calendar — the same trading date the order key and the
        // reconcile boundary use (#22 P1), and the one `ord_dt` is reported in, so the
        // "completed days < today" comparison stays apples-to-apples. Clamp a corrupt/future
        // watermark so we never send INQR_STRT_DT > INQR_END_DT.
        let today = trading_date(Market::KrEquity, time::OffsetDateTime::now_utc());
        let start = yyyymmdd(since.min(today));
        let end = yyyymmdd(today);
        // 모의 returns the whole account and (like overseas) won't reliably filter by PDNO, so
        // query broadly and filter to `ticker` client-side; a real account scopes server-side.
        // INQR_DVSN "01" (정순 = ascending) returns fills oldest-first — the chronological order
        // `Position::reconcile`/`apply_day` need to resolve same-day cycle boundaries.
        let pdno_param = match self.config.environment {
            KisEnv::Real => ticker.as_str(),
            KisEnv::Paper => "",
        };

        let mut fills = Vec::new();
        let (mut fk, mut nk, mut tr_cont) = (String::new(), String::new(), String::new());
        for _ in 0..MAX_DAILY_CCLD_PAGES {
            // NOT wrapped in `send_with_retry`: this read accumulates fills across pages and the
            // reconcile path folds each without de-duping by execution id, so a re-returned page
            // would over-count → over-buy. A transient 5xx aborts the tick instead (the next fire
            // retries on a fresh, un-advanced ledger). Single-shot reads (quote, balance) do retry.
            self.limiter.acquire().await;
            let resp = self
                .client
                .get(format!(
                    "{}/uapi/domestic-stock/v1/trading/inquire-daily-ccld",
                    self.base
                ))
                .header("authorization", format!("Bearer {token}"))
                .header("appkey", &self.config.app_key)
                .header("appsecret", &self.config.app_secret)
                .header("tr_id", self.daily_ccld_tr_id())
                .header("custtype", "P")
                .header("tr_cont", tr_cont.as_str())
                .query(&[
                    ("CANO", self.config.cano.as_str()),
                    ("ACNT_PRDT_CD", self.config.product_code.as_str()),
                    ("INQR_STRT_DT", start.as_str()),
                    ("INQR_END_DT", end.as_str()),
                    ("SLL_BUY_DVSN_CD", "00"),
                    ("INQR_DVSN", "01"),
                    ("PDNO", pdno_param),
                    ("CCLD_DVSN", "01"),
                    ("ORD_GNO_BRNO", ""),
                    ("ODNO", ""),
                    ("INQR_DVSN_3", "00"),
                    ("INQR_DVSN_1", ""),
                    ("EXCG_ID_DVSN_CD", "KRX"),
                    ("CTX_AREA_FK100", fk.as_str()),
                    ("CTX_AREA_NK100", nk.as_str()),
                ])
                .send()
                .await
                .map_err(broker_err)?
                .error_for_status()
                .map_err(broker_err)?;
            // The continuation flag is a response *header*: `M`/`F` mean more pages follow.
            let more = matches!(
                resp.headers().get("tr_cont").and_then(|v| v.to_str().ok()),
                Some("M") | Some("F")
            );
            let body: KisDailyCcldResp = resp.json().await.map_err(broker_err)?;
            if body.rt_cd != "0" {
                return Err(DomainError::Broker(format!(
                    "KIS domestic inquire-daily-ccld rt_cd={} {}",
                    body.rt_cd, body.msg1
                )));
            }
            for row in &body.output1 {
                // Skip other tickers (a paper query returns the whole account) and any row with
                // nothing filled.
                if row.pdno.trim() != ticker.as_str() {
                    continue;
                }
                let shares = parse_shares(&row.tot_ccld_qty)?;
                if shares.is_zero() {
                    continue;
                }
                // Domestic daily-ccld carries no per-fill execution price, so derive the average
                // from total filled value ÷ filled quantity (gross of fees = the execution price).
                let amount = parse_decimal(&row.tot_ccld_amt)?;
                let price = Price::new(amount / Decimal::from(shares.get())).ok_or_else(|| {
                    DomainError::Broker(format!(
                        "KIS domestic fill for {ticker} had a non-positive execution amount"
                    ))
                })?;
                // sll_buy_dvsn_cd 01 = 매도 (Sell), 02 = 매수 (Buy) — KIS's universal encoding. An
                // unknown code is a hard error, never a silent wrong-side fold (which would corrupt
                // shares / P&L / cycle banking).
                let side = match row.sll_buy_dvsn_cd.trim() {
                    "02" => Side::Buy,
                    "01" => Side::Sell,
                    other => {
                        return Err(DomainError::Broker(format!(
                            "KIS domestic fill had unknown sll_buy_dvsn_cd '{other}'"
                        )));
                    }
                };
                fills.push(Fill {
                    side,
                    shares,
                    price,
                    at: parse_yyyymmdd(&row.ord_dt)?,
                });
            }
            if !more {
                return Ok(fills);
            }
            fk = body.ctx_area_fk100.trim().to_string();
            nk = body.ctx_area_nk100.trim().to_string();
            tr_cont = "N".to_string();
        }
        // Ran past the page cap — surface it rather than silently truncating (under-count →
        // over-buy).
        Err(DomainError::Broker(format!(
            "KIS domestic inquire-daily-ccld exceeded {MAX_DAILY_CCLD_PAGES} pages for {ticker}; \
             narrow the window"
        )))
    }
}

/// Round a KRW limit price to the KRX **ETF** tick — 1원 below 2,000원, 5원 at/above (KRX 2023
/// rule). drip's 무한매수법 targets leveraged ETFs (e.g. KODEX 레버리지), so the ETF tick is correct
/// here; common stocks use a coarser per-band table (deferred to #22), and an off-tick price on a
/// common stock would simply be rejected by KIS (fails loud, never silent).
fn etf_tick_round(price: Decimal) -> Decimal {
    let tick = if price < Decimal::from(2000) {
        Decimal::ONE
    } else {
        Decimal::from(5)
    };
    (price / tick).round() * tick
}

#[async_trait]
impl OrderGateway for KisDomesticBroker {
    async fn place(&self, ticker: &Ticker, order: &OrderIntent) -> Result<OrderId> {
        // 모의 domestic placement is enabled (#22 P3). A REAL domestic account stays fenced: it is
        // a deliberate go-live step, taken only after the cross-day reconcile is confirmed live on
        // 모의 and a real placement is verified — real money on a new adapter is not bundled into
        // enabling 모의. Lifting this fence is a separate production-safety decision.
        if matches!(self.config.environment, KisEnv::Real) {
            return Err(DomainError::Unsupported(
                "domestic real-account placement is not enabled yet — it is a deliberate go-live \
                 after the 모의 cross-day reconcile is confirmed (#22)"
                    .into(),
            ));
        }
        // The strategy's LOC legs are placed as 지정가 (limit, `ORD_DVSN` "00") at the leg's limit
        // price, rounded to the KRX **ETF** tick (5원 ≥2,000원 / 1원 below — the 무한매수법 targets
        // leveraged ETFs; common-stock tick bands are #22). KIS 모의 has no true LOC, so this is a
        // day-limit at that price (the same degrade the overseas adapter makes), not close-only.
        let token = self.token().await?;
        let limit = order.limit.ok_or_else(|| {
            DomainError::Broker("domestic placement requires a limit price".into())
        })?;
        let unit_price = etf_tick_round(limit.value());
        let body = json!({
            "CANO": self.config.cano,
            "ACNT_PRDT_CD": self.config.product_code,
            "PDNO": ticker.as_str(),
            "ORD_DVSN": "00",
            "ORD_QTY": order.shares.get().to_string(),
            "ORD_UNPR": unit_price.normalize().to_string(),
            "EXCG_ID_DVSN_CD": "KRX",
        });
        // Not wrapped in `send_with_retry`: an order is a write; KIS assigns its own order number
        // with no client idempotency key, so a retried submission could double-place (see #20).
        self.limiter.acquire().await;
        let resp: KisOrderResp = self
            .client
            .post(format!(
                "{}/uapi/domestic-stock/v1/trading/order-cash",
                self.base
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("appkey", &self.config.app_key)
            .header("appsecret", &self.config.app_secret)
            .header("tr_id", self.order_tr_id(order.side))
            .header("custtype", "P")
            .json(&body)
            .send()
            .await
            .map_err(broker_err)?
            .error_for_status()
            .map_err(broker_err)?
            .json()
            .await
            .map_err(broker_err)?;
        if resp.rt_cd != "0" {
            return Err(DomainError::Broker(format!(
                "KIS domestic order rt_cd={} {}",
                resp.rt_cd, resp.msg1
            )));
        }
        let odno = resp.output.odno.trim();
        if odno.is_empty() {
            return Err(DomainError::Broker(
                "KIS order accepted but returned no order number".into(),
            ));
        }
        let org = resp.output.krx_fwdg_ord_orgno.trim();
        let number = if org.is_empty() {
            odno.to_string()
        } else {
            format!("{org}/{odno}")
        };
        Ok(OrderId::new(number))
    }

    async fn cancel(&self, _id: &OrderId) -> Result<()> {
        Err(DomainError::Unsupported(
            "domestic KIS order cancellation is not implemented".into(),
        ))
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

#[derive(Debug, Deserialize)]
struct KisOrderResp {
    #[serde(default)]
    rt_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output: KisOrderOutput,
}

#[derive(Debug, Default, Deserialize)]
struct KisOrderOutput {
    // The order-cash response field casing isn't validated live yet; accept either case.
    #[serde(default, alias = "ODNO")]
    odno: String,
    #[serde(default, alias = "KRX_FWDG_ORD_ORGNO")]
    krx_fwdg_ord_orgno: String,
}

/// Page cap for paginated `inquire-daily-ccld` (모의 returns ~15 rows/page). Generous for a few
/// days of one account's orders; hitting it is surfaced as an error rather than silently
/// truncating fills (under-counting would make the strategy over-buy).
const MAX_DAILY_CCLD_PAGES: usize = 50;

/// `inquire-daily-ccld` response — `output1` is the per-order array (a filled row carries
/// `tot_ccld_qty`/`tot_ccld_amt`); `output2` is an account summary drip ignores. Like the other
/// inquiry endpoints the field names are lowercase, with the `100` pagination keys in the body.
#[derive(Debug, Deserialize)]
struct KisDailyCcldResp {
    #[serde(default)]
    rt_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output1: Vec<KisDailyCcldRow>,
    #[serde(default)]
    ctx_area_fk100: String,
    #[serde(default)]
    ctx_area_nk100: String,
}

#[derive(Debug, Default, Deserialize)]
struct KisDailyCcldRow {
    #[serde(default)]
    ord_dt: String,
    #[serde(default)]
    pdno: String,
    #[serde(default)]
    sll_buy_dvsn_cd: String,
    #[serde(default)]
    tot_ccld_qty: String,
    #[serde(default)]
    tot_ccld_amt: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kis::KisExchange;
    use drip_domain::{Price, Shares};
    use rust_decimal_macros::dec;
    use time::macros::date;
    use wiremock::matchers::{body_partial_json, header, method, path};
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

    #[tokio::test]
    async fn place_sends_a_limit_order_rounded_to_the_etf_tick() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        // 지정가 (00); the 188,477.3 limit rounds to the 5원 ETF tick → 188,475.
        Mock::given(method("POST"))
            .and(path("/uapi/domestic-stock/v1/trading/order-cash"))
            .and(body_partial_json(json!({
                "PDNO": "122630",
                "ORD_DVSN": "00",
                "ORD_QTY": "1",
                "ORD_UNPR": "188475",
                "EXCG_ID_DVSN_CD": "KRX"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0",
                "msg1": "ok",
                "output": {"KRX_FWDG_ORD_ORGNO": "00950", "ODNO": "0001234567"}
            })))
            .mount(&server)
            .await;

        let intent = OrderIntent::loc(
            Side::Buy,
            Shares::new(1),
            Price::new(dec!(188477.3)).unwrap(),
            "loc_low",
        );
        let id = broker(server.uri())
            .place(&Ticker::new("122630"), &intent)
            .await
            .unwrap();
        assert_eq!(id.as_str(), "00950/0001234567");
    }

    #[test]
    fn etf_tick_round_is_5won_at_or_above_2000_and_1won_below() {
        assert_eq!(etf_tick_round(dec!(188477.3)), dec!(188475)); // nearest 5원 tick (≥2,000)
        assert_eq!(etf_tick_round(dec!(188478)), dec!(188480)); // nearest 5원 tick
        assert_eq!(etf_tick_round(dec!(2002.5)), dec!(2000)); // exact half-tick → banker's (to even)
        assert_eq!(etf_tick_round(dec!(2000)), dec!(2000)); // boundary → 5원 tick
        assert_eq!(etf_tick_round(dec!(1999.6)), dec!(2000)); // <2,000 → nearest 1원
        assert_eq!(etf_tick_round(dec!(1850.4)), dec!(1850)); // <2,000 → nearest 1원
    }

    #[tokio::test]
    async fn place_refuses_a_real_account() {
        // Domestic placement is 모의-only until execution-history reconcile (#22); a real account
        // must be refused before any network call (no order is ever sent).
        let broker = KisDomesticBroker {
            config: KisConfig {
                environment: KisEnv::Real,
                ..config()
            },
            base: "http://unused.invalid".into(),
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
            limiter: Arc::new(RateLimiter::new(Duration::ZERO, None)),
            retry_backoff: Duration::ZERO,
        };
        let intent = OrderIntent::loc(
            Side::Buy,
            Shares::new(1),
            Price::new(dec!(200000)).unwrap(),
            "loc",
        );
        assert!(broker.place(&Ticker::new("122630"), &intent).await.is_err());
    }

    #[tokio::test]
    async fn fills_since_derives_the_price_and_skips_unfilled_and_other_tickers() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/domestic-stock/v1/trading/inquire-daily-ccld"))
            .and(header("tr_id", "VTTC0081R")) // paper, 3개월 이내
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0", "msg1": "ok", "ctx_area_fk100": "", "ctx_area_nk100": "",
                "output1": [
                    // Filled buy of our ticker: 2 shares, 376,950원 total → 188,475원 avg price.
                    {"ord_dt": "20260624", "pdno": "122630", "sll_buy_dvsn_cd": "02",
                     "tot_ccld_qty": "2", "tot_ccld_amt": "376950"},
                    // A later filled sell.
                    {"ord_dt": "20260625", "pdno": "122630", "sll_buy_dvsn_cd": "01",
                     "tot_ccld_qty": "1", "tot_ccld_amt": "200000"},
                    // Ordered but nothing filled yet -> skip.
                    {"ord_dt": "20260624", "pdno": "122630", "sll_buy_dvsn_cd": "02",
                     "tot_ccld_qty": "0", "tot_ccld_amt": "0"},
                    // A different ticker (a paper query returns the whole account) -> skip.
                    {"ord_dt": "20260624", "pdno": "069500", "sll_buy_dvsn_cd": "02",
                     "tot_ccld_qty": "5", "tot_ccld_amt": "100000"}
                ]
            })))
            .mount(&server)
            .await;

        let fills = broker(server.uri())
            .fills_since(&Ticker::new("122630"), date!(2026 - 06 - 01))
            .await
            .unwrap();
        assert_eq!(fills.len(), 2);
        // The execution price is derived from total filled value ÷ filled quantity.
        assert_eq!(
            fills[0],
            Fill {
                side: Side::Buy,
                shares: Shares::new(2),
                price: Price::new(dec!(188475)).unwrap(),
                at: date!(2026 - 06 - 24),
            }
        );
        assert_eq!(fills[1].side, Side::Sell);
        assert_eq!(fills[1].shares, Shares::new(1));
        assert_eq!(fills[1].price, Price::new(dec!(200000)).unwrap());
        assert_eq!(fills[1].at, date!(2026 - 06 - 25));
    }
}
