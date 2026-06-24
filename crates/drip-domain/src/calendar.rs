//! Market calendar — pure, deterministic trading-date logic, per [`Market`].
//!
//! A trading calendar is a property of the *market*, not of any one broker, so it lives
//! in the domain (like [`settle`](crate::settle) and [`risk`](crate::risk)) and is shared by
//! the CLI, the scheduler engine, and broker adapters. Every function takes the current
//! instant (or a date) as input and performs no I/O, so it is fully unit-testable offline.
//!
//! drip's canonical "trading date" is the market's local **session date** —
//! [`Market::UsEquity`] uses US Eastern (DST-aware), [`Market::KrEquity`] uses Korea (KST,
//! UTC+9, no DST). The idempotency key and the reconcile boundary key on it, so a rerun at
//! any wall-clock time — even after UTC midnight while it is still the same local session —
//! maps to one date. (Holiday sets feed only [`is_trading_day`], used by the scheduler, which
//! covers NYSE only; KRX holidays arrive with the domestic daemon — see #22.)

use time::macros::{offset, time};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset, Weekday};

/// The market whose trading calendar applies to a position — selects the session time zone
/// (and, for the scheduler, the holiday set). Derived from the position's broker at the
/// composition root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Market {
    /// US equities — sessions keyed to US Eastern time (DST-aware), NYSE holidays.
    UsEquity,
    /// Korean equities (KRX) — sessions keyed to Korea Standard Time (UTC+9, no DST).
    KrEquity,
}

/// The market's local trading-date for a UTC instant — the date drip keys the idempotency
/// key and the reconcile boundary on, so a rerun later the same session maps to one date.
/// US uses the DST-aware Eastern date; Korea uses KST (UTC+9, no daylight saving), so the
/// date never flips mid-session the way the Eastern date does for a KRX instant.
pub fn trading_date(market: Market, now: OffsetDateTime) -> Date {
    match market {
        Market::UsEquity => us_eastern_date(now),
        Market::KrEquity => now.to_offset(offset!(+9)).date(),
    }
}

/// The US Eastern calendar date for a UTC instant, honoring daylight saving time.
///
/// US DST runs from 02:00 local on the second Sunday of March (spring-forward, 07:00 UTC) to
/// 02:00 local on the first Sunday of November (fall-back, 06:00 UTC); the offset is −4 (EDT)
/// inside that window and −5 (EST) outside it. We pick the offset by comparing the UTC
/// *instant* against those two transition instants, which is exact — there is no date to
/// extract first, so the usual "which day is it" ambiguity around a transition never arises.
pub fn us_eastern_date(now: OffsetDateTime) -> Date {
    eastern(now).date()
}

/// `now` re-expressed in the US Eastern time zone (DST-aware). The CLI keys the trading date
/// and the scheduler keys fire times off this.
pub(crate) fn eastern(now: OffsetDateTime) -> OffsetDateTime {
    now.to_offset(us_eastern_offset(now))
}

/// The UTC offset in effect for the US Eastern time zone at the UTC instant `now`.
fn us_eastern_offset(now: OffsetDateTime) -> UtcOffset {
    let (spring_date, fall_date) = dst_window(now.year());
    // Spring-forward: 02:00 EST = 07:00 UTC. Fall-back: 02:00 EDT = 06:00 UTC.
    let spring = OffsetDateTime::new_utc(spring_date, time!(7:00));
    let fall = OffsetDateTime::new_utc(fall_date, time!(6:00));
    if (spring..fall).contains(&now) {
        offset!(-4) // EDT
    } else {
        offset!(-5) // EST
    }
}

/// The US daylight-saving window for `year`: [second Sunday of March, first Sunday of November).
fn dst_window(year: i32) -> (Date, Date) {
    (
        nth_weekday(year, Month::March, Weekday::Sunday, 2),
        nth_weekday(year, Month::November, Weekday::Sunday, 1),
    )
}

/// The UTC instant at which the US Eastern wall-clock `date` + `time` occurs (DST-aware). Turns
/// a schedule's local fire time into a real instant. Trading fire times sit well clear of the
/// 02:00 DST switch, so selecting the offset by date is exact here.
pub(crate) fn eastern_at(date: Date, time: Time) -> OffsetDateTime {
    let (spring, fall) = dst_window(date.year());
    let offset = if (spring..fall).contains(&date) {
        offset!(-4)
    } else {
        offset!(-5)
    };
    PrimitiveDateTime::new(date, time).assume_offset(offset)
}

/// Whether `date` is a US equity trading day: a weekday that is not a NYSE holiday.
pub fn is_trading_day(date: Date) -> bool {
    !matches!(date.weekday(), Weekday::Saturday | Weekday::Sunday)
        && !nyse_holidays(date.year()).contains(&date)
}

/// The dates the NYSE is closed in `year` (observed closures, not the nominal holiday dates).
fn nyse_holidays(year: i32) -> Vec<Date> {
    let mut days = Vec::with_capacity(10);
    // New Year's Day: no Saturday make-up (the market stays open the prior Dec 31); a Sunday
    // New Year is observed the following Monday.
    let new_year = ymd(year, Month::January, 1);
    match new_year.weekday() {
        Weekday::Saturday => {}
        Weekday::Sunday => days.push(ymd(year, Month::January, 2)),
        _ => days.push(new_year),
    }
    days.push(nth_weekday(year, Month::January, Weekday::Monday, 3)); // MLK Day
    days.push(nth_weekday(year, Month::February, Weekday::Monday, 3)); // Washington's Birthday
    days.push(good_friday(year));
    days.push(last_weekday(year, Month::May, Weekday::Monday)); // Memorial Day
    if year >= 2022 {
        days.push(observed(ymd(year, Month::June, 19))); // Juneteenth (NYSE holiday since 2022)
    }
    days.push(observed(ymd(year, Month::July, 4))); // Independence Day
    days.push(nth_weekday(year, Month::September, Weekday::Monday, 1)); // Labor Day
    days.push(nth_weekday(year, Month::November, Weekday::Thursday, 4)); // Thanksgiving
    days.push(observed(ymd(year, Month::December, 25))); // Christmas
    days
}

/// A fixed-date holiday's observed closure: shifted to the Friday before if it lands on a
/// Saturday, or the Monday after if it lands on a Sunday.
fn observed(date: Date) -> Date {
    match date.weekday() {
        Weekday::Saturday => date.previous_day().expect("a Friday precedes any Saturday"),
        Weekday::Sunday => date.next_day().expect("a Monday follows any Sunday"),
        _ => date,
    }
}

/// Good Friday — two days before Easter Sunday.
fn good_friday(year: i32) -> Date {
    easter_sunday(year)
        .previous_day()
        .and_then(Date::previous_day)
        .expect("two days before Easter is in range")
}

/// Easter Sunday for `year`, via the Anonymous Gregorian Computus.
fn easter_sunday(year: i32) -> Date {
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let n = h + l - 7 * m + 114;
    let month = Month::try_from((n / 31) as u8).expect("Computus yields March or April");
    let day = (n % 31) as u8 + 1;
    ymd(year, month, day)
}

/// The last `weekday` of `month` in `year`, e.g. the last Monday of May (Memorial Day).
fn last_weekday(year: i32, month: Month, weekday: Weekday) -> Date {
    let mut date = ymd(year, month, month.length(year));
    while date.weekday() != weekday {
        date = date
            .previous_day()
            .expect("walking back within a month stays in range");
    }
    date
}

/// The date of the `n`-th `weekday` (1-based) of `month` in `year`, e.g. the 2nd Sunday of
/// March. `n` must be small enough to stay within the month (callers pass 1–4).
fn nth_weekday(year: i32, month: Month, weekday: Weekday, n: u8) -> Date {
    let first = ymd(year, month, 1);
    let shift =
        (7 + weekday.number_days_from_sunday() - first.weekday().number_days_from_sunday()) % 7;
    ymd(year, month, 1 + shift + (n - 1) * 7)
}

/// A [`Date`] from year/month/day, panicking only on a statically-invalid date.
fn ymd(year: i32, month: Month, day: u8) -> Date {
    Date::from_calendar_date(year, month, day).expect("valid calendar date")
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    #[test]
    fn winter_instant_resolves_to_est_date() {
        // 02:00 UTC on Jan 15 is 21:00 EST on Jan 14.
        assert_eq!(
            us_eastern_date(datetime!(2026-01-15 02:00 UTC)),
            date!(2026 - 01 - 14)
        );
    }

    #[test]
    fn summer_instant_resolves_to_edt_date() {
        // 02:00 UTC on Jul 15 is 22:00 EDT on Jul 14.
        assert_eq!(
            us_eastern_date(datetime!(2026-07-15 02:00 UTC)),
            date!(2026 - 07 - 14)
        );
    }

    #[test]
    fn after_hours_rerun_keeps_the_same_eastern_session_date() {
        // The bug #3 fixes: a US session on Mon 2026-06-22 (close 20:00 UTC). Re-running the
        // tick that evening at 01:00 UTC Tue is still 21:00 EDT Mon — the UTC date has rolled
        // to the 23rd but the Eastern session date is still the 22nd, so the idempotency key
        // stays stable instead of flipping and risking a double-place.
        assert_eq!(
            us_eastern_date(datetime!(2026-06-23 01:00 UTC)),
            date!(2026 - 06 - 22)
        );
    }

    #[test]
    fn dst_offset_changes_the_date_at_the_utc_evening_boundary() {
        // 04:30 UTC straddles Eastern midnight differently by season. In summer (EDT −4) it is
        // 00:30 the same day; in winter (EST −5) it is 23:30 the previous day.
        assert_eq!(
            us_eastern_date(datetime!(2026-07-01 04:30 UTC)),
            date!(2026 - 07 - 01)
        );
        assert_eq!(
            us_eastern_date(datetime!(2026-01-01 04:30 UTC)),
            date!(2025 - 12 - 31)
        );
    }

    #[test]
    fn spring_forward_transition_switches_offset_exactly() {
        // 2026 spring-forward is Sun Mar 8, 07:00 UTC. Just before, EST applies (01:59 EST);
        // at/after, EDT applies (03:00 EDT). Both are the same calendar day, but the boundary
        // must be exact — verify via the offset selection.
        assert_eq!(
            us_eastern_offset(datetime!(2026-03-08 06:59 UTC)),
            offset!(-5)
        );
        assert_eq!(
            us_eastern_offset(datetime!(2026-03-08 07:00 UTC)),
            offset!(-4)
        );
    }

    #[test]
    fn fall_back_transition_switches_offset_exactly() {
        // 2026 fall-back is Sun Nov 1, 06:00 UTC: EDT just before, EST at/after.
        assert_eq!(
            us_eastern_offset(datetime!(2026-11-01 05:59 UTC)),
            offset!(-4)
        );
        assert_eq!(
            us_eastern_offset(datetime!(2026-11-01 06:00 UTC)),
            offset!(-5)
        );
    }

    #[test]
    fn nth_weekday_finds_dst_transition_sundays() {
        assert_eq!(
            nth_weekday(2026, Month::March, Weekday::Sunday, 2),
            date!(2026 - 03 - 08)
        );
        assert_eq!(
            nth_weekday(2026, Month::November, Weekday::Sunday, 1),
            date!(2026 - 11 - 01)
        );
    }

    #[test]
    fn nyse_holidays_2026_match_the_official_calendar() {
        let holidays = nyse_holidays(2026);
        for day in [
            date!(2026 - 01 - 01), // New Year's Day (Thu)
            date!(2026 - 01 - 19), // MLK Day
            date!(2026 - 02 - 16), // Washington's Birthday
            date!(2026 - 04 - 03), // Good Friday
            date!(2026 - 05 - 25), // Memorial Day
            date!(2026 - 06 - 19), // Juneteenth
            date!(2026 - 07 - 03), // Independence Day observed (Jul 4 is a Saturday)
            date!(2026 - 09 - 07), // Labor Day
            date!(2026 - 11 - 26), // Thanksgiving
            date!(2026 - 12 - 25), // Christmas
        ] {
            assert!(holidays.contains(&day), "expected {day} to be a holiday");
        }
        assert_eq!(holidays.len(), 10);
    }

    #[test]
    fn nyse_holidays_2027_observe_weekend_shifts() {
        let holidays = nyse_holidays(2027);
        assert!(holidays.contains(&date!(2027 - 03 - 26))); // Good Friday
        assert!(holidays.contains(&date!(2027 - 06 - 18))); // Juneteenth (Jun 19 is a Saturday)
        assert!(holidays.contains(&date!(2027 - 07 - 05))); // Independence Day (Jul 4 is a Sunday)
        assert!(holidays.contains(&date!(2027 - 12 - 24))); // Christmas (Dec 25 is a Saturday)
    }

    #[test]
    fn is_trading_day_excludes_weekends_and_holidays() {
        assert!(is_trading_day(date!(2026 - 06 - 22))); // a normal Monday
        assert!(!is_trading_day(date!(2026 - 06 - 20))); // Saturday
        assert!(!is_trading_day(date!(2026 - 07 - 03))); // Independence Day observed
        assert!(!is_trading_day(date!(2026 - 12 - 25))); // Christmas
        assert!(is_trading_day(date!(2026 - 12 - 24))); // Christmas Eve still trades
    }

    #[test]
    fn korea_trading_date_is_stable_across_the_eastern_rollover_intraday() {
        // The #22 bug: two `drip tick` runs in the same KRX session (Wed 2026-06-24, 12:00 and
        // 14:30 KST = 03:00 and 05:30 UTC) must hash to ONE idempotency key. The US-Eastern date
        // rolls between them (13:00 KST ≈ Eastern midnight), so keying a KRX order on it would
        // split into two keys and double-place; the KST trading date stays 06-24 for both.
        let noon_kst = datetime!(2026-06-24 03:00 UTC);
        let afternoon_kst = datetime!(2026-06-24 05:30 UTC);
        assert_eq!(
            trading_date(Market::KrEquity, noon_kst),
            date!(2026 - 06 - 24)
        );
        assert_eq!(
            trading_date(Market::KrEquity, afternoon_kst),
            date!(2026 - 06 - 24)
        );
        // The same instants on the US-Eastern calendar straddle midnight → two different dates.
        assert_ne!(us_eastern_date(noon_kst), us_eastern_date(afternoon_kst));
    }

    #[test]
    fn us_trading_date_matches_the_eastern_date() {
        let now = datetime!(2026-06-23 01:00 UTC);
        assert_eq!(trading_date(Market::UsEquity, now), us_eastern_date(now));
        assert_eq!(trading_date(Market::UsEquity, now), date!(2026 - 06 - 22));
    }

    #[test]
    fn korea_trading_date_has_no_dst_shift() {
        // KST is UTC+9 year-round (Korea has no DST), so winter and summer instants convert with
        // the same offset — unlike the US Eastern date. 15:00 UTC = 00:00 KST the next day, both
        // seasons.
        assert_eq!(
            trading_date(Market::KrEquity, datetime!(2026-01-15 15:00 UTC)),
            date!(2026 - 01 - 16)
        );
        assert_eq!(
            trading_date(Market::KrEquity, datetime!(2026-07-15 15:00 UTC)),
            date!(2026 - 07 - 16)
        );
    }
}
