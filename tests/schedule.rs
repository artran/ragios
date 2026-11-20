use chrono::{DateTime, TimeDelta, Utc};
use ragios::{
    config::Check,
    runner::Status,
    schedule::{due_at, is_overdue, startup_delays},
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
