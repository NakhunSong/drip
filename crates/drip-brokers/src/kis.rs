//! 한국투자증권 (KIS) adapter — overseas quotes, balance, and (M2) live order placement.
//!
//! Quotes and balance are read-only. M2 adds [`drip_domain::OrderGateway`]: `place` posts a
//! real overseas order, so the type-level read-only guarantee of M1 is replaced by *runtime*
//! guards (capability + dry-run-by-default + a real-account flag + a pre-trade risk check),
//! enforced by the `drip tick` use case rather than by the absence of the trait. Endpoints
//! follow the official `koreainvestment/open-trading-api` reference. Realtime (WebSocket)
//! quotes, an execution-history endpoint, and order cancellation are later enhancements;
//! quote rate-limiting (~20 req/s real, ~5 req/s paper) arrives with the polling engine.

use crate::http::today_utc;
use crate::http::{TokenCache, broker_err, parse_decimal, parse_price_opt, parse_shares};
use async_trait::async_trait;
use drip_domain::{
    AccountQuery, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money,
    OrderGateway, OrderId, OrderIntent, OrderKind, Quote, Quotes, Result, Side, Ticker,
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
    /// US overseas order `tr_id`, by side and environment. The paper (모의) sell id is
    /// `VTTT1001U` — deliberately asymmetric with the buy id, per the KIS reference.
    fn order_tr_id(self, side: Side) -> &'static str {
        match (self, side) {
            (KisEnv::Real, Side::Buy) => "TTTT1002U",
            (KisEnv::Real, Side::Sell) => "TTTT1006U",
            (KisEnv::Paper, Side::Buy) => "VTTT1002U",
            (KisEnv::Paper, Side::Sell) => "VTTT1001U",
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
    /// 4-letter code used by the balance and order endpoints (`OVRS_EXCG_CD`).
    fn excg_code_4(self) -> &'static str {
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

/// A KIS broker adapter.
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
                ("OVRS_EXCG_CD", self.config.exchange.excg_code_4()),
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

    /// Map an order kind to a KIS US `ORD_DVSN` code. The 모의(paper) server accepts only
    /// limit (`00`) for US, so an LOC order is sent there as a day-limit at the close price
    /// — a deliberate, surfaced degradation (a paper LOC test fills like a day limit, not at
    /// the official close). Real accounts use the true LOC code `34`.
    fn ord_dvsn(&self, kind: OrderKind) -> Result<&'static str> {
        Ok(match (kind, self.config.environment) {
            (OrderKind::Limit, _) => "00",
            (OrderKind::LimitOnClose, KisEnv::Real) => "34",
            (OrderKind::LimitOnClose, KisEnv::Paper) => "00",
            (OrderKind::Market, _) => {
                return Err(DomainError::Unsupported(
                    "KIS US market orders are not supported by drip (the strategy uses LOC/limit)"
                        .into(),
                ));
            }
        })
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
            order_placement: true,
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

#[async_trait]
impl OrderGateway for KisBroker {
    async fn place(&self, ticker: &Ticker, order: &OrderIntent) -> Result<OrderId> {
        let ord_dvsn = self.ord_dvsn(order.kind)?;
        // KIS US prices trade in $0.01 ticks, but the strategy's averaged / variable-price
        // legs can carry many fractional digits; round to two decimals or the broker rejects
        // the order. A market intent (rejected above) has no limit; LOC/limit always carry one.
        let unit_price = order
            .limit
            .map(|price| price.value().round_dp(2).normalize().to_string())
            .unwrap_or_else(|| "0".to_string());
        let token = self.token().await?;
        let body = json!({
            "CANO": self.config.cano,
            "ACNT_PRDT_CD": self.config.product_code,
            "OVRS_EXCG_CD": self.config.exchange.excg_code_4(),
            "PDNO": ticker.as_str(),
            "ORD_QTY": order.shares.get().to_string(),
            "OVRS_ORD_UNPR": unit_price,
            "ORD_SVR_DVSN_CD": "0",
            "ORD_DVSN": ord_dvsn,
        });
        // Only the order summary is logged — never the auth headers or secrets.
        let resp: KisOrderResp = self
            .client
            .post(format!(
                "{}/uapi/overseas-stock/v1/trading/order",
                self.base
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("appkey", &self.config.app_key)
            .header("appsecret", &self.config.app_secret)
            .header("tr_id", self.config.environment.order_tr_id(order.side))
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
                "KIS order rt_cd={} {}",
                resp.rt_cd, resp.msg1
            )));
        }
        // ODNO is the required order number; prefix the KRX forwarding org number when
        // present so a later reconcile/cancel has both parts.
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
            "KIS order cancellation is not implemented in M2.1 — the daily tick places \
             idempotent LOC/limit orders and never cancels"
                .into(),
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
    #[serde(default, rename = "KRX_FWDG_ORD_ORGNO")]
    krx_fwdg_ord_orgno: String,
    #[serde(default, rename = "ODNO")]
    odno: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::{Price, Shares};
    use rust_decimal_macros::dec;
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

    fn broker(base: String) -> KisBroker {
        KisBroker {
            config: config(),
            base,
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
        }
    }

    fn broker_with(env: KisEnv, base: String) -> KisBroker {
        KisBroker {
            config: KisConfig {
                environment: env,
                ..config()
            },
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
        assert_eq!(holdings[0].shares, Shares::new(10));
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
    async fn paper_environment_advertises_paper_account_and_order_placement() {
        let caps = broker("http://unused".into()).capabilities();
        assert!(caps.paper_account);
        assert!(caps.order_placement); // M2: KIS can place orders (모의 and real)
        assert!(!caps.realtime_quotes);
    }

    #[tokio::test]
    async fn place_buy_loc_on_paper_degrades_to_limit_division() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/uapi/overseas-stock/v1/trading/order"))
            .and(header("tr_id", "VTTT1002U"))
            .and(body_partial_json(
                json!({"ORD_DVSN": "00", "PDNO": "TQQQ", "ORD_QTY": "4", "OVRS_ORD_UNPR": "100"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0", "msg1": "ok",
                "output": {"KRX_FWDG_ORD_ORGNO": "0000", "ODNO": "0030"}
            })))
            .mount(&server)
            .await;

        let intent = OrderIntent::loc(
            Side::Buy,
            Shares::new(4),
            Price::new(dec!(100)).unwrap(),
            "loc_low",
        );
        let id = broker(server.uri())
            .place(&Ticker::new("TQQQ"), &intent)
            .await
            .unwrap();
        assert_eq!(id, OrderId::new("0000/0030"));
    }

    #[tokio::test]
    async fn place_buy_loc_on_real_uses_loc_division_and_real_tr_id() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/uapi/overseas-stock/v1/trading/order"))
            .and(header("tr_id", "TTTT1002U"))
            .and(body_partial_json(json!({"ORD_DVSN": "34"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "0", "msg1": "ok",
                "output": {"KRX_FWDG_ORD_ORGNO": "", "ODNO": "55"}
            })))
            .mount(&server)
            .await;

        let intent = OrderIntent::loc(
            Side::Buy,
            Shares::new(2),
            Price::new(dec!(90)).unwrap(),
            "loc_high",
        );
        // An empty org number falls back to the bare order number.
        let id = broker_with(KisEnv::Real, server.uri())
            .place(&Ticker::new("TQQQ"), &intent)
            .await
            .unwrap();
        assert_eq!(id, OrderId::new("55"));
    }

    #[tokio::test]
    async fn place_rejects_market_orders_without_calling_the_api() {
        let intent = OrderIntent::market(Side::Buy, Shares::new(1), "mkt");
        let result = broker("http://unused".into())
            .place(&Ticker::new("TQQQ"), &intent)
            .await;
        assert!(matches!(result, Err(DomainError::Unsupported(_))));
    }

    #[tokio::test]
    async fn place_propagates_error_rt_cd() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/uapi/overseas-stock/v1/trading/order"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rt_cd": "1", "msg1": "rejected", "output": {}
            })))
            .mount(&server)
            .await;

        let intent = OrderIntent::limit(
            Side::Sell,
            Shares::new(1),
            Price::new(dec!(115)).unwrap(),
            "tp",
        );
        assert!(
            broker(server.uri())
                .place(&Ticker::new("TQQQ"), &intent)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cancel_is_unsupported_in_m2_1() {
        let result = broker("http://unused".into())
            .cancel(&OrderId::new("x"))
            .await;
        assert!(matches!(result, Err(DomainError::Unsupported(_))));
    }
}
