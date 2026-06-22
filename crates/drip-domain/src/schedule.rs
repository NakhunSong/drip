//! Scheduling vocabulary — when the engine wakes a strategy.
//!
//! The `Strategy` port is cadence-agnostic: [`decide`](crate::Strategy::decide) does not know
//! *when* it is called. A [`Trigger`] declares that, and the `drip run` engine wires the
//! matching event source. M2 implements the [`Schedule`](Trigger::Schedule) trigger — the daily
//! batch cadence that 무한매수법 uses; realtime triggers (on tick / on price-cross) arrive with
//! the WebSocket feed in M3.

use crate::calendar::{eastern, eastern_at, is_trading_day};
use time::macros::time;
use time::{OffsetDateTime, Time};

/// A daily fire at a fixed US Eastern wall-clock time, on every trading day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Local US Eastern time of day to fire at.
    pub at_eastern: Time,
}

impl Schedule {
    /// The default batch cadence: 09:00 Eastern, half an hour before the 09:30 open, to stage
    /// the day's limit-on-close orders ahead of the session.
    pub fn daily_before_open() -> Self {
        Self {
            at_eastern: time!(9:00),
        }
    }
}

/// When the engine should call a strategy's [`decide`](crate::Strategy::decide). M2 wires
/// [`Schedule`](Trigger::Schedule) only; realtime variants land with the M3 WebSocket feed.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// Fire on a wall-clock schedule (the cadence batch strategies use).
    Schedule(Schedule),
}

/// The next instant strictly after `now` at which `schedule` fires: its Eastern time on the
/// soonest trading day not already past. Drives the engine's sleep between fires.
pub fn next_fire(schedule: &Schedule, now: OffsetDateTime) -> OffsetDateTime {
    let mut day = eastern(now).date();
    loop {
        if is_trading_day(day) {
            let fire = eastern_at(day, schedule.at_eastern);
            if fire > now {
                return fire;
            }
        }
        day = day.next_day().expect("a next calendar day always exists");
    }
}

/// The NYSE regular-session close, in US Eastern time. Catch-up fires are bounded to *before*
/// this: a day order placed after the close cannot fill today and — depending on the broker —
/// could carry into the next session, where it would double-count against that day's
/// date-keyed tranche and over-buy. Bounding catch-up to the trading window keeps the
/// strategy's over-buy guard from resting on broker after-hours behavior.
const REGULAR_CLOSE: Time = time!(16:00);

/// Whether `schedule` is due at `now`: today (Eastern) is a trading day and the current Eastern
/// time is within the trading window `[at_eastern, 16:00)`. The engine fires due schedules on
/// startup, so a daemon launched mid-day still acts today — the at-most-once journal makes that
/// catch-up safe — but one launched after the close waits for the next session rather than
/// placing an order that cannot fill today.
pub fn is_due(schedule: &Schedule, now: OffsetDateTime) -> bool {
    let local = eastern(now);
    is_trading_day(local.date())
        && local.time() >= schedule.at_eastern
        && local.time() < REGULAR_CLOSE
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn next_fire_is_today_when_before_the_scheduled_time() {
        // Mon 2026-06-22, 12:00 UTC = 08:00 EDT, before the 09:00 fire.
        let s = Schedule::daily_before_open();
        assert_eq!(
            next_fire(&s, datetime!(2026-06-22 12:00 UTC)),
            datetime!(2026-06-22 9:00 -4)
        );
    }

    #[test]
    fn next_fire_rolls_to_the_next_trading_day_after_the_time_passes() {
        // Mon 2026-06-22, 14:00 UTC = 10:00 EDT, past the 09:00 fire → next is Tue.
        let s = Schedule::daily_before_open();
        assert_eq!(
            next_fire(&s, datetime!(2026-06-22 14:00 UTC)),
            datetime!(2026-06-23 9:00 -4)
        );
    }

    #[test]
    fn next_fire_skips_the_weekend() {
        // Fri 2026-06-26 after the fire → Mon 2026-06-29 (skips Sat/Sun).
        let s = Schedule::daily_before_open();
        assert_eq!(
            next_fire(&s, datetime!(2026-06-26 20:00 UTC)),
            datetime!(2026-06-29 9:00 -4)
        );
    }

    #[test]
    fn next_fire_skips_a_holiday_and_the_weekend_behind_it() {
        // Thu 2026-07-02 after the fire: Fri Jul 3 is Independence Day (observed), then Sat/Sun,
        // so the next fire is Mon Jul 6.
        let s = Schedule::daily_before_open();
        assert_eq!(
            next_fire(&s, datetime!(2026-07-02 20:00 UTC)),
            datetime!(2026-07-06 9:00 -4)
        );
    }

    #[test]
    fn is_due_only_within_the_trading_window_on_a_trading_day() {
        let s = Schedule::daily_before_open();
        assert!(is_due(&s, datetime!(2026-06-22 14:00 UTC))); // 10:00 EDT Mon — due
        assert!(is_due(&s, datetime!(2026-06-22 18:00 UTC))); // 14:00 EDT — mid-session catch-up
        assert!(!is_due(&s, datetime!(2026-06-22 12:00 UTC))); // 08:00 EDT — before the fire time
        assert!(!is_due(&s, datetime!(2026-06-22 21:00 UTC))); // 17:00 EDT — after the close
        assert!(!is_due(&s, datetime!(2026-06-20 20:00 UTC))); // Saturday — never due
    }
}
