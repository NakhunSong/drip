# Changelog

All notable changes to **drip** are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). drip has not cut a tagged release
yet, so changes accumulate under **[Unreleased]**.

_Baseline — M1 (read-only walking skeleton): domain model, 라오어 무한매수법 v2.2, paper broker,
backtesting, read-only KIS/Toss adapters, the CLI, and a read-only web dashboard._

## [Unreleased]

### Added

- **`drip tick` — live order placement on 한국투자증권 (KIS).** Computes a position's
  infinite-buying orders for today and, with `--execute`, places them. Preview (dry-run) by
  default — nothing is sent without `--execute`.
- **Real-account safeguard.** Placing against a KIS *real* (non-모의) account additionally
  requires an explicit `--live`; a 모의(paper) account needs only `--execute`.
- **Pre-trade risk checks.** Every order is vetted before it is sent — a sell may not exceed
  the holding, a buy may not spend many tranche budgets at once, and a limit price must stay
  within a 10× band of the position's average. A single failed check cancels the whole tick.
- **At-most-once placement.** Placed orders are journaled (sqlite), so re-running `drip tick`
  on the same day never double-places — it would rather skip an order than double-buy.
- **`drip reconcile` — advance the ledger from broker fills.** Pulls 한국투자증권 (KIS) overseas
  execution history (`inquire-ccnl`) and folds settled fills into a position, so its tranche
  counter `T` auto-advances day to day and completed cost-averaging cycles are banked. Read-only
  at the broker (it places no orders), idempotent (only fills on completed days not yet
  reconciled are applied — a re-run never double-counts).

### Changed

- **KIS is no longer read-only** — it can place US overseas orders (LOC / limit). Going live is
  guarded at runtime (capability + dry-run default + `--live` + risk check + idempotency)
  instead of being blocked by the type system. **토스증권 (Toss) stays read-only** (it has no
  모의 sandbox).
- KIS order prices are rounded to the US $0.01 tick before sending.
- **`drip tick` now reconciles settled fills before deciding**, so today's tranche is computed
  from an up-to-date ledger rather than a stale one. A preview reconciles in-memory (no state
  write — like `dry-run`); `--execute` and the standalone `drip reconcile` persist the advance.

> **Limitations.** Idempotency and the reconcile boundary use the **UTC** date, so run
> `drip tick` / `drip reconcile` once per trading day during US market hours (a proper ET
> trading calendar arrives with `MarketCalendar`). Reconciliation is per **completed day**
> (a day's fills are applied once that day is past), which is exact for drip's day orders
> (LOC / day-limit) but not intended for intraday partial-fill precision. Toss order placement
> is not supported.
