# M2 sketch — the execution engine (always-on + scheduled, unified)

> Design blueprint for M2. Nothing here is built yet; it is pinned so we implement it
> consistently. The principle: **separate the strategy (the decision) from the engine (the
> driving loop and its cadence).** One engine hosts both daily-batch strategies (무한매수법)
> and continuously-running strategies (trailing stops, intraday momentum) — "always-on"
> becomes a runtime/deployment concern, not a per-strategy fork.

## 1. The missing piece

The `Strategy` port is already cadence-agnostic:

```rust
fn decide(&self, ctx: &DailyContext) -> Vec<OrderIntent>;
```

It does not know *when* it is called. M2 adds a declaration of *when to wake it*, and an
engine that wires the matching event source.

## 2. Trigger vocabulary (domain)

```rust
/// When a strategy wants `decide` called.
pub enum Trigger {
    /// At a wall-clock schedule, e.g. 30 min before the US open (무한매수법).
    Schedule(Schedule),
    /// On every bar close of a timeframe (e.g. MA-cross on 5m / 1d).
    OnBarClose(Timeframe),
    /// On every quote tick (intraday momentum) — requires realtime quotes.
    OnTick,
    /// When price crosses a level in a direction (trailing stop / stop-loss).
    OnPriceCross { level: Price, direction: CrossDirection },
}

pub enum Timeframe { M1, M5, M15, H1, D1 }
pub enum CrossDirection { Above, Below }
// `Schedule` wraps a cron-like spec resolved against a market calendar + timezone.
```

The `Strategy` trait gains one method (default keeps existing strategies working):

```rust
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    fn decide(&self, ctx: &DailyContext) -> Vec<OrderIntent>;
    /// Default: a single daily schedule. Streaming strategies override this.
    fn triggers(&self) -> Vec<Trigger> { vec![Trigger::Schedule(Schedule::daily_before_open())] }
}
```

`InfiniteBuying` overrides to declare `Schedule(before US open)`; a trailing-stop strategy
declares `OnPriceCross{..}`; a momentum strategy declares `OnTick`.

## 3. Event sources (ports the engine drives)

```rust
#[async_trait] pub trait BarStream  { async fn next_bar(&mut self) -> Option<Bar>; }
#[async_trait] pub trait TickStream { async fn next_tick(&mut self) -> Option<Quote>; }   // broker WS
pub trait MarketCalendar { fn next_fire(&self, schedule: &Schedule) -> Instant; }
```

- `Schedule` → driven by `MarketCalendar` + a tokio timer.
- `OnBarClose` → driven by aggregating ticks into bars, or a broker bar feed.
- `OnTick` / `OnPriceCross` → driven by `TickStream` (broker WebSocket).

## 4. The engine loop (`drip-engine`, a new driving adapter)

```text
for each configured position:
    strat = registry.build(...)
    for trigger in strat.triggers():
        subscribe the matching event source       // capability-checked (see §5)

on each fired trigger (per position, serialized via a per-position task):
    state  = state_repo.load(broker, ticker)       // resume from disk
    ctx    = DailyContext { position: &state, market }
    intents = strat.decide(&ctx)
    for intent in intents.filter(risk_guard):
        id = order_gateway.place(ticker, intent)    // ← M2: KIS/Toss implement OrderGateway
    fills = order_gateway.fills_since(last_run)
    state.apply_fill(each); detect cycle; state_repo.save(state)
```

Same `decide`; only the wake-up differs. The engine is the only always-on component, and it
hosts every cadence uniformly.

## 5. Capability gating (reuses the existing pattern)

A strategy's triggers must be satisfiable by its broker:

```rust
if strat.triggers().iter().any(|t| matches!(t, Trigger::OnTick | Trigger::OnPriceCross{..}))
   && !broker.capabilities().realtime_quotes {
    // KIS: has WebSocket → OK.  Toss (today): none → reject, or degrade to polling.
}
```

So *which strategies can run on which broker* is enforced by `Capabilities`, exactly like
M1's read-only guarantee.

## 6. Cross-event state for streaming strategies

Pure `decide` per event is enough **if** the strategy can carry rolling state between events
(e.g. the current trailing-stop level, a rolling RSI). Add a strategy-owned scratchpad to
`Position` so even a sandboxed Rhai script stays pure (no `&mut self`):

```rust
pub struct Position {
    // … existing fields …
    pub scratch: serde_json::Value,   // strategy-private state, persisted across events
}
```

Only if a strategy genuinely needs a rich in-memory lifecycle do we add an *optional* second
port:

```rust
#[async_trait] pub trait StreamingStrategy {
    fn on_start(&mut self, ctx: &DailyContext);
    fn on_tick(&mut self, ctx: &DailyContext) -> Vec<OrderIntent>;
    fn on_fill(&mut self, fill: &Fill);
}
```

Most reactive strategies (stops, intraday signals) fit `decide` + `scratch`; keep that the
default.

## 7. Deployment: two runtimes, one core

| Runtime | For | Always-on? |
|---|---|---|
| **`drip tick`** (one-shot) | Schedule-only strategies (무한매수법). cron/launchd fires it; sqlite carries state between runs. | No — only at fire time |
| **`drip run`** (daemon) | Streaming / bar / price-cross strategies (and may also host scheduled ones). | Yes |

Both call the same use cases and strategy ports — `drip tick`, `drip run`, the CLI, and the
web dashboard are all **driving adapters over the same hexagonal core** (domain untouched).

**Crash-safety:** on startup the engine reconciles its persisted positions against the
broker's actual holdings/fills before acting, and order placement is idempotent (a stable
client order key per intent) so a restart never double-buys.

## 8. Honest limits

This supports **reactive / intraday** strategies (stops, signals, bar reactions). It is
**not** microsecond HFT — retail Korean-broker rate limits (KIS ~20 req/s) and scripting
speed cap it there, which is fine for the target audience.

## 9. M2 implementation order

1. Implement `OrderGateway` for KIS/Toss (going live = one deliberate, reviewed trait impl).
2. `drip tick` (one-shot) + `MarketCalendar` + risk guard + idempotent placement → run
   무한매수법 on a paper/모의 account end-to-end.
3. Add `Trigger` + `Strategy::triggers()` + the `drip-engine` daemon with the scheduler.
4. `TickStream` over the KIS WebSocket → enable `OnTick` / `OnPriceCross` strategies.
5. `scratch` state for rolling strategy state; Rhai user strategies on top.
6. Dashboard: live updates via SSE/WebSocket fed by the engine.
