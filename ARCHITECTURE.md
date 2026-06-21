# Architecture

drip is a **hexagonal (ports & adapters)** Rust workspace. The domain defines abstract
ports; outer crates implement them. Dependencies always point inward, enforced physically
by crate boundaries.

## System overview

```mermaid
flowchart TD
    CLI["drip-cli (clap + composition root)"]
    WEB["drip-web (axum dashboard)"]
    APP["drip-app (use cases)"]
    STRAT["drip-strategies (InfiniteBuying v2.2, registry)"]
    BROK["drip-brokers (Kis, Toss, Paper)"]
    INFRA["drip-infra (config, secrets, sqlite, csv, logging)"]
    DOM["drip-domain (entities, value objects, PORTS)"]

    CLI --> APP
    WEB --> APP
    CLI --> BROK
    WEB --> BROK
    CLI --> INFRA
    WEB --> INFRA
    APP --> DOM
    STRAT --> DOM
    BROK --> DOM
    INFRA --> DOM
```

Every arrow points toward `drip-domain`. The domain depends on nothing in the workspace,
so it has no idea HTTP, sqlite, or clap exist. The CLI and the web dashboard are the
driving adapters (composition roots) — the only crates that know every concrete adapter.

## Crates

| Crate | Responsibility | Key types |
|---|---|---|
| `drip-domain` | Pure model + ports. No I/O, no runtime. | `Money`, `Price`, `Shares`, `Position`, `Holding`, `OrderIntent`, `settle()`, and the port traits. |
| `drip-strategies` | Built-in strategies + registry (OCP seam). | `InfiniteBuying`, `StrategyRegistry`. |
| `drip-brokers` | Broker adapters. | `KisBroker`, `TossBroker`, `PaperBroker`. |
| `drip-app` | Use cases orchestrating ports (shared by CLI + web). | `Backtest`, `BacktestReport`, `account_snapshot`, `dry_run`, `run_backtest`. |
| `drip-infra` | Filesystem/sqlite/logging adapters. | `AppConfig`, `FileSecretStore`, `SqliteStateRepository`, `CsvMarketData`. |
| `drip-cli` | CLI + composition root (binary `drip`). | `main`, command handlers. |
| `drip-web` | Read-only axum dashboard (`drip web`); a driving adapter over the use cases. | `serve`, HTTP handlers. |

## Ports (domain abstractions)

- **`Strategy`** — `decide(&DailyContext) -> Vec<OrderIntent>`. Pure and deterministic; the
  primary extension point.
- **Broker ports, segregated (ISP):** `BrokerInfo` (id + capabilities), `Quotes`,
  `AccountQuery`, `OrderGateway`. An adapter implements only what it supports.
- **`MarketDataSource`**, **`StateRepository`**, **`SecretStore`** — infra ports.

## Key design decisions (ADR-style)

### ADR-1: Money is an exact `Decimal`, never `f64`
**Context:** Average-price and fill math compounds across an "infinite buying" cycle.
**Decision:** `Money`/`Price`/`Percent` wrap `rust_decimal::Decimal`; there is no `f64`
constructor. **Rationale:** float rounding errors are unacceptable on real orders. Backtest
*statistics* (CAGR/MDD) use `f64` — those are ratios, not money.

### ADR-2: Read-only live integration is enforced by the type system
**Context:** M1 must talk to live brokers but must not place live orders.
**Decision:** Split the broker surface into `Quotes` / `AccountQuery` / `OrderGateway`
(Interface Segregation). KIS and Toss implement the first two; **only `PaperBroker`
implements `OrderGateway`.** **Rationale:** there is literally no code path to place a live
order — the compiler guarantees it, not a runtime flag.

### ADR-3: Capability-based broker abstraction with graceful degradation
**Context:** Brokers differ — KIS has WebSocket + paper trading; Toss (today) has neither.
**Decision:** `Broker::capabilities()` reports `realtime_quotes`, `paper_account`,
`order_placement`, `overseas`. The engine reads them and degrades (poll instead of stream).
**Rationale:** adding/removing a capability is a new trait impl, never a core change (OCP).

### ADR-4: One settlement rule, shared by backtest and paper trading
**Context:** A backtest and the paper broker must agree exactly on what fills.
**Decision:** `drip_domain::settle(intent, bar) -> Option<Fill>` is the single source of
truth, used by both `Backtest` and `PaperBroker`. **Rationale:** DRY; no drift between
"what the backtest said" and "what paper trading does".

### ADR-5: `Position` (strategy ledger) ≠ `Holding` (broker truth)
**Context:** A broker reports shares + average price; it does not know our seed/splits/cycle.
**Decision:** `AccountQuery` returns `Holding` (ticker, shares, avg); `Position` (seed,
splits, T, cycle) is drip's own state in `StateRepository`. **Rationale:** SRP; the engine
reconciles broker holdings against its local ledger.

### ADR-6: Single binary over runtime dependencies
**Decision:** `reqwest` with `rustls-tls` (no OpenSSL) and `rusqlite` `bundled` (sqlite
compiled in). **Rationale:** `curl | sh`-style install with zero system dependencies.

## Data flow: a backtest

1. CLI loads the position config and builds the strategy via `StrategyRegistry`.
2. `CsvMarketData` reads daily bars.
3. `Backtest::run` iterates bars: `strategy.decide` → `settle` each intent against the bar →
   `Position::apply_fill` → detect cycle completion → mark to market.
4. Returns a `BacktestReport` (equity curve, CAGR, MDD, cycles).

## Milestone boundaries

- **M1 (this milestone):** domain + 무한매수 v2.2 + Paper + Backtest + read-only KIS/Toss +
  CLI + a read-only web dashboard. No live orders, no WebSocket, no Rhai, no scheduler.
- **M2+:** live `OrderGateway` for KIS/Toss, the scheduler (US open/close), Rhai user
  strategies, WebSocket quotes, OS-keychain secrets, rate-limiting, notifications. See the
  [M2 engine sketch](./docs/M2-engine-sketch.md) for the unified always-on/scheduled design.
