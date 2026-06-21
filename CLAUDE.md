# CLAUDE.md — drip

Automated trading CLI for Korean brokerages (KIS, Toss). Rust 2024, hexagonal six-crate
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
- Broker ports are segregated: `Quotes` / `AccountQuery` / `OrderGateway`. **Live brokers
  (KIS/Toss) must NOT implement `OrderGateway`** — read-only is a type-level guarantee in M1.

## Conventions

- **Money is `Decimal`, never `f64`.** Use `Money`/`Price`/`Percent`/`Shares` value objects.
  `f64` is allowed only for report statistics (CAGR/MDD).
- One fill rule: `drip_domain::settle`. Don't reimplement fill logic anywhere else.
- `Position` = drip's strategy ledger (seed/splits/T/cycle). `Holding` = broker-reported
  shares/avg. Don't conflate them.
- New strategy → add an adapter in `drip-strategies` and register it in `StrategyRegistry`.
  Nothing downstream changes (OCP).
- New broker → implement the capability ports it supports; declare them in `capabilities()`.
- Errors map to `DomainError` at adapter boundaries. The CLI uses `anyhow` at the top.
- Secrets: `FileSecretStore` (`~/.drip/secrets.toml`, `0600`). Never log secret values.
  Secret keys use underscores (`kis_app_key`), never dots (dots are TOML nesting).

## Directory map

```
crates/drip-domain      # value objects, entities, ports, settle()
crates/drip-strategies  # InfiniteBuying v2.2 + registry
crates/drip-brokers     # KisBroker, TossBroker, PaperBroker (+ shared http.rs)
crates/drip-app         # use cases (backtest, account, quote, dry-run) shared by cli+web
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

## M1 scope (do not silently exceed)

In scope: domain, 무한매수 v2.2, Paper broker, Backtest, **read-only** KIS/Toss, CLI.
Out of scope (M2): live order placement, US open/close scheduler, WebSocket quotes, Rhai
user strategies, OS-keychain secrets, broker rate-limiting, notifications. If a change would
add live order placement, treat it as a production-safety change and surface it explicitly.

## Definition of done

`cargo test` green · `clippy -D warnings` clean · `cargo fmt --check` clean · no `f64` for
money · live brokers still have no `OrderGateway` · docs updated when conventions change.
