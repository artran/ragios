use chrono::{DateTime, TimeDelta, Utc, Weekday};
use ragios::{
    config::{Check, CheckWindow, WindowRange},
    runner::Status,
    schedule::{due_at, eligible_from, interval_due_at, is_overdue, startup_delays, window_wait},
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

const PENDING: ProblemState = ProblemState {
    confirmed_severity: None,
    pending_severity: Some(Status::Critical),
    pending_attempts: 1,
};

#[test]
fn checks_fall_due_one_interval_after_their_last_result_or_one_retry_interval_while_pending() {
    let finished = at("2026-11-20T12:00:00Z");
    let now = at("2026-11-20T12:00:30Z");
    assert_eq!(
        due_at(&check(300, 30), 60, finished, ProblemState::default(), now),
        Some(at("2026-11-20T12:05:00Z"))
    );
    assert_eq!(
        due_at(&check(300, 30), 60, finished, PENDING, now),
        Some(at("2026-11-20T12:01:00Z"))
    );
    let confirmed = ProblemState {
        confirmed_severity: Some(Status::Critical),
        ..ProblemState::default()
    };
    assert_eq!(
        due_at(&check(300, 30), 60, finished, confirmed, now),
        Some(at("2026-11-20T12:05:00Z")),
        "a stable confirmed problem uses the normal interval"
    );
    assert_eq!(
        due_at(
            &check(300, 30),
            60,
            finished,
            ProblemState::default(),
            at("2026-11-20T11:59:59Z")
        ),
        None,
        "a result from the future means the clock moved back"
    );
}

#[test]
fn a_check_is_overdue_only_after_its_own_deadline_and_scheduling_grace_have_passed() {
    let due = at("2026-11-20T12:05:00Z");
    let check = check(300, 30);
    assert!(!is_overdue(&check, due, at("2026-11-20T12:04:00Z")));
    // A due check may be running for up to its timeout, plus 30 s grace.
    assert!(!is_overdue(&check, due, due + TimeDelta::seconds(60)));
    assert!(is_overdue(&check, due, due + TimeDelta::seconds(61)));
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
    // Friday 17:00 in London (GMT in November).
    let finished = at("2026-11-20T17:00:00Z");
    let check = windowed(check(300, 30), "Europe/London", &WEEKDAYS, "09:00", "17:30");
    assert_eq!(
        due_at(&check, 60, finished, ProblemState::default(), finished),
        Some(at("2026-11-20T17:05:00Z")),
        "a check due inside its window runs on its interval"
    );
    let finished = at("2026-11-20T17:28:00Z");
    let saturday = at("2026-11-21T10:00:00Z");
    let monday = at("2026-11-23T09:00:00Z");
    assert_eq!(
        due_at(&check, 60, finished, ProblemState::default(), saturday),
        Some(monday)
    );
    assert!(
        !is_overdue(&check, monday, saturday),
        "a closed window alone never makes a check overdue"
    );
}

#[test]
fn checks_sharing_a_window_keep_independent_intervals_including_daily_weekday_checks() {
    let finished = at("2026-11-19T09:00:05Z");
    let frequent = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "10:00");
    let certificate = windowed(check(86_400, 30), "UTC", &WEEKDAYS, "09:00", "10:00");
    let state = ProblemState::default();
    assert_eq!(
        due_at(&frequent, 60, finished, state, finished),
        Some(at("2026-11-19T09:05:05Z"))
    );
    assert_eq!(
        due_at(&certificate, 60, finished, state, finished),
        Some(at("2026-11-20T09:00:05Z")),
        "Thursday's certificate check runs again on Friday"
    );
    let friday = at("2026-11-20T09:00:05Z");
    assert_eq!(
        due_at(&certificate, 60, friday, state, friday),
        Some(at("2026-11-23T09:00:00Z")),
        "Friday's certificate check skips the weekend"
    );
    let late = at("2026-11-19T09:58:00Z");
    assert_eq!(
        due_at(&frequent, 60, late, state, late),
        Some(at("2026-11-20T09:00:00Z")),
        "a due time after the window closes waits for the next day's window"
    );
}

#[test]
fn retries_wait_for_the_window_to_reopen() {
    let check = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "17:30");
    let finished = at("2026-11-19T17:29:30Z");
    assert_eq!(
        due_at(&check, 60, finished, PENDING, finished),
        Some(at("2026-11-20T09:00:00Z"))
    );
    let finished = at("2026-11-19T12:00:00Z");
    assert_eq!(
        due_at(&check, 60, finished, PENDING, finished),
        Some(at("2026-11-19T12:01:00Z")),
        "retries inside an open window keep retry timing"
    );
}

#[test]
fn a_restart_outside_the_window_waits_for_it_to_open_instead_of_catching_up() {
    let check = windowed(check(300, 30), "UTC", &WEEKDAYS, "09:00", "17:30");
    let saturday = at("2026-11-21T12:00:00Z");
    let monday = at("2026-11-23T09:00:00Z");
    let finished = at("2026-11-20T17:28:00Z");
    let state = ProblemState::default();
    assert_eq!(due_at(&check, 60, finished, state, saturday), Some(monday));
    assert!(!is_overdue(&check, monday, saturday));
    // The restored schedule keeps the interval deadline; the window defers the
    // start, so a later window change still applies from that deadline.
    let interval_due = interval_due_at(&check, 60, finished, state, saturday);
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
    let due = due_at(
        &check,
        60,
        at("2026-11-20T12:00:00Z"),
        ProblemState::default(),
        saturday,
    );
    assert_eq!(due, Some(missed));
    assert!(is_overdue(&check, missed, saturday));
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
