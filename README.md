# drip

A seamless, extensible **automated trading CLI** for Korean brokerages, written in Rust.
Ships as a single binary, runs strategies against 한국투자증권 (KIS) and 토스증권 (Toss), and
is designed to be driven by your own Claude Code.

The flagship strategy is **라오어의 무한매수법 (Laoer's "Infinite Buying", v2.2)** for US
leveraged ETFs (TQQQ, SOXL, …), and adding your own strategy is a first-class concern.

> ⚠️ **Disclaimer.** Leveraged ETFs are high-risk and this software can place **real orders
> with real money** (M2.1: `drip tick --execute` on KIS). Nothing here is financial advice.
> Always backtest, dry-run, and test on a 모의(paper) account first. Placement is dry-run by
> default; a real account additionally requires an explicit `--live`.

## Features

- **Single static binary** — `cargo install` or a prebuilt release; no runtime, no Docker.
- **Pluggable strategies** — a strategy is a pure function `(state, market) → orders`.
  Built-in 무한매수 v2.2; user strategies (sandboxed Rhai scripts) are the next milestone.
- **Multi-broker** — capability-based adapters: KIS overseas (REST, paper trading, US, **live
  order placement**), KIS domestic (KRX, **모의 order placement**), Toss (REST, read-only), and a
  Paper simulator.
- **Backtesting** — replay daily bars (CSV) with the same fill rules as paper trading;
  reports equity curve, CAGR, and max drawdown.
- **Web dashboard** — `drip web` serves a read-only dashboard (positions, quotes, backtests)
  from the same single binary; no Node, no build step.
- **Safe order placement** — money is exact decimal (never `f64`); going live is dry-run by
  default, gated by `--live` for real accounts, risk-vetted per order, and idempotent
  (at-most-once — never double-buys); secrets live in a `0600` file, never logged.
- **Self-reconciling ledger** — `drip reconcile` (and every `drip tick`) folds settled KIS
  fills into the position, so the cost-averaging tranche counter advances day to day on its
  own and completed cycles are banked; idempotent (never double-counts a fill).
- **Scheduler daemon** — `drip run` fires every configured position on its schedule, on US
  trading days only (NYSE holidays observed, DST-correct); per-position errors are isolated, a
  missed slot is caught up idempotently within the trading window, and SIGINT/SIGTERM stops it
  cleanly between fires.

## Tech stack

Rust 2024 · `tokio` · `reqwest` (rustls, no OpenSSL) · `rust_decimal` · `rusqlite` (bundled
sqlite) · `clap` · `serde` · `tracing` · `axum` (dashboard). A seven-crate hexagonal
workspace — see [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Getting started

```bash
# Build
cargo build --release           # binary at target/release/drip

# Initialize ~/.drip (config + secrets paths)
drip init

# Configure a position (paper broker needs no credentials)
drip strategy add --name tqqq --account paper --broker paper --ticker TQQQ --seed 4000 --splits 40

# Backtest over a CSV of daily bars (date,open,high,low,close)
drip backtest --name tqqq --data examples/tqqq-sample.csv

# Show persisted positions
drip status
```

### Connecting a live broker

drip isolates each brokerage **account** — its credentials, its environment, and its ledger.
For KIS, 모의 (`paper`) and 실전 (`real`) are **separate accounts** (e.g. `kis-paper`,
`kis-real`): switching between them never reinterprets a position or crosses ledgers. KIS
supports **live order placement** — US overseas (M2.1) and 모의 KRX via `--broker kis-domestic`
(#22, a real domestic account is refused). Toss is read-only.

```bash
# Register a KIS account (creds → ~/.drip/secrets.toml 0600, validated with a probe quote).
drip account add --name kis-paper --env paper \
  --app-key KEY --app-secret SECRET --cano 12345678 --product-code 01 --exchange nasdaq

# Attach a position to that account.
drip strategy add --name tqqq --account kis-paper --broker kis --ticker TQQQ --seed 4000 --splits 40

drip account show --name kis-paper --broker kis     # holdings + balance (read-only)
drip quote AAPL --account kis-paper --broker kis     # current quote
drip dry-run --name tqqq                             # today's intended orders — NOT placed

# Place today's infinite-buying orders. Dry-run by default; --execute actually sends.
drip tick --name tqqq                # preview what would be placed (nothing sent)
drip tick --name tqqq --execute      # place on a 모의(paper) account
# A real account is a separate registration (e.g. `--name kis-real --env real`); a real order
# additionally requires --live:  drip tick --name <real-position> --execute --live

# After the close, fold settled fills into the ledger so T advances (tick does this too).
drip reconcile --name tqqq

# Run the scheduler daemon: place every configured position on its daily schedule, on US
# trading days only. Preview by default; --execute places, --execute --live for a real account.
drip run                             # Ctrl-C (or SIGTERM) stops it cleanly between fires.

# Toss (read-only)
drip account toss --name toss --app-key KEY --app-secret SECRET --account-seq 7
drip account show --name toss --broker toss
```

> Upgrading from a pre-account drip? The first command migrates `~/.drip` automatically —
> credentials move under their account, the ledger is re-keyed (state.db is backed up first),
> and your existing positions are assigned a `kis-<env>` account from the old global setting.

## Commands

| Command | Purpose |
|---|---|
| `drip init` | Create `~/.drip` and an empty config. |
| `drip account add --name --env …` | Register a KIS account (credentials + 모의/실전), validated with a probe quote. |
| `drip account toss --name …` | Register a 토스증권 (Toss) account. |
| `drip account show --name --broker` | Show an account's holdings and balance (read-only). |
| `drip strategy add --name --account --broker` | Add/update a position, attached to an account. |
| `drip backtest` | Backtest a position over a CSV. |
| `drip quote <t> --account --broker` | Fetch a current quote (read-only). |
| `drip dry-run --name` | Compute today's orders from a live quote (placed: none). |
| `drip tick --name [--execute] [--live]` | Compute and (with `--execute`) place today's orders on KIS. Dry-run by default; `--live` confirms a real account. |
| `drip reconcile --name` | Fold settled KIS fills into the position's ledger (advances `T`). Read-only at the broker. |
| `drip fills --name [--since]` | Print broker-reported executions (date, side, qty, price) for a position. Read-only; touches no ledger. |
| `drip run [--execute] [--live]` | Scheduler daemon: fire every configured position on its daily schedule (US trading days), through the same guarded path as `tick`. Dry-run by default. |
| `drip status` | Show persisted positions (account, environment `[REAL]`/`[paper]`, `T`, holdings). |
| `drip web` | Serve the read-only web dashboard (axum). |

## Environment variables

| Var | Default | Purpose |
|---|---|---|
| `DRIP_LOG` | `info` | Log verbosity (`error`/`warn`/`info`/`debug`/`trace`). |
| `HOME` | — | `~/.drip` is resolved from the home directory. |

## License

MIT.
