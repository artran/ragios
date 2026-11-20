//! When checks fall due and whether their latest results are still current.
//! Every decision takes an explicit `now`, so callers control time.

use crate::{
    config::{Check, CheckWindow},
    store::ProblemState,
};
use chrono::{DateTime, Datelike, Days, NaiveDateTime, Offset, TimeDelta, TimeZone, Timelike, Utc};
use std::time::Duration;

/// Slack beyond a due execution's own timeout before it counts as overdue.
pub const OVERDUE_GRACE: TimeDelta = TimeDelta::seconds(30);

/// When a check is next due and allowed to run after the result that finished
/// at `finished`. Pending problems use retry timing, and a check due while its
/// window is closed falls due when the window next opens. A result that claims to
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
    Some(eligible_from(check, finished + seconds(interval)))
}

/// The earliest instant at or after `at` when `check` may start.
pub fn eligible_from(check: &Check, at: DateTime<Utc>) -> DateTime<Utc> {
    check
        .window
        .as_ref()
        .map_or(at, |window| next_open(window, at))
}

/// Whether local wall-clock time at `at` falls inside one of the window's
/// ranges. Local time keeps the window fixed across daylight-saving changes.
fn is_open(window: &CheckWindow, at: DateTime<Utc>) -> bool {
    let local = at.with_timezone(&window.timezone);
    let minute = local.hour() * 60 + local.minute();
    window.ranges.iter().any(|range| {
        range.days.contains(&local.weekday())
            && (range.start_minute..range.end_minute).contains(&minute)
    })
}

/// A closed window next opens where a range starts, where a range starting
/// inside a skipped daylight-saving hour first exists, or where clocks go back
/// into the middle of a range. A repeated hour offers both of its instants. Validation guarantees a range, so one opens within a
/// week.
fn next_open(window: &CheckWindow, at: DateTime<Utc>) -> DateTime<Utc> {
    if is_open(window, at) {
        return at;
    }
    let today = at.with_timezone(&window.timezone).date_naive();
    (0..=8)
        .filter_map(|offset| today.checked_add_days(Days::new(offset)))
        .flat_map(|day| {
            window
                .ranges
                .iter()
                .filter(move |range| range.days.contains(&day.weekday()))
                .map(move |range| {
                    day.and_time(chrono::NaiveTime::MIN)
                        + TimeDelta::minutes(range.start_minute.into())
                })
        })
        .flat_map(|start| instants(window.timezone, start))
        .chain(
            (0..=8)
                .filter_map(|offset| today.checked_add_days(Days::new(offset)))
                .filter_map(|day| clocks_go_back(window.timezone, day)),
        )
        .filter(|&instant| instant > at && is_open(window, instant))
        .min()
        .expect("a validated window opens every week")
}

/// When local clocks go back during `day`, if they do.
fn clocks_go_back(timezone: chrono_tz::Tz, day: chrono::NaiveDate) -> Option<DateTime<Utc>> {
    let offset = |at: DateTime<Utc>| at.with_timezone(&timezone).offset().fix().local_minus_utc();
    let start = *instants(timezone, day.and_time(chrono::NaiveTime::MIN)).first()?;
    let (mut before, mut after) = (start, start + TimeDelta::days(1));
    if offset(after) >= offset(before) {
        return None;
    }
    // Transitions fall on whole seconds, so bisect down to one.
    while after - before > TimeDelta::seconds(1) {
        let middle = before + TimeDelta::seconds((after - before).num_seconds() / 2);
        if offset(middle) == offset(start) {
            before = middle;
        } else {
            after = middle;
        }
    }
    Some(after)
}

fn instants(timezone: chrono_tz::Tz, local: NaiveDateTime) -> Vec<DateTime<Utc>> {
    // Skipped local times resolve to the first minute that exists after them.
    (0..=24 * 60)
        .map(|minute| timezone.from_local_datetime(&(local + TimeDelta::minutes(minute))))
        .find_map(|times| times.earliest().map(|earliest| (earliest, times.latest())))
        .map_or_else(Vec::new, |(earliest, latest)| {
            [Some(earliest), latest]
                .into_iter()
                .flatten()
                .map(|time| time.to_utc())
                .collect()
        })
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
