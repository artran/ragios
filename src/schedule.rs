//! When checks fall due and whether their latest results are still current.
//! Wall-clock decisions take explicit timestamps; cadence can also drive
//! monotonic timers without a wall clock.

use crate::{
    config::{Check, CheckWindow},
    runner::CheckResult,
    store::ProblemState,
};
use chrono::{
    DateTime, Days, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta, TimeZone, Utc,
};
use chrono_tz::Tz;
use std::time::Duration;

/// The repeat cadence, independent of the clock used to measure it.
#[derive(Clone, Copy)]
pub enum Cadence {
    Normal,
    Retry,
}

impl From<ProblemState> for Cadence {
    fn from(problem: ProblemState) -> Self {
        if problem.needs_retry() {
            Self::Retry
        } else {
            Self::Normal
        }
    }
}

impl Cadence {
    pub fn interval(self, check: &Check, retry_interval_seconds: u64) -> Duration {
        Duration::from_secs(match self {
            Self::Normal => check.interval_seconds,
            Self::Retry => retry_interval_seconds,
        })
    }
}

/// Slack beyond a due execution's own timeout before it counts as overdue.
const OVERDUE_GRACE: TimeDelta = TimeDelta::seconds(30);

/// Wall-clock freshness, independent of the plugin's reported status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    NotYetChecked,
    Current(DateTime<Utc>),
    Overdue(Option<DateTime<Utc>>),
    /// Overdue without ever saving a result.
    Missing(DateTime<Utc>),
}

impl Freshness {
    pub fn is_overdue(self) -> bool {
        matches!(self, Self::Overdue(_) | Self::Missing(_))
    }
}

/// Assess when a newer result should have been available. Use the first due
/// time only in the absence of a saved result. Apply the window at the original
/// due time, not at `now`: a missed deadline survives a later window closure.
pub fn freshness(
    check: &Check,
    retry_interval_seconds: u64,
    saved: Option<(&CheckResult, ProblemState)>,
    first_due: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Freshness {
    let Some((result, problem)) = saved else {
        return match first_due.map(|since| eligible_from(check, since)) {
            Some(due) if is_overdue(check, due, now) => Freshness::Missing(due),
            Some(due) => Freshness::Current(due),
            None => Freshness::NotYetChecked,
        };
    };
    let Some(due) = interval_due_at(check, retry_interval_seconds, result, problem, now) else {
        return Freshness::Overdue(None);
    };
    let due = eligible_from(check, due);
    if is_overdue(check, due, now) {
        Freshness::Overdue(Some(due))
    } else {
        Freshness::Current(due)
    }
}

/// Restore the saved result's interval deadline, ignoring Check windows.
/// An unreadable or future finish time leaves the deadline unknown.
pub fn interval_due_at(
    check: &Check,
    retry_interval_seconds: u64,
    result: &CheckResult,
    problem: ProblemState,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let finished = result.finished()?;
    if finished > now {
        return None;
    }
    let interval = Cadence::from(problem).interval(check, retry_interval_seconds);
    Some(finished + TimeDelta::from_std(interval).expect("bounded check interval"))
}

/// The earliest instant at or after `at` when `check` may start.
pub fn eligible_from(check: &Check, at: DateTime<Utc>) -> DateTime<Utc> {
    check
        .window
        .as_ref()
        .map_or(at, |window| next_open(window, at))
}

/// How long after `at` the check's window keeps it from starting.
pub fn window_wait(check: &Check, at: DateTime<Utc>) -> Duration {
    (eligible_from(check, at) - at).to_std().unwrap_or_default()
}

/// A closed window next opens where a range starts, where a range starting
/// inside a skipped daylight-saving hour first exists, or where clocks go back
/// into the middle of a range. A repeated hour offers both of its instants.
/// Validation guarantees a range, so one opens within a week, or within two
/// when daylight saving skips an entire occurrence.
fn next_open(window: &CheckWindow, at: DateTime<Utc>) -> DateTime<Utc> {
    if window.is_open(at) {
        return at;
    }
    let today = at.with_timezone(&window.timezone).date_naive();
    let days: Vec<_> = (0..=15)
        .filter_map(|offset| today.checked_add_days(Days::new(offset)))
        .collect();
    let starts = days.iter().flat_map(|&day| {
        window
            .ranges
            .iter()
            .filter_map(move |range| range.start_on(day))
            .flat_map(|start| resolve_local(window.timezone, start))
    });
    let set_backs = days
        .iter()
        .filter_map(|&day| clocks_go_back(window.timezone, day));
    starts
        .chain(set_backs)
        .filter(|&instant| instant > at && window.is_open(instant))
        .min()
        .expect("a validated window opens within two weeks")
}

/// When local clocks go back during `day`, if they do.
fn clocks_go_back(timezone: Tz, day: NaiveDate) -> Option<DateTime<Utc>> {
    let offset = |at: DateTime<Utc>| at.with_timezone(&timezone).offset().fix().local_minus_utc();
    let start = *resolve_local(timezone, day.and_time(NaiveTime::MIN)).first()?;
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

/// The instants at which a local clock shows `local`: one normally, two in a
/// repeated hour. A skipped local time resolves to the first minute after it
/// that exists.
fn resolve_local(timezone: Tz, local: NaiveDateTime) -> Vec<DateTime<Utc>> {
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
fn is_overdue(check: &Check, due: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now > due + seconds(check.timeout_seconds) + OVERDUE_GRACE
}

/// How long after `now` each check becomes eligible at startup, given when it
/// is due, or `None` if its deadline is unknown. Unknown deadlines are
/// eligible immediately. Checks
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
