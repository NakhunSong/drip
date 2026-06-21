//! Read-only 한국투자증권 (KIS) adapter — overseas quotes and balance.
//!
//! It deliberately does **not** implement [`drip_domain::OrderGateway`]: in M1 there is no
//! type-level path to place a live order through KIS. Endpoints follow the official
//! `koreainvestment/open-trading-api` reference. Realtime (WebSocket) quotes and an
//! execution-history endpoint are later enhancements; quote rate-limiting (~20 req/s real,
//! ~5 req/s paper) arrives with the polling engine.

use crate::http::today_utc;
use crate::http::{TokenCache, broker_err, parse_decimal, parse_price_opt, parse_shares};
use async_trait::async_trait;
use drip_domain::{
    AccountQuery, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money, Quote,
    Quotes, Result, Ticker,
};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// KIS environment: real trading vs the paper-trading (VTS) server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KisEnv {
    Real,
    Paper,
}
impl KisEnv {
    fn base_url(self) -> &'static str {
        match self {
            KisEnv::Real => "https://openapi.koreainvestment.com:9443",
            KisEnv::Paper => "https://openapivts.koreainvestment.com:29443",
        }
    }
    /// The balance-inquiry `tr_id` differs by environment (`T*` real, `V*` paper); the quote
    /// `tr_id` is environment-invariant.
    fn balance_tr_id(self) -> &'static str {
        match self {
            KisEnv::Real => "TTTS3012R",
            KisEnv::Paper => "VTTS3012R",
        }
    }
}

/// US exchange for the (single-exchange in M1) KIS instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KisExchange {
    Nasdaq,
    Nyse,
    Amex,
}
impl KisExchange {
    /// 3-letter code used by the quote endpoint.
    fn quote_code(self) -> &'static str {
        match self {
            KisExchange::Nasdaq => "NAS",
            KisExchange::Nyse => "NYS",
            KisExchange::Amex => "AMS",
        }
    }
    /// 4-letter code used by the balance endpoint.
    fn balance_code(self) -> &'static str {
        match self {
            KisExchange::Nasdaq => "NASD",
            KisExchange::Nyse => "NYSE",
            KisExchange::Amex => "AMEX",
        }
    }
}

/// Configuration for a KIS adapter instance.
#[derive(Debug, Clone)]
pub struct KisConfig {
    pub environment: KisEnv,
    pub app_key: String,
    pub app_secret: String,
    /// Account number, first 8 digits (CANO).
    pub cano: String,
    /// Account product code, last 2 digits (ACNT_PRDT_CD).
    pub product_code: String,
    /// Default exchange for quotes and balance. M1 uses one exchange per instance;
    /// per-ticker exchange resolution is a later enhancement.
    pub exchange: KisExchange,
}

/// A read-only KIS broker adapter.
#[derive(Debug)]
pub struct KisBroker {
    config: KisConfig,
    base: String,
    client: reqwest::Client,
    tokens: TokenCache,
}

impl KisBroker {
    pub fn new(config: KisConfig) -> Result<KisBroker> {
        let base = config.environment.base_url().to_string();
        let client = reqwest::Client::builder().build().map_err(broker_err)?;
        Ok(KisBroker {
            config,
            base,
            client,
            tokens: TokenCache::new(),
        })
    }

    async fn token(&self) -> Result<String> {
        let base = self.base.clone();
        let app_key = self.config.app_key.clone();
        let app_secret = self.config.app_secret.clone();
        let client = self.client.clone();
        self.tokens
            .get_or_refresh(move || async move {
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

    async fn fetch_balance(&self) -> Result<KisBalanceResp> {
        let token = self.token().await?;
        let body: KisBalanceResp = self
            .client
            .get(format!(
                "{}/uapi/overseas-stock/v1/trading/inquire-balance",
                self.base
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("appkey", &self.config.app_key)
            .header("appsecret", &self.config.app_secret)
            .header("tr_id", self.config.environment.balance_tr_id())
            .header("custtype", "P")
            .query(&[
                ("CANO", self.config.cano.as_str()),
                ("ACNT_PRDT_CD", self.config.product_code.as_str()),
                ("OVRS_EXCG_CD", self.config.exchange.balance_code()),
                ("TR_CRCY_CD", "USD"),
                ("CTX_AREA_FK200", ""),
                ("CTX_AREA_NK200", ""),
            ])
            .send()
            .await
            .map_err(broker_err)?
            .error_for_status()
            .map_err(broker_err)?
            .json()
            .await
            .map_err(broker_err)?;
        if body.rt_cd != "0" {
            return Err(DomainError::Broker(format!(
                "KIS balance rt_cd={} {}",
                body.rt_cd, body.msg1
            )));
        }
        Ok(body)
    }
}

impl BrokerInfo for KisBroker {
    fn id(&self) -> BrokerId {
        BrokerId::Kis
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            realtime_quotes: false,
            paper_account: self.config.environment == KisEnv::Paper,
            order_placement: false,
            overseas: true,
        }
    }
}

#[async_trait]
impl Quotes for KisBroker {
    async fn quote(&self, ticker: &Ticker) -> Result<Quote> {
        let token = self.token().await?;
        let body: KisQuoteResp = self
            .client
            .get(format!(
                "{}/uapi/overseas-price/v1/quotations/price",
                self.base
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("appkey", &self.config.app_key)
            .header("appsecret", &self.config.app_secret)
            .header("tr_id", "HHDFS00000300")
            .header("custtype", "P")
            .query(&[
                ("AUTH", ""),
                ("EXCD", self.config.exchange.quote_code()),
                ("SYMB", ticker.as_str()),
            ])
            .send()
            .await
            .map_err(broker_err)?
            .error_for_status()
            .map_err(broker_err)?
            .json()
            .await
            .map_err(broker_err)?;
        if body.rt_cd != "0" {
            return Err(DomainError::Broker(format!(
                "KIS quote rt_cd={} {}",
                body.rt_cd, body.msg1
            )));
        }
        let price = parse_price_opt(&body.output.last)?
            .ok_or_else(|| DomainError::Broker(format!("KIS returned no price for {ticker}")))?;
        Ok(Quote {
            ticker: ticker.clone(),
            price,
            as_of: today_utc(),
        })
    }
}

#[async_trait]
impl AccountQuery for KisBroker {
    async fn holdings(&self) -> Result<Vec<Holding>> {
        let body = self.fetch_balance().await?;
        let mut holdings = Vec::new();
        for lot in &body.output1 {
            let shares = parse_shares(&lot.ovrs_cblc_qty)?;
            if shares.is_zero() {
                continue;
            }
            holdings.push(Holding {
                ticker: Ticker::new(&lot.ovrs_pdno),
                shares,
                avg_price: parse_price_opt(&lot.pchs_avg_pric)?,
            });
        }
        Ok(holdings)
    }

    async fn balance(&self) -> Result<Money> {
        // inquire-balance has no clean deposit figure; report total holdings evaluation
        // amount. Spendable cash uses inquire-present-balance (a later enhancement).
        let body = self.fetch_balance().await?;
        Ok(Money::new(parse_decimal(&body.output2.ovrs_stck_evlu_amt)?))
    }

    async fn fills_since(&self, _since: time::Date) -> Result<Vec<Fill>> {
        Err(DomainError::Unsupported(
            "KIS execution history is not implemented in M1".into(),
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
    last: String,
}

#[derive(Debug, Deserialize)]
struct KisBalanceResp {
    #[serde(default)]
    rt_cd: String,
    #[serde(default)]
    msg1: String,
    #[serde(default)]
    output1: Vec<KisHolding>,
    #[serde(default)]
    output2: KisSummary,
}

#[derive(Debug, Default, Deserialize)]
struct KisSummary {
    #[serde(default)]
    ovrs_stck_evlu_amt: String,
}

#[derive(Debug, Deserialize)]
struct KisHolding {
    #[serde(default)]
    ovrs_pdno: String,
    #[serde(default)]
    ovrs_cblc_qty: String,
    #[serde(default)]
    pchs_avg_pric: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::Price;
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

    fn broker(base: String) -> KisBroker {
        KisBroker {
            config: config(),
            base,
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
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
    async fn quote_parses_last_price() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/overseas-price/v1/quotations/price"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"rt_cd": "0", "msg1": "ok", "output": {"last": "123.45"}}),
                ),
            )
            .mount(&server)
            .await;

        let quote = broker(server.uri())
            .quote(&Ticker::new("TQQQ"))
            .await
            .unwrap();
        assert_eq!(quote.price, Price::new(dec!(123.45)).unwrap());
    }

    #[tokio::test]
    async fn holdings_skip_zero_quantity_lots() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/overseas-stock/v1/trading/inquire-balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0",
                "msg1": "ok",
                "output1": [
                    {"ovrs_pdno": "TQQQ", "ovrs_cblc_qty": "10", "pchs_avg_pric": "55.5"},
                    {"ovrs_pdno": "SOXL", "ovrs_cblc_qty": "0", "pchs_avg_pric": "0"}
                ],
                "output2": {"ovrs_stck_evlu_amt": "600.00"}
            })))
            .mount(&server)
            .await;

        let broker = broker(server.uri());
        let holdings = broker.holdings().await.unwrap();
        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].ticker, Ticker::new("TQQQ"));
        assert_eq!(holdings[0].shares, drip_domain::Shares::new(10));
        assert_eq!(holdings[0].avg_price, Price::new(dec!(55.5)));
        assert_eq!(broker.balance().await.unwrap(), Money::new(dec!(600.00)));
    }

    #[tokio::test]
    async fn error_rt_cd_is_propagated() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/uapi/overseas-price/v1/quotations/price"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"rt_cd": "1", "msg1": "rate limit", "output": {"last": ""}}),
                ),
            )
            .mount(&server)
            .await;

        assert!(
            broker(server.uri())
                .quote(&Ticker::new("TQQQ"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn paper_environment_advertises_paper_account_and_no_orders() {
        let caps = broker("http://unused".into()).capabilities();
        assert!(caps.paper_account);
        assert!(!caps.order_placement);
        assert!(!caps.realtime_quotes);
    }
}
