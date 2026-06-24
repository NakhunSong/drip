//! `drip` — the CLI and a composition root. It parses commands, builds the infrastructure
//! adapters, and dispatches to `drip-app` use cases. Business logic lives in the use cases
//! (shared with the web dashboard), so these handlers stay thin: parse → call → print.

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use drip_app::BacktestReport;
use drip_app::{
    AccountView, DryRunView, ReconcileView, TickPorts, TickStatus, TickView, account_snapshot,
    dry_run, fetch_quote, list_positions, place_orders, reconcile, run_backtest,
};
use drip_brokers::{LiveBroker, connect};
use drip_domain::calendar::{Market, trading_date};
use drip_domain::{Position, Schedule, StateRepository, Strategy, Ticker, Trigger};
use drip_infra::{
    AppConfig, CsvMarketData, FileSecretStore, PositionConfig, SqliteStateRepository, parse_date,
};
use drip_strategies::StrategyRegistry;
use rust_decimal::Decimal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use time::Date;

mod engine;

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
    /// Compute and (with --execute) place today's orders for a position. Dry-run by default.
    Tick {
        #[arg(long)]
        name: String,
        /// Actually place orders. Omit for a preview that sends nothing.
        #[arg(long)]
        execute: bool,
        /// Required to place against a KIS *real* (not 모의) account.
        #[arg(long)]
        live: bool,
    },
    /// Reconcile settled fills into a position's ledger (advances T). Read-only at the broker.
    Reconcile {
        #[arg(long)]
        name: String,
    },
    /// Show the broker's reported executions (fills) for a position — a read-only diagnostic for
    /// reconcile (what `fills_since` sees). Never places.
    Fills {
        #[arg(long)]
        name: String,
        /// Only fills on/after this date (YYYY-MM-DD). Defaults to the reconcile watermark, else
        /// ~90 days back (the 모의 inquiry window).
        #[arg(long)]
        since: Option<String>,
    },
    /// Run the scheduler daemon: fire every configured position on its schedule, on US trading
    /// days. Dry-run by default; `--execute` places orders, `--live` allows a real account.
    Run {
        /// Actually place orders on each fire. Omit for a preview daemon that sends nothing.
        #[arg(long)]
        execute: bool,
        /// Required to place against a KIS *real* (not 모의) account.
        #[arg(long)]
        live: bool,
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
        Command::Tick {
            name,
            execute,
            live,
        } => cmd_tick(&name, execute, live, &secrets, &config_path, state_path).await?,
        Command::Reconcile { name } => {
            cmd_reconcile(&name, &secrets, &config_path, state_path).await?
        }
        Command::Fills { name, since } => cmd_fills(&name, since, &secrets, &config_path).await?,
        Command::Run { execute, live } => {
            cmd_run(execute, live, &secrets, &config_path, state_path).await?
        }
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
    match connect(broker, secrets, Some(drip_infra::drip_home()?.as_path())) {
        Ok(live) => match fetch_quote(live.as_quotes(), &Ticker::new("AAPL")).await {
            Ok(quote) => println!("Validated: AAPL = {}", quote.price),
            Err(e) => println!("Warning: credential probe failed: {e}"),
        },
        Err(e) => println!("Warning: {e}"),
    }
    Ok(())
}

async fn cmd_account(broker: &str, secrets: &FileSecretStore) -> Result<()> {
    let live = connect(broker, secrets, Some(drip_infra::drip_home()?.as_path()))?;
    print_account(&account_snapshot(live.as_account()).await?);
    Ok(())
}

async fn cmd_quote(ticker: &str, broker: &str, secrets: &FileSecretStore) -> Result<()> {
    let live = connect(broker, secrets, Some(drip_infra::drip_home()?.as_path()))?;
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
    let live = connect(
        &position.broker,
        secrets,
        Some(drip_infra::drip_home()?.as_path()),
    )?;
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

/// The market whose trading calendar applies to a position, by its configured broker.
/// `kis-domestic` (and Toss) trade on KRX → KST sessions; everything else is treated as US
/// equities. Drives the trading-date computation that the idempotency key and the reconcile
/// boundary key on, so a same-session rerun maps to one date.
fn position_market(broker: &str) -> Market {
    match broker {
        "kis-domestic" | "toss" => Market::KrEquity,
        _ => Market::UsEquity,
    }
}

async fn cmd_tick(
    name: &str,
    execute: bool,
    live: bool,
    secrets: &FileSecretStore,
    config_path: &Path,
    state_path: PathBuf,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let position = config
        .find(name)
        .ok_or_else(|| anyhow!("no position '{name}' — add one with `drip strategy add`"))?;
    if execute && position.broker == "kis-domestic" {
        // Domestic live placement is gated (the daemon already skips it). drip keys the
        // at-most-once order key off the US-Eastern date, which rolls over mid-KRX-session
        // (~13:00 KST) → an intraday rerun could double-place; and domestic fill-reconcile is
        // unimplemented, so repeated ticks never advance `T` and would over-buy. Both are fixed by
        // #22 (a KST trading calendar + domestic reconcile). A preview (no `--execute`) is fine.
        return Err(anyhow!(
            "domestic (kis-domestic) live placement is gated until #22: the at-most-once order key \
             uses the US-Eastern date (rolls over mid-KRX-session → intraday double-place) and \
             domestic fill-reconcile is unimplemented (→ cross-day over-buy). Preview without \
             `--execute` works."
        ));
    }
    let live_broker = connect(
        &position.broker,
        secrets,
        Some(drip_infra::drip_home()?.as_path()),
    )?;
    let gateway = live_broker.as_order_gateway().ok_or_else(|| {
        anyhow!(
            "broker '{}' does not support order placement (KIS only in M2.1)",
            position.broker
        )
    })?;
    // The real-account safety gate lives in `place_orders` (drip-app) so every caller
    // inherits it; `--live` is the user's explicit consent to trade a real account.
    let registry = StrategyRegistry::with_builtins();
    let strategy = registry.build(&position.strategy, &position.strategy_params())?;
    let repo = SqliteStateRepository::open(state_path)?;
    // drip's trading date is the position's market session date, so the at-most-once order key
    // stays stable across a same-session rerun (issue #3 / #22) — a UTC date, or the US-Eastern
    // date for a KRX order, would flip mid-session and risk a double-place.
    let today = trading_date(
        position_market(&position.broker),
        time::OffsetDateTime::now_utc(),
    );
    let ports = TickPorts {
        quotes: live_broker.as_quotes(),
        gateway,
        account: live_broker.as_account(),
        repo: &repo,
        journal: &repo,
    };
    let view = place_orders(
        &ports,
        position.to_position()?,
        strategy.as_ref(),
        execute,
        live,
        today,
    )
    .await?;
    print_tick(name, &view);
    Ok(())
}

async fn cmd_reconcile(
    name: &str,
    secrets: &FileSecretStore,
    config_path: &Path,
    state_path: PathBuf,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let position = config
        .find(name)
        .ok_or_else(|| anyhow!("no position '{name}' — add one with `drip strategy add`"))?;
    let live = connect(
        &position.broker,
        secrets,
        Some(drip_infra::drip_home()?.as_path()),
    )?;
    let repo = SqliteStateRepository::open(state_path)?;
    // The position's market session date (issue #3 / #22): the reconcile boundary "completed
    // days < today" must use the exchange's date, matching the `ord_dt` the broker reports.
    let today = trading_date(
        position_market(&position.broker),
        time::OffsetDateTime::now_utc(),
    );
    let view = reconcile(live.as_account(), &repo, position.to_position()?, today).await?;
    print_reconcile(name, &view);
    Ok(())
}

/// `drip fills` — print the broker's reported executions for a position via `fills_since`. A
/// read-only diagnostic (never places, never writes state) for inspecting what reconcile sees.
async fn cmd_fills(
    name: &str,
    since: Option<String>,
    secrets: &FileSecretStore,
    config_path: &Path,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    let position = config
        .find(name)
        .ok_or_else(|| anyhow!("no position '{name}' — add one with `drip strategy add`"))?;
    let live = connect(
        &position.broker,
        secrets,
        Some(drip_infra::drip_home()?.as_path()),
    )?;
    let today = trading_date(
        position_market(&position.broker),
        time::OffsetDateTime::now_utc(),
    );
    // Default to the reconcile watermark, else the broker's inquiry window (~90 days back).
    let since = match since {
        Some(raw) => parse_date(&raw)?,
        None => position
            .to_position()?
            .reconciled_through
            .unwrap_or(today - time::Duration::days(90)),
    };
    let ticker = Ticker::new(&position.ticker);
    let fills = live.as_account().fills_since(&ticker, since).await?;
    println!(
        "Fills {name} ({ticker}) since {since}: {} fill(s)",
        fills.len()
    );
    for fill in &fills {
        println!(
            "  {} {:?} {} @ {}",
            fill.at,
            fill.side,
            fill.shares.get(),
            fill.price.value()
        );
    }
    Ok(())
}

/// One scheduled position the daemon drives: its broker, strategy, and seed template. The
/// broker and strategy are resolved once at startup so each fire just builds ports and places.
struct EngineJob {
    name: String,
    broker: LiveBroker,
    strategy: Box<dyn Strategy>,
    template: Position,
    /// The position's market, for the per-fire trading date (resolved once at startup).
    market: Market,
}

async fn cmd_run(
    execute: bool,
    live: bool,
    secrets: &FileSecretStore,
    config_path: &Path,
    state_path: PathBuf,
) -> Result<()> {
    let config = AppConfig::load(config_path)?;
    if config.positions.is_empty() {
        return Err(anyhow!(
            "no positions configured — add one with `drip strategy add`"
        ));
    }
    let repo = SqliteStateRepository::open(state_path)?;
    let registry = StrategyRegistry::with_builtins();

    // Resolve every position's broker + strategy up front, so a misconfiguration fails before
    // the daemon starts rather than at the first fire. `schedules[i]` pairs with `jobs[i]`.
    let mut jobs: Vec<EngineJob> = Vec::new();
    let mut schedules: Vec<Schedule> = Vec::new();
    for pc in &config.positions {
        if pc.broker == "kis-domestic" {
            // Domestic reconcile + the KST trading date work now (#22 P1/P2), but the daemon's
            // schedule still fires at a US-Eastern time on the NYSE calendar (schedule.rs); a KRX
            // position needs KST fire times + the KRX holiday calendar (#22 P4). Skip it here
            // until the market-aware schedule lands — manual `drip tick` (no schedule) works.
            tracing::warn!(
                "skipping position '{}': the `drip run` schedule is US-Eastern-only — a KRX \
                 position needs the KST schedule (#22 P4); place it with `drip tick` for now",
                pc.name
            );
            continue;
        }
        let broker = connect(
            &pc.broker,
            secrets,
            Some(drip_infra::drip_home()?.as_path()),
        )?;
        if broker.as_order_gateway().is_none() {
            // A read-only broker (e.g. Toss) can't place — skip it rather than blocking the
            // whole daemon, so it still runs every position that can trade.
            tracing::warn!(
                "skipping position '{}': broker '{}' cannot place orders (KIS only)",
                pc.name,
                pc.broker
            );
            continue;
        }
        let strategy = registry.build(&pc.strategy, &pc.strategy_params())?;
        let schedule = strategy
            .triggers()
            .into_iter()
            .map(|Trigger::Schedule(s)| s)
            .next()
            .unwrap_or_else(Schedule::daily_before_open);
        schedules.push(schedule);
        jobs.push(EngineJob {
            name: pc.name.clone(),
            broker,
            strategy,
            template: pc.to_position()?,
            market: position_market(&pc.broker),
        });
    }

    if jobs.is_empty() {
        return Err(anyhow!(
            "no configured position can place orders — `drip run` needs a KIS position"
        ));
    }

    tracing::info!(
        "drip run: scheduling {} position(s) (execute={execute}, live={live}). Ctrl-C to stop.",
        jobs.len()
    );

    engine::run(
        &schedules,
        async |i, now| {
            let job = &jobs[i];
            let gateway = job
                .broker
                .as_order_gateway()
                .expect("order placement was verified at startup");
            let ports = TickPorts {
                quotes: job.broker.as_quotes(),
                gateway,
                account: job.broker.as_account(),
                repo: &repo,
                journal: &repo,
            };
            let today = trading_date(job.market, now);
            let view = place_orders(
                &ports,
                job.template.clone(),
                job.strategy.as_ref(),
                execute,
                live,
                today,
            )
            .await?;
            print_tick(&job.name, &view);
            Ok(())
        },
        time::OffsetDateTime::now_utc,
        engine::shutdown_signal(),
    )
    .await;
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

fn print_tick(name: &str, view: &TickView) {
    println!(
        "Tick {name}: {} @ {} (T={})",
        view.ticker, view.price, view.t
    );
    if view.reconciled_fills > 0 {
        println!(
            "  reconciled {} fill(s) before deciding",
            view.reconciled_fills
        );
    }
    if let Some(note) = &view.note {
        println!("NOTE: {note}");
    }
    if view.orders.is_empty() {
        println!("  (no orders today)");
    }
    for order in &view.orders {
        let limit = order
            .intent
            .limit
            .map(|p| p.to_string())
            .unwrap_or_else(|| "MKT".to_string());
        let status = match &order.status {
            TickStatus::Placed => "PLACED",
            TickStatus::SkippedIdempotent => "skip(dup)",
            TickStatus::WouldPlace => "would-place",
            TickStatus::Failed => "FAILED",
        };
        print!(
            "  [{status}] {:?} {} {:?} @ {} [{}]",
            order.intent.side, order.intent.shares, order.intent.kind, limit, order.intent.tag
        );
        if let Some(id) = &order.order_id {
            print!(" id={id}");
        }
        if let Some(err) = &order.error {
            print!(" error={err}");
        }
        println!();
    }
    if !view.executed {
        println!(
            "NOTE: preview only — pass --execute to place (KIS 모의 needs --execute; real needs --execute --live)."
        );
    }
}

fn print_reconcile(name: &str, view: &ReconcileView) {
    println!(
        "Reconcile {name}: {} — applied {} fill(s), {} cycle(s); T {} -> {}",
        view.ticker, view.applied_fills, view.cycles_completed, view.t_before, view.t_after
    );
    if let Some(through) = view.through {
        println!("  reconciled through {through}");
    }
    if let Some(note) = &view.note {
        println!("NOTE: {note}");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_market_maps_domestic_brokers_to_korea() {
        // kis-domestic places on KRX, so its trading date must be KST (the #22 fix); the
        // overseas KIS adapter stays US Eastern. Toss is a Korean broker too.
        assert_eq!(position_market("kis-domestic"), Market::KrEquity);
        assert_eq!(position_market("toss"), Market::KrEquity);
        assert_eq!(position_market("kis"), Market::UsEquity);
        assert_eq!(position_market("paper"), Market::UsEquity);
    }
}
