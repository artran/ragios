//! When checks fall due and whether their latest results are still current.
//! Every decision takes an explicit `now`, so callers control time.

use crate::{config::Check, store::ProblemState};
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;

/// Slack beyond a due execution's own timeout before it counts as overdue.
pub const OVERDUE_GRACE: TimeDelta = TimeDelta::seconds(30);

/// When a check is next due and allowed to run after the result that finished
/// at `finished`. Pending problems use retry timing. A result that claims to
/// finish after `now` means the clock moved back, so its due time is unknown.
pub fn due_at(
    check: &Check,
    retry_interval_seconds: u64,
    finished: DateTime<Utc>,
    problem: ProblemState,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if finished > now {
        return None;
    }
    let interval = if problem.needs_retry() {
        retry_interval_seconds
    } else {
        check.interval_seconds
    };
    Some(finished + seconds(interval))
}

/// Whether a check due at `due` should have produced a newer result by `now`.
pub fn is_overdue(check: &Check, due: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now > due + seconds(check.timeout_seconds) + OVERDUE_GRACE
}

/// How long after `now` each check becomes eligible at startup, given when it
/// is due, or `None` if it has never run. Never-run checks start now. Checks
/// due before the end of the `spread` period take evenly spaced slots across
/// it in due order, oldest first, so a restart neither forgets overdue checks
/// nor launches them all at once; none runs before it is due. Later checks wait
/// for their due time. Delays are returned in input order.
pub fn startup_delays<K>(
    checks: Vec<(K, Option<DateTime<Utc>>)>,
    now: DateTime<Utc>,
    spread: Duration,
) -> Vec<(K, Duration)> {
    let until = |due: DateTime<Utc>| (due - now).to_std().unwrap_or_default();
    let mut catch_up: Vec<_> = checks
        .iter()
        .enumerate()
        .filter_map(|(index, (_, due))| {
            due.filter(|due| until(*due) < spread || *due <= now)
                .map(|due| (due, index))
        })
        .collect();
    catch_up.sort();
    let mut delays: Vec<_> = checks
        .iter()
        .map(|(_, due)| due.map_or(Duration::ZERO, until))
        .collect();
    let count = catch_up.len() as u32;
    for (slot, (due, index)) in catch_up.into_iter().enumerate() {
        delays[index] = until(due).max(spread * slot as u32 / count);
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
