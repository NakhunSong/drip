//! `drip` — the CLI and a composition root. It parses commands, builds the infrastructure
//! adapters, and dispatches to `drip-app` use cases. Business logic lives in the use cases
//! (shared with the web dashboard), so these handlers stay thin: parse → call → print.

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use drip_app::BacktestReport;
use drip_app::{
    AccountView, DryRunView, account_snapshot, dry_run, fetch_quote, list_positions, run_backtest,
};
use drip_brokers::connect;
use drip_domain::{StateRepository, Ticker};
use drip_infra::{
    AppConfig, CsvMarketData, FileSecretStore, PositionConfig, SqliteStateRepository, parse_date,
};
use drip_strategies::StrategyRegistry;
use rust_decimal::Decimal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use time::Date;

#[derive(Parser)]
#[command(
    name = "drip",
    version,
    about = "Seamless, extensible automated trading CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize the drip home directory and config.
    Init,
    /// Store broker API credentials (in ~/.drip/secrets.toml, 0600) and validate them.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Show holdings and balance for a broker (read-only).
    Account {
        #[arg(long)]
        broker: String,
    },
    /// Fetch a current quote (read-only).
    Quote {
        ticker: String,
        #[arg(long)]
        broker: String,
    },
    /// Manage configured positions.
    Strategy {
        #[command(subcommand)]
        action: StrategyAction,
    },
    /// Backtest a configured position over a CSV of daily bars.
    Backtest {
        #[arg(long)]
        name: String,
        #[arg(long)]
        data: PathBuf,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Compute today's orders for a position from a live quote (no orders placed).
    DryRun {
        #[arg(long)]
        name: String,
    },
    /// Show persisted positions.
    Status,
    /// Serve the read-only web dashboard.
    Web {
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: SocketAddr,
    },
}

#[derive(Subcommand)]
enum KeysAction {
    /// Store 한국투자증권 (KIS) credentials.
    Kis {
        #[arg(long)]
        app_key: String,
        #[arg(long)]
        app_secret: String,
        #[arg(long)]
        cano: String,
        #[arg(long)]
        product_code: String,
        #[arg(long, default_value = "paper")]
        env: String,
        #[arg(long, default_value = "nasdaq")]
        exchange: String,
    },
    /// Store 토스증권 (Toss) credentials.
    Toss {
        #[arg(long)]
        app_key: String,
        #[arg(long)]
        app_secret: String,
        #[arg(long)]
        account_seq: i64,
    },
}

#[derive(Subcommand)]
enum StrategyAction {
    /// Add or update a position.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        broker: String,
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        seed: Decimal,
        #[arg(long, default_value_t = 40)]
        splits: u32,
        #[arg(long)]
        take_profit_pct: Option<Decimal>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    drip_infra::logging::init();
    let cli = Cli::parse();
    drip_infra::ensure_home()?;
    let secrets = FileSecretStore::new(drip_infra::secrets_path()?);
    let config_path = drip_infra::config_path()?;
    let state_path = drip_infra::state_path()?;

    match cli.command {
        Command::Init => cmd_init()?,
        Command::Keys { action } => cmd_keys(action, &secrets).await?,
        Command::Account { broker } => cmd_account(&broker, &secrets).await?,
        Command::Quote { ticker, broker } => cmd_quote(&ticker, &broker, &secrets).await?,
        Command::Strategy { action } => match action {
            StrategyAction::Add {
                name,
                broker,
                ticker,
                seed,
                splits,
                take_profit_pct,
            } => {
                cmd_strategy_add(
                    PositionConfig {
                        name,
                        broker,
                        ticker,
                        seed,
                        splits,
                        strategy: "infinite-buying".to_string(),
                        take_profit_pct,
                    },
                    &config_path,
                    state_path,
                )
                .await?
            }
        },
        Command::Backtest {
            name,
            data,
            from,
            to,
        } => cmd_backtest(&name, data, from, to, &config_path).await?,
        Command::DryRun { name } => cmd_dry_run(&name, &secrets, &config_path, state_path).await?,
        Command::Status => cmd_status(state_path).await?,
        Command::Web { addr } => drip_web::serve(addr).await?,
    }
    Ok(())
}

fn cmd_init() -> Result<()> {
    let home = drip_infra::ensure_home()?;
    let config_path = drip_infra::config_path()?;
    if !config_path.exists() {
        AppConfig::default().save(&config_path)?;
    }
    println!("Initialized drip home at {}", home.display());
    println!("Config:  {}", config_path.display());
    println!("Secrets: {}", drip_infra::secrets_path()?.display());
    Ok(())
}

async fn cmd_keys(action: KeysAction, secrets: &FileSecretStore) -> Result<()> {
    use drip_domain::SecretStore;
    let broker = match action {
        KeysAction::Kis {
            app_key,
            app_secret,
            cano,
            product_code,
            env,
            exchange,
        } => {
            drip_brokers::parse_exchange(&exchange)?; // validate before persisting
            secrets.set("kis_app_key", &app_key)?;
            secrets.set("kis_app_secret", &app_secret)?;
            secrets.set("kis_cano", &cano)?;
            secrets.set("kis_product_code", &product_code)?;
            secrets.set("kis_env", &env)?;
            secrets.set("kis_exchange", &exchange)?;
            "kis"
        }
        KeysAction::Toss {
            app_key,
            app_secret,
            account_seq,
        } => {
            secrets.set("toss_app_key", &app_key)?;
            secrets.set("toss_app_secret", &app_secret)?;
            secrets.set("toss_account_seq", &account_seq.to_string())?;
            "toss"
        }
    };
    println!("Stored {broker} credentials.");
    match connect(broker, secrets) {
        Ok(live) => match fetch_quote(live.as_quotes(), &Ticker::new("AAPL")).await {
            Ok(quote) => println!("Validated: AAPL = {}", quote.price),
            Err(e) => println!("Warning: credential probe failed: {e}"),
        },
        Err(e) => println!("Warning: {e}"),
    }
    Ok(())
}

async fn cmd_account(broker: &str, secrets: &FileSecretStore) -> Result<()> {
    let live = connect(broker, secrets)?;
    print_account(&account_snapshot(live.as_account()).await?);
    Ok(())
}

async fn cmd_quote(ticker: &str, broker: &str, secrets: &FileSecretStore) -> Result<()> {
    let live = connect(broker, secrets)?;
    let quote = fetch_quote(live.as_quotes(), &Ticker::new(ticker)).await?;
    println!(
        "{} {} = {} (as of {})",
        broker, quote.ticker, quote.price, quote.as_of
    );
    Ok(())
}

async fn cmd_strategy_add(
    position: PositionConfig,
    config_path: &Path,
    state_path: PathBuf,
) -> Result<()> {
    let mut config = AppConfig::load(config_path)?;
    config.upsert(position.clone());
    config.save(config_path)?;

    // Persist a flat position so `status`/`dry-run` have state to work from.
    SqliteStateRepository::open(state_path)?
        .save(&position.to_position()?)
        .await?;

    println!(
        "Added position '{}' ({} {} seed {} / {} splits).",
        position.name, position.broker, position.ticker, position.seed, position.splits
    );
    Ok(())
}

async fn cmd_backtest(
    name: &str,
    data: PathBuf,
    from: Option<String>,
    to: Option<String>,
    config_path: &Path,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let position = config
        .find(name)
        .ok_or_else(|| anyhow!("no position '{name}' — add one with `drip strategy add`"))?;
    let registry = StrategyRegistry::with_builtins();
    let strategy = registry.build(&position.strategy, &position.strategy_params())?;
    let from = from
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or(Date::MIN);
    let to = to
        .as_deref()
        .map(parse_date)
        .transpose()?
        .unwrap_or(Date::MAX);
    let source = CsvMarketData::new(data);
    let report = run_backtest(
        &source,
        strategy.as_ref(),
        position.to_position()?,
        from,
        to,
    )
    .await?;
    print_report(&report);
    Ok(())
}

async fn cmd_dry_run(
    name: &str,
    secrets: &FileSecretStore,
    config_path: &Path,
    state_path: PathBuf,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let position = config
        .find(name)
        .ok_or_else(|| anyhow!("no position '{name}' — add one with `drip strategy add`"))?;
    let live = connect(&position.broker, secrets)?;
    let registry = StrategyRegistry::with_builtins();
    let strategy = registry.build(&position.strategy, &position.strategy_params())?;
    let repo = SqliteStateRepository::open(state_path)?;
    let view = dry_run(
        live.as_quotes(),
        &repo,
        position.to_position()?,
        strategy.as_ref(),
    )
    .await?;
    print_dry_run(name, &view);
    Ok(())
}

async fn cmd_status(state_path: PathBuf) -> Result<()> {
    let repo = SqliteStateRepository::open(state_path)?;
    let positions = list_positions(&repo).await?;
    if positions.is_empty() {
        println!("No positions. Add one with `drip strategy add`.");
        return Ok(());
    }
    for p in &positions {
        let avg = p
            .avg_price
            .map(|x| x.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{} {:8} T={:<5} avg={:>8} shares={:>6} cum={} realized={} cycle={}",
            p.broker,
            p.ticker,
            p.t(),
            avg,
            p.shares,
            p.cum_spent,
            p.realized_pnl,
            p.cycle_index
        );
    }
    Ok(())
}

fn print_account(view: &AccountView) {
    let c = view.capabilities;
    println!(
        "Broker {} (realtime={}, paper={}, orders={}, overseas={})",
        view.broker, c.realtime_quotes, c.paper_account, c.order_placement, c.overseas
    );
    if view.holdings.is_empty() {
        println!("No holdings.");
    }
    for h in &view.holdings {
        let avg = h
            .avg_price
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!("  {:8} qty={:>8} avg={}", h.ticker, h.shares, avg);
    }
    match &view.balance {
        Some(balance) => println!("Cash/eval: {balance}"),
        None => println!("Cash/eval: unavailable"),
    }
}

fn print_dry_run(name: &str, view: &DryRunView) {
    let avg = view
        .avg_price
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
    println!(
        "Dry-run {name}: {} @ {} (T={}, avg={})",
        view.ticker, view.price, view.t, avg
    );
    if view.intents.is_empty() {
        println!("  (no orders today)");
    }
    for intent in &view.intents {
        let limit = intent
            .limit
            .map(|p| p.to_string())
            .unwrap_or_else(|| "MKT".to_string());
        println!(
            "  {:?} {} {:?} @ {} [{}]",
            intent.side, intent.shares, intent.kind, limit, intent.tag
        );
    }
    println!("NOTE: dry-run only — no orders were placed.");
}

fn print_report(report: &BacktestReport) {
    println!(
        "Backtest {} {} .. {}",
        report.ticker, report.start, report.end
    );
    println!("  seed           {}", report.initial_seed);
    println!("  final equity   {}", report.final_equity);
    println!("  realized P&L   {}", report.realized_pnl);
    println!("  cycles         {}", report.cycles_completed);
    println!(
        "  trading days   {}/{}",
        report.trading_days, report.total_days
    );
    println!("  max drawdown   {:.2}%", report.max_drawdown * 100.0);
    println!("  CAGR           {:.2}%", report.cagr * 100.0);
}
