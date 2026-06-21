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
- **Multi-broker** — capability-based adapters: KIS (REST, paper trading, US overseas, **live
  order placement**), Toss (REST, read-only), and a Paper simulator.
- **Backtesting** — replay daily bars (CSV) with the same fill rules as paper trading;
  reports equity curve, CAGR, and max drawdown.
- **Web dashboard** — `drip web` serves a read-only dashboard (positions, quotes, backtests)
  from the same single binary; no Node, no build step.
- **Safe order placement** — money is exact decimal (never `f64`); going live is dry-run by
  default, gated by `--live` for real accounts, risk-vetted per order, and idempotent
  (at-most-once — never double-buys); secrets live in a `0600` file, never logged.

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
drip strategy add --name tqqq --broker paper --ticker TQQQ --seed 4000 --splits 40

# Backtest over a CSV of daily bars (date,open,high,low,close)
drip backtest --name tqqq --data examples/tqqq-sample.csv

# Show persisted positions
drip status
```

### Connecting a live broker

KIS supports **live order placement** (M2.1); Toss is read-only.

```bash
# KIS — store credentials (validated with a probe quote) in ~/.drip/secrets.toml (0600)
drip keys kis --app-key KEY --app-secret SECRET \
  --cano 12345678 --product-code 01 --env paper --exchange nasdaq

drip account --broker kis           # holdings + balance (read-only)
drip quote AAPL --broker kis         # current quote
drip dry-run --name tqqq             # today's intended orders — NOT placed

# Place today's infinite-buying orders. Dry-run by default; --execute actually sends.
drip tick --name tqqq                # preview what would be placed (nothing sent)
drip tick --name tqqq --execute      # place on a 모의(paper) account
# A KIS *real* account additionally requires --live:
#   drip tick --name tqqq --execute --live

# Toss (read-only)
drip keys toss --app-key KEY --app-secret SECRET --account-seq 7
drip account --broker toss
```

## Commands

| Command | Purpose |
|---|---|
| `drip init` | Create `~/.drip` and an empty config. |
| `drip strategy add` | Add/update a configured position. |
| `drip backtest` | Backtest a position over a CSV. |
| `drip keys kis\|toss` | Store + validate broker credentials. |
| `drip account --broker` | Show holdings and balance (read-only). |
| `drip quote <t> --broker` | Fetch a current quote (read-only). |
| `drip dry-run --name` | Compute today's orders from a live quote (placed: none). |
| `drip tick --name [--execute] [--live]` | Compute and (with `--execute`) place today's orders on KIS. Dry-run by default; `--live` confirms a real account. |
| `drip status` | Show persisted positions. |
| `drip web` | Serve the read-only web dashboard (axum). |

## Environment variables

| Var | Default | Purpose |
|---|---|---|
| `DRIP_LOG` | `info` | Log verbosity (`error`/`warn`/`info`/`debug`/`trace`). |
| `HOME` | — | `~/.drip` is resolved from the home directory. |

## License

MIT.
