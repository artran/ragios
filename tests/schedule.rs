use chrono::{DateTime, TimeDelta, Utc, Weekday};
use ragios::{
    config::{Check, CheckWindow, WindowRange},
    runner::{CheckResult, Status},
    schedule::{
        Cadence, Freshness, eligible_from, freshness, interval_due_at, startup_delays, window_wait,
    },
    store::ProblemState,
};
use std::time::Duration;

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text).unwrap().to_utc()
}

fn check(interval_seconds: u64, timeout_seconds: u64) -> Check {
    Check {
        executable: "check".into(),
        args: Vec::new(),
        interval_seconds,
        timeout_seconds,
        window: None,
    }
}

const WEEKDAYS: [Weekday; 5] = [
    Weekday::Mon,
    Weekday::Tue,
    Weekday::Wed,
    Weekday::Thu,
    Weekday::Fri,
];

fn windowed(check: Check, timezone: &str, days: &[Weekday], start: &str, end: &str) -> Check {
    Check {
        window: Some(CheckWindow {
            id: "window".into(),
            timezone: timezone.parse().unwrap(),
            ranges: vec![WindowRange {
                days: days.to_vec(),
                start: start.parse().unwrap(),
                end: end.parse().unwrap(),
            }],
        }),
        ..check
    }
}

fn result_at(finished_at: &str) -> CheckResult {
    CheckResult {
        status: Status::Ok,
        output: String::new(),
        started_at: finished_at.into(),
        finished_at: finished_at.into(),
        duration_ms: 0,
    }
}

const PENDING: ProblemState = ProblemState {
    confirmed_severity: None,
    pending_severity: Some(Status::Critical),
    pending_attempts: 1,
};

#[test]
fn the_repeat_delay_retries_pending_problems_and_pending_severity_changes() {
    let check = check(300, 30);
    let confirmed = ProblemState {
        confirmed_severity: Some(Status::Warning),
        ..ProblemState::default()
    };
    let changing = ProblemState {
        confirmed_severity: confirmed.confirmed_severity,
        ..PENDING
    };
    for (problem, expected) in [
        (ProblemState::default(), 300),
        (PENDING, 60),
        (changing, 60),
        (confirmed, 300),
    ] {
        assert_eq!(
            Cadence::from(problem).interval(&check, 60),
            Duration::from_secs(expected),
            "{problem:?}"
        );
    }
}

#[test]
fn checks_fall_due_one_interval_after_their_last_result_or_one_retry_interval_while_pending() {
    let check = check(300, 30);
    let result = result_at("2026-11-20T12:00:00Z");
    let now = at("2026-11-20T12:00:30Z");
    assert_eq!(
        freshness(
            &check,
            60,
            Some((&result, ProblemState::default())),
            None,
            now
        ),
        Freshness::Current(at("2026-11-20T12:05:00Z"))
    );
    assert_eq!(
        freshness(&check, 60, Some((&result, PENDING)), None, now),
        Freshness::Current(at("2026-11-20T12:01:00Z"))
    );
}

#[test]
fn unreadable_and_future_results_are_stale_without_a_known_due_time() {
    let check = check(300, 30);
    let now = at("2026-11-20T12:00:00Z");
    let problem = ProblemState::default();
    for timestamp in ["unreadable", "2026-11-20T12:00:01Z"] {
        let result = result_at(timestamp);
        assert_eq!(interval_due_at(&check, 60, &result, problem, now), None);
        assert_eq!(
            freshness(&check, 60, Some((&result, problem)), Some(now), now),
            Freshness::Overdue(None),
            "a saved result with timestamp {timestamp:?} is stale even if a first due time is known"
        );
    }
}

#[test]
fn a_check_without_a_result_uses_its_first_due_time_if_known() {
    let check = check(300, 30);
    let first_due = at("2026-11-20T12:00:00Z");
    let deadline = at("2026-11-20T12:01:00Z");
    assert_eq!(
        freshness(&check, 60, None, None, deadline),
        Freshness::NotYetChecked
    );
    assert_eq!(
        freshness(&check, 60, None, Some(first_due), deadline),
        Freshness::Current(first_due),
        "a first check gets the same timeout and grace as later checks"
    );
    assert_eq!(
        freshness(
            &check,
            60,
            None,
            Some(first_due),
            at("2026-11-20T12:01:01Z")
        ),
        Freshness::Missing(first_due)
    );
}

#[test]
fn a_check_is_overdue_only_after_its_own_deadline_and_scheduling_grace_have_passed() {
    let due = at("2026-11-20T12:05:00Z");
    let check = check(300, 30);
    let result = result_at("2026-11-20T12:00:00Z");
    let saved = Some((&result, ProblemState::default()));
    assert_eq!(
        freshness(&check, 60, saved, None, at("2026-11-20T12:04:00Z")),
        Freshness::Current(due)
    );
    // A due check may be running for up to its timeout, plus 30 s grace.
    assert_eq!(
        freshness(&check, 60, saved, None, due + TimeDelta::seconds(60)),
        Freshness::Current(due)
    );
    assert_eq!(
        freshness(&check, 60, saved, None, due + TimeDelta::seconds(61)),
        Freshness::Overdue(Some(due))
    );
}

#[test]
fn startup_runs_new_checks_now_waits_for_future_ones_and_spreads_overdue_ones_oldest_first() {
    let now = at("2026-11-20T12:00:00Z");
    let delays = startup_delays(
        vec![
            ("new", None),
            ("future", Some(at("2026-11-20T12:02:00Z"))),
            ("recent", Some(at("2026-11-20T11:59:00Z"))),
            ("oldest", Some(at("2026-11-20T08:00:00Z"))),
            ("due-now", Some(now)),
            ("older", Some(at("2026-11-20T11:00:00Z"))),
        ],
        now,
        Duration::from_secs(60),
    );
    assert_eq!(
        delays,
        [
            ("new", Duration::ZERO),
            ("future", Duration::from_secs(120)),
            ("recent", Duration::from_secs(30)),
            ("oldest", Duration::ZERO),
            ("due-now", Duration::from_secs(45)),
            ("older", Duration::from_secs(15)),
        ]
    );
}

#[test]
fn without_a_startup_period_overdue_checks_are_all_eligible_immediately() {
    let now = at("2026-11-20T12:00:00Z");
    let delays = startup_delays(
        vec![
            (1, Some(at("2026-11-20T11:00:00Z"))),
            (2, Some(at("2026-11-20T10:00:00Z"))),
        ],
        now,
        Duration::ZERO,
    );
    assert_eq!(delays, [(1, Duration::ZERO), (2, Duration::ZERO)]);
}

#[test]
fn checks_falling_due_during_the_startup_period_queue_behind_overdue_ones_but_never_run_early() {
    let now = at("2026-11-20T12:00:00Z");
    let delays = startup_delays(
        vec![
            ("late", Some(at("2026-11-20T12:00:50Z"))),
            ("soon", Some(at("2026-11-20T12:00:10Z"))),
            ("overdue", Some(at("2026-11-20T11:59:00Z"))),
            ("oldest", Some(at("2026-11-20T11:00:00Z"))),
            ("after-startup", Some(at("2026-11-20T12:02:00Z"))),
        ],
        now,
        Duration::from_secs(60),
    );
    assert_eq!(
        delays,
        [
            ("late", Duration::from_secs(50)),
            ("soon", Duration::from_secs(30)),
            ("overdue", Duration::from_secs(15)),
            ("oldest", Duration::ZERO),
            ("after-startup", Duration::from_secs(120)),
        ]
    );
}

#[test]
fn a_weekday_work_hours_check_due_at_the_weekend_falls_due_when_its_window_next_opens() {
    let check = windowed(check(300, 30), "Europe/London", &WEEKDAYS, "09:00", "17:30");
    let saturday = at("2026-11-21T10:00:00Z");
    let monday = at("2026-11-23T09:00:00Z");
    let result = result_at("2026-11-20T17:28:00Z");
    assert_eq!(
        freshness(
            &check,
            60,
            Some((&result, ProblemState::default())),
            None,
            saturday
        ),
        Freshness::Current(monday),
        "a closed window alone never makes a saved result overdue"
    );
    assert_eq!(
        freshness(&check, 60, None, Some(at("2026-11-20T17:33:00Z")), saturday),
        Freshness::Current(monday),
        "a first check due outside its window also waits for it to open"
    );
}

#[test]
fn checks_sharing_a_window_keep_independent_intervals_including_daily_weekday_checks() {
    let frequent = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "10:00");
    let certificate = windowed(check(86_400, 30), "UTC", &WEEKDAYS, "09:00", "10:00");
    for (check, finished, due) in [
        (&frequent, "2026-11-19T09:00:05Z", "2026-11-19T09:05:05Z"),
        (&certificate, "2026-11-19T09:00:05Z", "2026-11-20T09:00:05Z"),
        (&certificate, "2026-11-20T09:00:05Z", "2026-11-23T09:00:00Z"),
        (&frequent, "2026-11-19T09:58:00Z", "2026-11-20T09:00:00Z"),
    ] {
        let result = result_at(finished);
        let saved = Some((&result, ProblemState::default()));
        assert_eq!(
            freshness(check, 60, saved, None, at(finished)),
            Freshness::Current(at(due)),
            "with a {} second interval and last result at {finished}",
            check.interval_seconds
        );
    }
}

#[test]
fn retries_wait_for_the_window_to_reopen() {
    let check = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "17:30");
    let late = result_at("2026-11-19T17:29:30Z");
    assert_eq!(
        freshness(
            &check,
            60,
            Some((&late, PENDING)),
            None,
            at("2026-11-19T17:29:30Z")
        ),
        Freshness::Current(at("2026-11-20T09:00:00Z"))
    );
    let midday = result_at("2026-11-19T12:00:00Z");
    assert_eq!(
        freshness(
            &check,
            60,
            Some((&midday, PENDING)),
            None,
            at("2026-11-19T12:00:00Z")
        ),
        Freshness::Current(at("2026-11-19T12:01:00Z")),
        "retries inside an open window keep retry timing"
    );
}

#[test]
fn a_restart_outside_the_window_waits_for_it_to_open_instead_of_catching_up() {
    let check = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "17:30");
    let saturday = at("2026-11-21T12:00:00Z");
    let monday = at("2026-11-23T09:00:00Z");
    let result = result_at("2026-11-20T17:28:00Z");
    // The restored schedule keeps the interval deadline; the window defers the
    // start, so a later window change still applies from that deadline.
    let interval_due = interval_due_at(&check, 60, &result, ProblemState::default(), saturday);
    assert_eq!(interval_due, Some(at("2026-11-20T17:33:00Z")));
    assert_eq!(
        startup_delays(
            vec![("work", interval_due)],
            saturday,
            Duration::from_secs(60)
        ),
        [("work", Duration::ZERO)]
    );
    assert_eq!(
        window_wait(&check, saturday),
        (monday - saturday).to_std().unwrap()
    );
}

#[test]
fn a_check_missed_inside_its_window_is_overdue_but_waits_for_the_window_to_run() {
    let check = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "17:30");
    let saturday = at("2026-11-21T12:00:00Z");
    let missed = at("2026-11-20T12:05:00Z");
    let result = result_at("2026-11-20T12:00:00Z");
    let saved = Some((&result, ProblemState::default()));
    assert_eq!(
        freshness(&check, 60, saved, None, saturday),
        Freshness::Overdue(Some(missed))
    );
    assert_eq!(
        eligible_from(&check, saturday),
        at("2026-11-23T09:00:00Z"),
        "catch-up and new checks start only when the window opens"
    );
}

#[test]
fn windows_keep_their_local_hours_across_daylight_saving_changes() {
    let check = windowed(check(300, 30), "Europe/London", &WEEKDAYS, "09:00", "17:30");
    // British Summer Time starts on Sunday 29 March 2026 and ends on Sunday
    // 25 October 2026.
    assert_eq!(
        eligible_from(&check, at("2026-03-27T18:00:00Z")),
        at("2026-03-30T08:00:00Z"),
        "Monday 09:00 BST"
    );
    assert_eq!(
        eligible_from(&check, at("2026-10-23T18:00:00Z")),
        at("2026-10-26T09:00:00Z"),
        "Monday 09:00 GMT"
    );
    assert_eq!(
        eligible_from(&check, at("2026-03-30T16:15:00Z")),
        at("2026-03-30T16:15:00Z"),
        "17:15 BST is still open"
    );
    assert_eq!(
        eligible_from(&check, at("2026-03-30T16:45:00Z")),
        at("2026-03-31T08:00:00Z"),
        "17:45 BST is closed until Tuesday 09:00 BST"
    );
}

#[test]
fn a_window_starting_in_a_skipped_hour_opens_when_local_time_resumes() {
    let check = windowed(
        check(300, 30),
        "Europe/London",
        &[Weekday::Sun],
        "01:30",
        "03:00",
    );
    // Clocks go from 01:00 GMT straight to 02:00 BST.
    assert_eq!(
        eligible_from(&check, at("2026-03-28T12:00:00Z")),
        at("2026-03-29T01:00:00Z")
    );
}

#[test]
fn a_window_in_a_repeated_hour_opens_on_each_occurrence() {
    let check = windowed(
        check(300, 30),
        "Europe/London",
        &[Weekday::Sun],
        "01:30",
        "02:00",
    );
    // Local 01:00-02:00 happens first in BST (00:00-01:00 UTC), then in GMT.
    assert_eq!(
        eligible_from(&check, at("2026-10-24T12:00:00Z")),
        at("2026-10-25T00:30:00Z")
    );
    assert_eq!(
        eligible_from(&check, at("2026-10-25T01:10:00Z")),
        at("2026-10-25T01:30:00Z")
    );
}

#[test]
fn a_window_reopens_when_clocks_go_back_into_the_middle_of_its_range() {
    let check = windowed(
        check(300, 30),
        "Europe/London",
        &[Weekday::Sun],
        "00:30",
        "01:30",
    );
    // 01:30 BST closes the range at 00:30 UTC; at 01:00 UTC clocks go back to
    // 01:00 GMT, inside the range again, without passing its start.
    assert_eq!(
        eligible_from(&check, at("2026-10-25T00:40:00Z")),
        at("2026-10-25T01:00:00Z")
    );
}

#[test]
fn a_window_skipped_entirely_by_the_spring_clock_change_opens_the_following_week() {
    let check = windowed(
        check(300, 30),
        "Europe/London",
        &[Weekday::Sun],
        "01:10",
        "01:20",
    );
    // On 29 March 2026 local time jumps from 01:00 GMT to 02:00 BST.
    for from in [
        "2026-03-23T12:00:00Z",
        "2026-03-28T12:00:00Z",
        "2026-03-29T00:30:00Z",
        "2026-03-29T00:59:59Z",
    ] {
        assert_eq!(
            eligible_from(&check, at(from)),
            at("2026-04-05T00:10:00Z"),
            "Sunday 5 April 01:10 BST"
        );
    }
}
