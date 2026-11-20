//! When checks fall due and whether their latest results are still current.
//! Every decision takes an explicit `now`, so callers control time.

use crate::{config::Check, store::ProblemState};
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;

/// Slack beyond a due execution's own timeout before it counts as overdue.
pub const OVERDUE_GRACE: TimeDelta = TimeDelta::seconds(30);

/// When a check is next due and allowed to run after the result that finished
/// at `finished`. Pending problems use retry timing.
pub fn due_at(
    check: &Check,
    retry_interval_seconds: u64,
    finished: DateTime<Utc>,
    problem: ProblemState,
) -> DateTime<Utc> {
    let interval = if problem.needs_retry() {
        retry_interval_seconds
    } else {
        check.interval_seconds
    };
    finished + seconds(interval)
}

/// Whether a check due at `due` should have produced a newer result by `now`.
pub fn is_overdue(check: &Check, due: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now > due + seconds(check.timeout_seconds) + OVERDUE_GRACE
}

/// How long after `now` each check becomes eligible at startup, given when it
/// is due, or `None` if it has never run. Never-run checks start now and checks
/// not yet due wait for their due time. Overdue checks are spaced evenly, oldest
/// first, across `spread`, so a restart neither forgets them nor launches them
/// all at once. Delays are returned in input order.
pub fn startup_delays<K>(
    checks: Vec<(K, Option<DateTime<Utc>>)>,
    now: DateTime<Utc>,
    spread: Duration,
) -> Vec<(K, Duration)> {
    let mut overdue: Vec<_> = checks
        .iter()
        .enumerate()
        .filter_map(|(index, (_, due))| due.filter(|due| *due <= now).map(|due| (due, index)))
        .collect();
    overdue.sort();
    let mut delays: Vec<_> = checks
        .iter()
        .map(|(_, due)| match due {
            Some(due) if *due > now => (*due - now).to_std().unwrap_or_default(),
            _ => Duration::ZERO,
        })
        .collect();
    let count = overdue.len() as u32;
    for (slot, (_, index)) in overdue.into_iter().enumerate() {
        delays[index] = spread * slot as u32 / count;
    }
    checks
        .into_iter()
        .zip(delays)
        .map(|((key, _), delay)| (key, delay))
        .collect()
}

// Validation bounds configured intervals and deadlines to 32 bits.
fn seconds(seconds: u64) -> TimeDelta {
    TimeDelta::seconds(seconds as i64)
}
