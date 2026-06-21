# CLAUDE.md — drip

Automated trading CLI for Korean brokerages (KIS, Toss). Rust 2024, hexagonal seven-crate
workspace. Flagship strategy: 라오어 무한매수법 v2.2. Binary name: `drip`.

## Build / test / lint

```bash
cargo build --workspace
cargo test --workspace                                  # all tests are offline + deterministic
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
cargo fmt                                                # format before commit
```

Add dependencies with `cargo add` (never hand-write versions). Broker adapter tests use
`wiremock`; never call the real KIS/Toss APIs from tests.

## Architecture (see ARCHITECTURE.md)

Dependency rule: everything points inward to `drip-domain`. Order of crates:
`domain → {strategies, brokers, app, infra} → cli`. The CLI is the composition root.

- Ports live in `drip-domain/src/ports.rs`. Adapters implement them in outer crates.
- Broker ports are segregated: `Quotes` / `AccountQuery` / `OrderGateway`. In M1 no live
  broker implemented `OrderGateway` (read-only by type). **M2.1: `KisBroker` implements it
  (live order placement); `TossBroker` must NOT (no 모의 sandbox → stays read-only).** Going
  live is now guarded at *runtime*, not by the type system — see placement safety below.

## Conventions

- **Money is `Decimal`, never `f64`.** Use `Money`/`Price`/`Percent`/`Shares` value objects.
  `f64` is allowed only for report statistics (CAGR/MDD).
- One fill rule: `drip_domain::settle`. Don't reimplement fill logic anywhere else.
- `Position` = drip's strategy ledger (seed/splits/T/cycle). `Holding` = broker-reported
  shares/avg. Don't conflate them.
- New strategy → add an adapter in `drip-strategies` and register it in `StrategyRegistry`.
  Nothing downstream changes (OCP).
- New broker → implement the capability ports it supports; declare them in `capabilities()`.
- **Live order placement (M2.1) is runtime-guarded** (it replaced M1's type-level block). Every
  order goes through `drip_app::place_orders`, which: (1) refuses a real (non-`paper_account`)
  account unless `allow_real`/`--live`; (2) runs `drip_domain::risk::vet` on every intent and
  aborts the whole tick on any violation; (3) reserves an `OrderJournal` client key *before*
  sending (at-most-once — never double-buys); (4) is dry-run by default (`drip tick` previews,
  `--execute` places). Don't add a placement path that bypasses it.
- **Fill reconciliation (M2.2)** advances the ledger from broker executions. `Position::reconcile`
  applies only fills on completed days not yet reconciled (`reconciled_through < date < today`) —
  idempotent. `place_orders` reconciles before deciding (in-memory for a preview, persisted on
  `--execute`); never decide on a stale `T`. A fill must never be silently dropped (under-count
  → over-buy): every drop path is an explicit error.
  `fills_since` returns fills in chronological order (`apply_day` needs it).
- Errors map to `DomainError` at adapter boundaries. The CLI uses `anyhow` at the top.
- Secrets: `FileSecretStore` (`~/.drip/secrets.toml`, `0600`). Never log secret values.
  Secret keys use underscores (`kis_app_key`), never dots (dots are TOML nesting).

## Directory map

```
crates/drip-domain      # value objects, entities, ports, settle()
crates/drip-strategies  # InfiniteBuying v2.2 + registry
crates/drip-brokers     # KisBroker, TossBroker, PaperBroker (+ shared http.rs)
crates/drip-app         # use cases (backtest, account, quote, dry-run, tick) shared by cli+web
crates/drip-infra       # config, secrets, sqlite state, csv data, logging
crates/drip-cli         # clap commands + composition root (binary `drip`)
crates/drip-web         # read-only axum dashboard (drip web)
examples/               # sample CSV for backtests
docs/                   # M2 engine design sketch
```

## Pinned-version gotchas

- `rusqlite = "0.32"` (bundled). Newer pulls a `libsqlite3-sys` that needs unstable
  `cfg_select` — do not bump without checking it compiles on the toolchain.
- `reqwest = "0.12"` with `default-features = false, features = ["json", "rustls-tls"]`.
  0.13 renamed the TLS feature; rustls keeps us off OpenSSL (single binary).

## Scope (do not silently exceed)

- **M1 (done):** domain, 무한매수 v2.2, Paper broker, Backtest, read-only KIS/Toss, CLI, web.
- **M2.1 (done):** live **KIS** order placement via `drip tick` — `OrderGateway`, pre-trade
  `risk::vet`, at-most-once `OrderJournal`, dry-run default + `--live` real-account gate.
- **M2.2 (done):** fill reconciliation — `drip reconcile` + KIS `inquire-ccnl`
  (`AccountQuery::fills_since(ticker, since)`) fold executions into the ledger so `T`
  auto-advances and cycles bank; `drip tick` reconciles before deciding. Idempotent per
  completed-day watermark (`Position.reconciled_through`).
- **Still out of scope (M2+):** US-session scheduler / `drip run` daemon, WebSocket quotes,
  Rhai strategies, OS-keychain secrets, rate-limiting, notifications, Toss order placement (no
  모의 sandbox). Idempotency and the reconcile boundary use the **UTC** date, so run
  `tick`/`reconcile` once per day during US hours (proper ET trading-calendar lands with a
  future `MarketCalendar`). Any further live-trading change is a production-safety change —
  surface it.

## Definition of done

`cargo test` green · `clippy -D warnings` clean · `cargo fmt --check` clean · no `f64` for
money · only `KisBroker` implements `OrderGateway` (Toss stays read-only) · every placement
path stays behind `place_orders`' guards · docs updated when conventions change.
