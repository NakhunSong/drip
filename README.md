# drip

A seamless, extensible **automated trading CLI** for Korean brokerages, written in Rust.
Ships as a single binary, runs strategies against 한국투자증권 (KIS) and 토스증권 (Toss), and
is designed to be driven by your own Claude Code.

The flagship strategy is **라오어의 무한매수법 (Laoer's "Infinite Buying", v2.2)** for US
leveraged ETFs (TQQQ, SOXL, …), and adding your own strategy is a first-class concern.

> ⚠️ **Disclaimer.** Leveraged ETFs are high-risk and this software places (or will place)
> real orders with real money. Nothing here is financial advice. Always dry-run and paper
> trade first. Milestone 1 is **read-only** for live brokers — it cannot place live orders.

## Features

- **Single static binary** — `cargo install` or a prebuilt release; no runtime, no Docker.
- **Pluggable strategies** — a strategy is a pure function `(state, market) → orders`.
  Built-in 무한매수 v2.2; user strategies (sandboxed Rhai scripts) are the next milestone.
- **Multi-broker** — capability-based adapters: KIS (REST + paper trading + US overseas),
  Toss (REST), and a Paper simulator. Live brokers are **read-only by type** in M1.
- **Backtesting** — replay daily bars (CSV) with the same fill rules as paper trading;
  reports equity curve, CAGR, and max drawdown.
- **Web dashboard** — `drip web` serves a read-only dashboard (positions, quotes, backtests)
  from the same single binary; no Node, no build step.
- **Safe by construction** — money is exact decimal (never `f64`); read-only live brokers
  cannot place orders (no `OrderGateway` impl); secrets live in a `0600` file, never logged.

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

### Connecting a live broker (read-only in M1)

```bash
# KIS — store credentials (validated with a probe quote) in ~/.drip/secrets.toml (0600)
drip keys kis --app-key KEY --app-secret SECRET \
  --cano 12345678 --product-code 01 --env paper --exchange nasdaq

drip account --broker kis           # holdings + balance (read-only)
drip quote AAPL --broker kis         # current quote
drip dry-run --name tqqq             # today's intended orders — NOT placed

# Toss
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
| `drip status` | Show persisted positions. |
| `drip web` | Serve the read-only web dashboard (axum). |

## Environment variables

| Var | Default | Purpose |
|---|---|---|
| `DRIP_LOG` | `info` | Log verbosity (`error`/`warn`/`info`/`debug`/`trace`). |
| `HOME` | — | `~/.drip` is resolved from the home directory. |

## License

MIT.
