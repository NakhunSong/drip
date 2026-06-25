//! `drip-web` — a minimal embedded dashboard (axum).
//!
//! A driving adapter: every handler calls a `drip-app` use case, never a broker or sqlite
//! directly. The dashboard HTML is compiled into the binary (`include_str!`), so `drip web`
//! serves it with no separate runtime or build step — consistent with the single-binary goal.

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use drip_app::{AccountView, BacktestReport};
use drip_brokers::connect;
use drip_domain::{Position, Quote, Ticker};
use drip_infra::{AppConfig, CsvMarketData, FileSecretStore, SqliteStateRepository};
use drip_strategies::StrategyRegistry;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use time::Date;

struct AppState {
    secrets_path: PathBuf,
    config_path: PathBuf,
    state_path: PathBuf,
}

/// Start the dashboard server bound to `addr`, resolving config/secrets/state from `~/.drip`.
pub async fn serve(addr: SocketAddr) -> Result<()> {
    drip_infra::ensure_home()?;
    let state = Arc::new(AppState {
        secrets_path: drip_infra::secrets_path()?,
        config_path: drip_infra::config_path()?,
        state_path: drip_infra::state_path()?,
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/quote", get(api_quote))
        .route("/api/account", get(api_account))
        .route("/api/backtest", get(api_backtest))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("drip dashboard → http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD)
}

async fn api_status(State(s): State<Arc<AppState>>) -> Result<Json<Vec<Position>>, ApiError> {
    let repo = SqliteStateRepository::open(s.state_path.clone())?;
    Ok(Json(drip_app::list_positions(&repo).await?))
}

#[derive(Deserialize)]
struct QuoteQuery {
    account: String,
    broker: String,
    ticker: String,
}

async fn api_quote(
    State(s): State<Arc<AppState>>,
    Query(q): Query<QuoteQuery>,
) -> Result<Json<Quote>, ApiError> {
    let secrets = FileSecretStore::new(s.secrets_path.clone());
    let env = AppConfig::load(&s.config_path)?.env_for(&q.account);
    let live = connect(
        &q.broker,
        &q.account,
        &env,
        &secrets,
        s.secrets_path.parent(),
    )?;
    Ok(Json(
        drip_app::fetch_quote(live.as_quotes(), &Ticker::new(q.ticker)).await?,
    ))
}

#[derive(Deserialize)]
struct AccountQuery {
    account: String,
    broker: String,
}

async fn api_account(
    State(s): State<Arc<AppState>>,
    Query(q): Query<AccountQuery>,
) -> Result<Json<AccountView>, ApiError> {
    let secrets = FileSecretStore::new(s.secrets_path.clone());
    let env = AppConfig::load(&s.config_path)?.env_for(&q.account);
    let live = connect(
        &q.broker,
        &q.account,
        &env,
        &secrets,
        s.secrets_path.parent(),
    )?;
    Ok(Json(drip_app::account_snapshot(live.as_account()).await?))
}

#[derive(Deserialize)]
struct BacktestQuery {
    name: String,
    data: String,
    from: Option<String>,
    to: Option<String>,
}

async fn api_backtest(
    State(s): State<Arc<AppState>>,
    Query(q): Query<BacktestQuery>,
) -> Result<Json<BacktestReport>, ApiError> {
    let config = AppConfig::load(&s.config_path)?;
    let position = config
        .find(&q.name)
        .ok_or_else(|| anyhow::anyhow!("no position '{}'", q.name))?;
    let registry = StrategyRegistry::with_builtins();
    let strategy = registry.build(&position.strategy, &position.strategy_params())?;
    let from = q
        .from
        .as_deref()
        .map(drip_infra::parse_date)
        .transpose()?
        .unwrap_or(Date::MIN);
    let to =
        q.to.as_deref()
            .map(drip_infra::parse_date)
            .transpose()?
            .unwrap_or(Date::MAX);
    let source = CsvMarketData::new(PathBuf::from(q.data));
    let report = drip_app::run_backtest(
        &source,
        strategy.as_ref(),
        position.to_position()?,
        from,
        to,
    )
    .await?;
    Ok(Json(report))
}

/// Wraps any error into a JSON 500 response.
struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        ApiError(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

const DASHBOARD: &str = include_str!("dashboard.html");
