//! Read-only 토스증권 (Toss) adapter — overseas quotes and holdings.
//!
//! Like the KIS adapter it does **not** implement [`drip_domain::OrderGateway`]. Toss uses
//! OAuth2 client-credentials with ~1h tokens and a per-account header
//! (`X-Tossinvest-Account`). It has no WebSocket and no paper sandbox today, so the engine
//! falls back to REST polling and uses [`crate::PaperBroker`] for dry-runs. Response wrapper
//! shapes follow the public OpenAPI spec; a couple are tolerant by design (see below).

use crate::http::{TokenCache, broker_err, parse_price_opt, parse_shares, today_utc};
use async_trait::async_trait;
use drip_domain::{
    AccountQuery, BrokerId, BrokerInfo, Capabilities, DomainError, Fill, Holding, Money, Quote,
    Quotes, Result, Ticker,
};
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const TOSS_BASE: &str = "https://openapi.tossinvest.com";

/// Configuration for a Toss adapter instance.
#[derive(Debug, Clone)]
pub struct TossConfig {
    pub app_key: String,
    pub app_secret: String,
    /// Account sequence sent as the `X-Tossinvest-Account` header.
    pub account_seq: i64,
}

/// A read-only Toss broker adapter.
#[derive(Debug)]
pub struct TossBroker {
    config: TossConfig,
    base: String,
    client: reqwest::Client,
    tokens: TokenCache,
}

impl TossBroker {
    pub fn new(config: TossConfig) -> Result<TossBroker> {
        let client = reqwest::Client::builder().build().map_err(broker_err)?;
        Ok(TossBroker {
            config,
            base: TOSS_BASE.to_string(),
            client,
            tokens: TokenCache::new(),
        })
    }

    async fn token(&self) -> Result<String> {
        let base = self.base.clone();
        let client_id = self.config.app_key.clone();
        let client_secret = self.config.app_secret.clone();
        let client = self.client.clone();
        self.tokens
            .get_or_refresh(move || async move {
                let token: TossToken = client
                    .post(format!("{base}/oauth2/token"))
                    .form(&[
                        ("grant_type", "client_credentials"),
                        ("client_id", client_id.as_str()),
                        ("client_secret", client_secret.as_str()),
                    ])
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

    async fn get_json(&self, url: String, query: &[(&str, &str)]) -> Result<Value> {
        let token = self.token().await?;
        self.client
            .get(url)
            .header("authorization", format!("Bearer {token}"))
            .header("x-tossinvest-account", self.config.account_seq.to_string())
            .query(query)
            .send()
            .await
            .map_err(broker_err)?
            .error_for_status()
            .map_err(broker_err)?
            .json()
            .await
            .map_err(broker_err)
    }
}

/// Return the array under `result` if present, otherwise treat the root as the array.
fn result_array(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("result")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
}

/// Navigate `result.items` (Toss holdings) tolerantly.
fn result_items(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("result")
        .and_then(|r| r.get("items"))
        .and_then(Value::as_array)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| DomainError::Broker(format!("toss: missing field '{key}'")))
}

impl BrokerInfo for TossBroker {
    fn id(&self) -> BrokerId {
        BrokerId::Toss
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            realtime_quotes: false,
            paper_account: false,
            order_placement: false,
            overseas: true,
        }
    }
}

#[async_trait]
impl Quotes for TossBroker {
    async fn quote(&self, ticker: &Ticker) -> Result<Quote> {
        let body = self
            .get_json(
                format!("{}/api/v1/prices", self.base),
                &[("symbols", ticker.as_str())],
            )
            .await?;
        let first = result_array(&body)
            .and_then(|a| a.first())
            .ok_or_else(|| DomainError::Broker(format!("toss: no price for {ticker}")))?;
        let price = parse_price_opt(str_field(first, "lastPrice")?)?
            .ok_or_else(|| DomainError::Broker(format!("toss: no price for {ticker}")))?;
        Ok(Quote {
            ticker: ticker.clone(),
            price,
            as_of: today_utc(),
        })
    }
}

#[async_trait]
impl AccountQuery for TossBroker {
    async fn holdings(&self) -> Result<Vec<Holding>> {
        let body = self
            .get_json(format!("{}/api/v1/holdings", self.base), &[])
            .await?;
        let items = result_items(&body)
            .ok_or_else(|| DomainError::Broker("toss: holdings shape unexpected".into()))?;
        let mut holdings = Vec::new();
        for item in items {
            let shares = parse_shares(str_field(item, "quantity")?)?;
            if shares.is_zero() {
                continue;
            }
            holdings.push(Holding {
                ticker: Ticker::new(str_field(item, "symbol")?),
                shares,
                avg_price: parse_price_opt(str_field(item, "averagePurchasePrice")?)?,
            });
        }
        Ok(holdings)
    }

    async fn balance(&self) -> Result<Money> {
        // Toss buying-power is computed per symbol, so there is no symbol-independent cash
        // call; we read `cash` via a liquid reference symbol.
        let body = self
            .get_json(
                format!("{}/api/v1/buying-power", self.base),
                &[("symbol", "AAPL")],
            )
            .await?;
        let cash = body
            .get("result")
            .and_then(|r| r.get("cash"))
            .and_then(Value::as_str)
            .ok_or_else(|| DomainError::Broker("toss: missing result.cash".into()))?;
        Ok(Money::new(crate::http::parse_decimal(cash)?))
    }

    async fn fills_since(&self, _since: time::Date) -> Result<Vec<Fill>> {
        Err(DomainError::Unsupported(
            "Toss execution history is not implemented in M1".into(),
        ))
    }
}

#[derive(Debug, Deserialize)]
struct TossToken {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use drip_domain::{Price, Shares};
    use rust_decimal_macros::dec;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn broker(base: String) -> TossBroker {
        TossBroker {
            config: TossConfig {
                app_key: "k".into(),
                app_secret: "s".into(),
                account_seq: 7,
            },
            base,
            client: reqwest::Client::new(),
            tokens: TokenCache::new(),
        }
    }

    async fn mock_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"access_token": "tok", "token_type": "Bearer", "expires_in": 3600}),
            ))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn quote_parses_last_price_from_result_array() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/prices"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": [{"symbol": "TQQQ", "lastPrice": "88.20", "currency": "USD"}]
            })))
            .mount(&server)
            .await;

        let quote = broker(server.uri())
            .quote(&Ticker::new("TQQQ"))
            .await
            .unwrap();
        assert_eq!(quote.price, Price::new(dec!(88.20)).unwrap());
    }

    #[tokio::test]
    async fn holdings_parse_items_and_skip_empty() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/holdings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": {"items": [
                    {"symbol": "TQQQ", "quantity": "12", "averagePurchasePrice": "60.0"},
                    {"symbol": "SOXL", "quantity": "0", "averagePurchasePrice": "0"}
                ]}
            })))
            .mount(&server)
            .await;

        let holdings = broker(server.uri()).holdings().await.unwrap();
        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].shares, Shares::new(12));
        assert_eq!(holdings[0].avg_price, Price::new(dec!(60.0)));
    }

    #[tokio::test]
    async fn balance_reads_cash_from_buying_power() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/buying-power"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": {"cash": "1500.25", "currency": "USD"}
            })))
            .mount(&server)
            .await;

        assert_eq!(
            broker(server.uri()).balance().await.unwrap(),
            Money::new(dec!(1500.25))
        );
    }

    #[tokio::test]
    async fn capabilities_have_no_paper_no_orders_no_realtime() {
        let caps = broker("http://unused".into()).capabilities();
        assert!(!caps.paper_account);
        assert!(!caps.order_placement);
        assert!(!caps.realtime_quotes);
        assert!(caps.overseas);
    }
}
