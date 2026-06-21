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

### Changed

- **KIS is no longer read-only** — it can place US overseas orders (LOC / limit). Going live is
  guarded at runtime (capability + dry-run default + `--live` + risk check + idempotency)
  instead of being blocked by the type system. **토스증권 (Toss) stays read-only** (it has no
  모의 sandbox).
- KIS order prices are rounded to the US $0.01 tick before sending.

> **Limitations (planned for M2.2).** `drip tick` places today's orders only — it does not yet
> reconcile fills, so a position's tranche counter does not auto-advance between days. Run
> `drip tick` once per trading day during US market hours (idempotency keys use the UTC date).
> Toss order placement is not supported.
