use ragios::{
    runner::{CheckResult, Status},
    store::{ProblemState, Store, TransitionKind},
};
use tempfile::TempDir;

fn result(status: Status) -> CheckResult {
    CheckResult {
        status,
        output: "probe output".into(),
        started_at: "2026-11-20T12:00:00.000Z".into(),
        finished_at: "2026-11-20T12:00:01.000Z".into(),
        duration_ms: 1000,
    }
}

async fn record(path: &std::path::Path, status: Status, attempts: u32) -> ProblemState {
    let store = Store::open(path).unwrap();
    let state = store
        .save("host".into(), "service".into(), result(status), attempts)
        .await
        .unwrap();
    drop(store);
    let reopened = Store::open(path).unwrap();
    let latest = reopened.latest().await.unwrap();
    let saved = &latest[&("host".into(), "service".into())];
    assert_eq!(saved.result.status, status);
    assert_eq!(saved.problem, state);
    state
}

#[tokio::test]
async fn mixed_failure_severities_confirm_the_threshold_results_severity() {
    use TransitionKind::{Confirmed, Pending};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    record(&path, Status::Warning, 3).await;
    assert_eq!(
        record(&path, Status::Critical, 3).await,
        ProblemState {
            confirmed_severity: None,
            pending_severity: Some(Status::Critical),
            pending_attempts: 2,
        }
    );
    assert_eq!(
        record(&path, Status::Unknown, 3).await,
        ProblemState {
            confirmed_severity: Some(Status::Unknown),
            pending_severity: None,
            pending_attempts: 0,
        }
    );
    let store = Store::open(&path).unwrap();
    let transitions = store
        .transitions("host".into(), "service".into())
        .await
        .unwrap();
    assert_eq!(
        transitions.iter().map(|t| t.kind).collect::<Vec<_>>(),
        [Pending, Pending, Confirmed]
    );
    assert_eq!(transitions[2].result_status, Status::Unknown);
    assert_eq!(
        transitions[2].state.confirmed_severity,
        Some(Status::Unknown)
    );
}

#[tokio::test]
async fn severity_changes_require_an_uninterrupted_streak_at_the_same_new_severity() {
    use Status::{Critical as C, Unknown as U, Warning as W};
    use TransitionKind::{
        Confirmed, Pending, SeverityChangeCancelled, SeverityChangePending, SeverityChanged,
    };
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    for _ in 0..3 {
        record(&path, W, 3).await;
    }
    // Candidate changes restart at one; returning to WARNING cancels the candidate.
    for (status, confirmed, pending, attempts) in [
        (W, W, None, 0),
        (C, W, Some(C), 1),
        (C, W, Some(C), 2),
        (U, W, Some(U), 1),
        (U, W, Some(U), 2),
        (C, W, Some(C), 1),
        (W, W, None, 0),
        (U, W, Some(U), 1),
        (U, W, Some(U), 2),
        (U, U, None, 0),
        (U, U, None, 0),
    ] {
        assert_eq!(
            record(&path, status, 3).await,
            ProblemState {
                confirmed_severity: Some(confirmed),
                pending_severity: pending,
                pending_attempts: attempts,
            },
            "result {status:?}"
        );
    }
    let store = Store::open(&path).unwrap();
    let transitions = store
        .transitions("host".into(), "service".into())
        .await
        .unwrap();
    assert_eq!(
        transitions.iter().map(|t| t.kind).collect::<Vec<_>>(),
        [
            Pending,
            Pending,
            Confirmed,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChangeCancelled,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChanged,
        ]
    );
    assert!(transitions.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[tokio::test]
async fn ok_clears_any_problem_without_recording_initial_or_repeated_recovery() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    assert_eq!(record(&path, Status::Ok, 3).await, ProblemState::default());
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .transitions("host".into(), "service".into())
            .await
            .unwrap()
            .is_empty()
    );
    drop(store);

    for failures in [
        vec![Status::Critical],
        vec![Status::Warning; 3],
        vec![
            Status::Unknown,
            Status::Unknown,
            Status::Unknown,
            Status::Critical,
        ],
    ] {
        for failure in failures {
            record(&path, failure, 3).await;
        }
        assert_eq!(record(&path, Status::Ok, 3).await, ProblemState::default());
        let store = Store::open(&path).unwrap();
        let transitions = store
            .transitions("host".into(), "service".into())
            .await
            .unwrap();
        let recovery = transitions.last().unwrap();
        assert_eq!(recovery.kind, TransitionKind::Recovery);
        assert_eq!(recovery.state, ProblemState::default());
        assert_eq!(recovery.result_status, Status::Ok);
        let count = transitions.len();
        drop(store);
        record(&path, Status::Ok, 3).await;
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .transitions("host".into(), "service".into())
                .await
                .unwrap()
                .len(),
            count
        );
        drop(store);
        // A subsequent failure starts a fresh streak, even after a confirmed recovery.
        assert_eq!(record(&path, Status::Warning, 3).await.pending_attempts, 1);
        record(&path, Status::Ok, 3).await;
    }
}

#[tokio::test]
async fn non_default_thresholds_apply_to_initial_failures_and_severity_changes() {
    for threshold in [1, 2] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.sqlite3");
        for severity in [Status::Critical, Status::Warning, Status::Unknown] {
            for attempt in 1..=threshold {
                let state = record(&path, severity, threshold).await;
                if attempt < threshold {
                    assert_eq!(state.pending_severity, Some(severity));
                    assert_eq!(state.pending_attempts, 1);
                    assert_ne!(state.confirmed_severity, Some(severity));
                    assert!(state.needs_retry());
                } else {
                    assert_eq!(
                        state,
                        ProblemState {
                            confirmed_severity: Some(severity),
                            pending_severity: None,
                            pending_attempts: 0,
                        }
                    );
                    assert!(!state.needs_retry());
                }
            }
        }
    }
}

#[tokio::test]
async fn service_problem_streaks_and_transitions_are_independent() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
    for (host, service, severity) in [
        ("one", "shared", Status::Warning),
        ("two", "shared", Status::Critical),
        ("one", "other", Status::Unknown),
    ] {
        store
            .save(host.into(), service.into(), result(severity), 2)
            .await
            .unwrap();
    }
    store
        .save("one".into(), "shared".into(), result(Status::Critical), 2)
        .await
        .unwrap();
    let saved = store.latest().await.unwrap();
    assert_eq!(saved.len(), 3);
    assert_eq!(
        saved[&("one".into(), "shared".into())]
            .problem
            .confirmed_severity,
        Some(Status::Critical)
    );
    assert_eq!(
        saved[&("two".into(), "shared".into())]
            .problem
            .pending_attempts,
        1
    );
    assert_eq!(
        saved[&("one".into(), "other".into())]
            .problem
            .pending_severity,
        Some(Status::Unknown)
    );
    assert_eq!(
        store
            .transitions("one".into(), "shared".into())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .transitions("two".into(), "shared".into())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn pre_lifecycle_databases_keep_their_results_without_inventing_confirmed_problems() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    // Fixture in the schema shipped before problem confirmation existed.
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE latest_results (
        host_id TEXT NOT NULL, service_id TEXT NOT NULL, status INTEGER NOT NULL,
        output TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT NOT NULL,
        duration_ms INTEGER NOT NULL, PRIMARY KEY (host_id, service_id));
        INSERT INTO latest_results VALUES
        ('host', 'service', 2, 'legacy failure', 'start', 'finish', 1),
        ('host', 'healthy', 0, 'legacy success', 'start', 'finish', 1);",
        )
        .unwrap();
    drop(legacy);
    let store = Store::open(&path).unwrap();
    let saved = store.latest().await.unwrap();
    let failure = &saved[&("host".into(), "service".into())];
    assert_eq!(failure.result.output, "legacy failure");
    assert_eq!(
        failure.problem,
        ProblemState {
            confirmed_severity: None,
            pending_severity: Some(Status::Critical),
            pending_attempts: 1,
        }
    );
    assert_eq!(
        saved[&("host".into(), "healthy".into())].problem,
        ProblemState::default()
    );
    assert!(
        store
            .transitions("host".into(), "service".into())
            .await
            .unwrap()
            .is_empty()
    );
    drop(store);
    assert_eq!(record(&path, Status::Warning, 3).await.pending_attempts, 2);
    assert_eq!(
        record(&path, Status::Unknown, 3).await.confirmed_severity,
        Some(Status::Unknown)
    );
}

#[tokio::test]
async fn first_failure_is_persisted_as_a_pending_problem() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    let store = Store::open(&path).unwrap();
    let state = store
        .save("host".into(), "service".into(), result(Status::Warning), 3)
        .await
        .unwrap();
    let expected = ProblemState {
        confirmed_severity: None,
        pending_severity: Some(Status::Warning),
        pending_attempts: 1,
    };
    assert_eq!(state, expected);
    drop(store);

    let reopened = Store::open(&path).unwrap();
    let latest = reopened.latest().await.unwrap();
    let saved = &latest[&("host".into(), "service".into())];
    assert_eq!(saved.result.status, Status::Warning);
    assert_eq!(saved.problem, expected);
    let transitions = reopened
        .transitions("host".into(), "service".into())
        .await
        .unwrap();
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].kind, TransitionKind::Pending);
    assert_eq!(transitions[0].state, expected);
    assert_eq!(transitions[0].occurred_at, "2026-11-20T12:00:01.000Z");
}
