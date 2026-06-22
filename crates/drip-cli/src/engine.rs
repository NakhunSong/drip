//! The `drip run` scheduler engine — a driving adapter that fires positions on their schedule.
//!
//! The scheduling *brains* are pure and live in the domain (`drip_domain::schedule` +
//! `calendar`); this module is the async glue: sleep until the next fire, fire any due jobs,
//! and stop cleanly on a shutdown signal. Two safety properties matter for an unattended
//! daemon and are enforced here:
//!
//! - **Per-position error isolation** — one job's failure is logged and never aborts the loop,
//!   so a single bad ticker cannot silently halt trading for the others.
//! - **Catch-up on start** — a schedule already due when the daemon launches fires immediately
//!   rather than waiting until tomorrow. The at-most-once order journal (keyed on the Eastern
//!   date) makes that catch-up safe: re-firing a day already handled places nothing new.

use std::future::Future;
use std::time::Duration;

use drip_domain::schedule::{Schedule, is_due, next_fire};
use time::OffsetDateTime;

/// Run the scheduler loop until `shutdown` resolves.
///
/// `fire(i, now)` acts on the `i`-th schedule (places that position's orders); its `Err` is
/// logged and isolated so other jobs and later fires continue. `now` supplies the current
/// instant — injected rather than read directly so the loop is deterministic under test.
pub async fn run<F>(
    schedules: &[Schedule],
    mut fire: F,
    now: impl Fn() -> OffsetDateTime,
    shutdown: impl Future<Output = ()>,
) where
    F: AsyncFnMut(usize, OffsetDateTime) -> anyhow::Result<()>,
{
    if schedules.is_empty() {
        tracing::warn!("no schedules configured; engine has nothing to run");
        return;
    }
    tokio::pin!(shutdown);
    loop {
        let fired_at = now();
        for (i, schedule) in schedules.iter().enumerate() {
            if is_due(schedule, fired_at) {
                // Isolate each job: a failure is logged and the loop carries on, so one bad
                // position can never silently halt trading for the others.
                let outcome = fire(i, fired_at).await;
                if let Err(e) = outcome {
                    tracing::error!("scheduled fire for job {i} failed: {e:#}");
                }
            }
        }
        // Re-read the clock after firing (a fire takes real time) and use that one timestamp
        // for both the next-fire search and the sleep, so they stay consistent.
        let after = now();
        let next = schedules
            .iter()
            .map(|s| next_fire(s, after))
            .min()
            .expect("schedules is non-empty");
        let wait = (next - after).whole_milliseconds().max(0) as u64;
        tracing::info!("next fire at {next}; sleeping {wait} ms");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            _ = &mut shutdown => {
                tracing::info!("shutdown signal received; engine stopping");
                return;
            }
        }
    }
}

/// Resolves when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM — the signal for
/// the daemon to stop between fires.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!("could not install SIGTERM handler ({e}); Ctrl-C only");
                ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use time::macros::datetime;

    #[tokio::test]
    async fn fires_a_due_schedule_then_stops_on_shutdown() {
        // 10:00 EDT Mon — the 09:00 schedule is already due, so it catches up immediately.
        let now = datetime!(2026-06-22 14:00 UTC);
        let fired = RefCell::new(Vec::new());
        run(
            &[Schedule::daily_before_open()],
            async |i, _| {
                fired.borrow_mut().push(i);
                Ok(())
            },
            || now,
            std::future::ready(()),
        )
        .await;
        assert_eq!(*fired.borrow(), vec![0]);
    }

    #[tokio::test]
    async fn does_not_fire_before_the_scheduled_time() {
        // 08:00 EDT Mon — before the 09:00 schedule, so nothing fires before we shut down.
        let now = datetime!(2026-06-22 12:00 UTC);
        let fired = RefCell::new(Vec::new());
        run(
            &[Schedule::daily_before_open()],
            async |i, _| {
                fired.borrow_mut().push(i);
                Ok(())
            },
            || now,
            std::future::ready(()),
        )
        .await;
        assert!(fired.borrow().is_empty());
    }

    #[tokio::test]
    async fn isolates_a_failing_job_from_the_others() {
        // Both schedules are due; job 0 errors but must not stop job 1 from firing.
        let now = datetime!(2026-06-22 14:00 UTC);
        let fired = RefCell::new(Vec::new());
        run(
            &[Schedule::daily_before_open(), Schedule::daily_before_open()],
            async |i, _| {
                fired.borrow_mut().push(i);
                if i == 0 {
                    anyhow::bail!("simulated placement failure");
                }
                Ok(())
            },
            || now,
            std::future::ready(()),
        )
        .await;
        assert_eq!(*fired.borrow(), vec![0, 1]);
    }
}
