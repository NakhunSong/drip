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
- **`drip run` — the scheduler daemon.** Runs every configured position on a daily schedule
  (US trading days only, with NYSE holidays observed), placing each through the same guarded path
  as `drip tick`. Preview by default; `--execute` places and `--live` allows a real account. A
  position whose scheduled time has already passed when the daemon starts is caught up
  immediately (idempotently, only within the trading window); one position's failure is logged
  and never stops the others; SIGINT/SIGTERM stops it cleanly between fires.

### Changed

- **KIS is no longer read-only** — it can place US overseas orders (LOC / limit). Going live is
  guarded at runtime (capability + dry-run default + `--live` + risk check + idempotency)
  instead of being blocked by the type system. **토스증권 (Toss) stays read-only** (it has no
  모의 sandbox).
- KIS order prices are rounded to the US $0.01 tick before sending.
- **`drip tick` now reconciles settled fills before deciding**, so today's tranche is computed
  from an up-to-date ledger rather than a stale one. A preview reconciles in-memory (no state
  write — like `dry-run`); `--execute` and the standalone `drip reconcile` persist the advance.
- **The idempotency key and reconcile boundary now use the US Eastern trading date, not UTC.**
  An Eastern session runs into the next UTC day, so a UTC key flipped after the close and risked
  a double-place on an after-hours rerun; keying on the Eastern date (DST-aware) is stable.

### Fixed

- **KIS commands now work against a 모의(paper) account.** drip fired several KIS requests per
  command within the same second, exceeding KIS's per-second limit — which 모의 enforces strictly
  (it rejects the burst with `EGW00201` "초당 거래건수 초과", wrapped in an HTTP 500). So
  `drip tick` / `drip run` failed on the quote after reconciling, and `drip account` failed to
  read the balance. Every KIS request is now spaced under the broker's per-second limit, so a
  single command's multiple calls run cleanly (back-to-back commands too — see below).
- **Back-to-back KIS commands no longer hit a token 403.** Each `drip` command is a separate
  process, and each was re-issuing a KIS OAuth token; KIS allows ~1 token/min per app key, so
  commands run in quick succession got `403` on the token endpoint — and `drip run` with two or
  more KIS positions tripped the same limit at the daily fire. The token (valid ~24h) is now
  cached on disk (`~/.drip/token-kis-*.json`, `0600`), so it is issued at most once per day
  across every command and daemon position.
- **Back-to-back KIS commands no longer trip the per-second limit either.** The request spacing
  is now also shared on disk (`~/.drip/ratelimit-kis-*.json`, `0600`), so commands run in quick
  succession — `drip account` then `drip quote`, or a script chaining ticks — stay under KIS's
  per-second cap across separate processes, not just within one command. Commands may briefly
  pause (≈1s on 모의) to respect the limit.

> **Limitations.** Reconciliation is per **completed day** (a day's fills are applied once that
> day is past), which is exact for drip's day orders (LOC / day-limit) but not intended for
> intraday partial-fill precision. The scheduler fires on a daily cadence only — realtime
> triggers (on tick / on price-cross) arrive with the M3 WebSocket feed. Toss order placement is
> not supported (read-only positions are skipped by `drip run`).
