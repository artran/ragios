use ragios::{
    config::Probe,
    runner::{CheckResult, Reachability, Status},
    store::{ProblemState, Store, TransitionKind},
};
use tempfile::TempDir;

fn result(status: Status) -> CheckResult {
    CheckResult {
        status,
        output: "host probe output".into(),
        started_at: "2026-11-20T12:00:00.000Z".into(),
        finished_at: "2026-11-20T12:00:01.000Z".into(),
        duration_ms: 1000,
    }
}

#[tokio::test]
async fn host_exit_statuses_map_to_assessments_without_losing_plugin_status() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
    for (status, assessment, failure) in [
        (Status::Ok, Reachability::Reachable, None),
        (Status::Warning, Reachability::Reachable, None),
        (Status::Critical, Reachability::Down, Some(Status::Critical)),
        (
            Status::Unknown,
            Reachability::Undetermined,
            Some(Status::Unknown),
        ),
    ] {
        let state = store
            .save_host("host".into(), Probe::Ping, result(status), 1)
            .await
            .unwrap();
        assert_eq!(state.confirmed_severity, failure);
        assert!(!state.needs_retry());
        let saved = store.latest_hosts().await.unwrap();
        assert_eq!(saved["host"].result.status, status);
        assert_eq!(saved["host"].reachability(), assessment);
        assert_eq!(
            saved["host"].is_confirmed_down(),
            status == Status::Critical
        );
    }
}

#[tokio::test]
async fn mixed_host_failures_confirm_then_changes_require_a_matching_streak_across_restarts() {
    use Status::{Critical, Unknown, Warning};
    use TransitionKind::{
        Confirmed, Pending, Recovery, SeverityChangeCancelled, SeverityChangePending,
        SeverityChanged,
    };
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    let store = Store::open(&path).unwrap();
    store
        .save("host".into(), "service".into(), result(Critical), 1)
        .await
        .unwrap();
    drop(store);
    for (status, confirmed, pending, attempts) in [
        (Unknown, None, Some(Unknown), 1),
        (Critical, None, Some(Critical), 2),
        (Unknown, Some(Unknown), None, 0),
        (Critical, Some(Unknown), Some(Critical), 1),
        (Critical, Some(Unknown), Some(Critical), 2),
        (Unknown, Some(Unknown), None, 0),
        (Critical, Some(Unknown), Some(Critical), 1),
        (Critical, Some(Unknown), Some(Critical), 2),
        (Critical, Some(Critical), None, 0),
        (Unknown, Some(Critical), Some(Unknown), 1),
        (Unknown, Some(Critical), Some(Unknown), 2),
        (Unknown, Some(Unknown), None, 0),
        (Critical, Some(Unknown), Some(Critical), 1),
        (Warning, None, None, 0),
        (Critical, None, Some(Critical), 1),
        (Warning, None, None, 0),
    ] {
        let store = Store::open(&path).unwrap();
        let state = store
            .save_host("host".into(), Probe::Ping, result(status), 3)
            .await
            .unwrap();
        assert_eq!(
            state,
            ProblemState {
                confirmed_severity: confirmed,
                pending_severity: pending,
                pending_attempts: attempts
            }
        );
        assert_eq!(state.needs_retry(), pending.is_some());
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let hosts = reopened.latest_hosts().await.unwrap();
        assert_eq!(hosts["host"].problem, state);
        assert_eq!(hosts["host"].result.status, status);
        assert_eq!(
            hosts["host"].is_confirmed_down(),
            confirmed == Some(Critical)
        );
        let services = reopened.latest().await.unwrap();
        assert_eq!(
            services[&("host".into(), "service".into())]
                .problem
                .confirmed_severity,
            Some(Critical)
        );
    }
    let store = Store::open(&path).unwrap();
    let transitions = store.host_transitions("host".into()).await.unwrap();
    assert_eq!(
        transitions.iter().map(|t| t.kind).collect::<Vec<_>>(),
        [
            Pending,
            Pending,
            Confirmed,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChangeCancelled,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChanged,
            SeverityChangePending,
            SeverityChangePending,
            SeverityChanged,
            SeverityChangePending,
            Recovery,
            Pending,
            Recovery,
        ]
    );
    assert_eq!(transitions[13].result_status, Warning);
    assert!(transitions.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[tokio::test]
async fn either_reachable_status_immediately_clears_pending_or_confirmed_host_problems() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
    for reachable in [Status::Ok, Status::Warning] {
        assert_eq!(
            store
                .save_host("host".into(), Probe::Tcp, result(reachable), 2)
                .await
                .unwrap(),
            ProblemState::default()
        );
        for failure in [Status::Critical, Status::Unknown] {
            for count in [1, 2, 3] {
                for _ in 0..count {
                    store
                        .save_host("host".into(), Probe::Tcp, result(failure), 2)
                        .await
                        .unwrap();
                }
                assert_eq!(
                    store
                        .save_host("host".into(), Probe::Tcp, result(reachable), 2)
                        .await
                        .unwrap(),
                    ProblemState::default()
                );
                assert!(!store.latest_hosts().await.unwrap()["host"].is_confirmed_down());
                assert_eq!(
                    store
                        .host_transitions("host".into())
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .kind,
                    TransitionKind::Recovery
                );
            }
        }
    }
}

#[tokio::test]
async fn existing_service_lifecycle_state_and_transition_ids_survive_the_host_schema_upgrade() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy.execute_batch(
        "CREATE TABLE latest_results (
            host_id TEXT NOT NULL, service_id TEXT NOT NULL, status INTEGER NOT NULL,
            output TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT NOT NULL,
            duration_ms INTEGER NOT NULL, PRIMARY KEY (host_id, service_id));
         CREATE TABLE problem_states (
            host_id TEXT NOT NULL, service_id TEXT NOT NULL, confirmed_severity INTEGER,
            pending_severity INTEGER, pending_attempts INTEGER NOT NULL,
            PRIMARY KEY (host_id, service_id));
         CREATE TABLE problem_transitions (
            id INTEGER PRIMARY KEY, host_id TEXT NOT NULL, service_id TEXT NOT NULL,
            occurred_at TEXT NOT NULL, result_status INTEGER NOT NULL, kind TEXT NOT NULL,
            confirmed_severity INTEGER, pending_severity INTEGER, pending_attempts INTEGER NOT NULL);
         INSERT INTO latest_results VALUES ('host', 'reachability', 3, 'old service output', 'start', 'finish', 10);
         INSERT INTO problem_states VALUES ('host', 'reachability', 2, 3, 2);
         INSERT INTO problem_transitions VALUES (7, 'host', 'reachability', 'finish', 3, 'severity_change_pending', 2, 3, 2);"
    ).unwrap();
    drop(legacy);
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        assert!(store.latest_hosts().await.unwrap().is_empty());
        let services = store.latest().await.unwrap();
        let service = &services[&("host".into(), "reachability".into())];
        assert_eq!(service.result.output, "old service output");
        assert_eq!(
            service.problem,
            ProblemState {
                confirmed_severity: Some(Status::Critical),
                pending_severity: Some(Status::Unknown),
                pending_attempts: 2
            }
        );
        let transitions = store
            .transitions("host".into(), "reachability".into())
            .await
            .unwrap();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].id, 7);
        assert_eq!(transitions[0].kind, TransitionKind::SeverityChangePending);
        assert_eq!(transitions[0].state, service.problem);
    }
    let store = Store::open(&path).unwrap();
    store
        .save_host("host".into(), Probe::Tcp, result(Status::Critical), 3)
        .await
        .unwrap();
    let service = store
        .save(
            "host".into(),
            "reachability".into(),
            result(Status::Unknown),
            3,
        )
        .await
        .unwrap();
    assert_eq!(service.confirmed_severity, Some(Status::Unknown));
    assert!(!service.needs_retry());
    assert_eq!(
        store.latest_hosts().await.unwrap()["host"]
            .problem
            .pending_attempts,
        1
    );
    let transitions = store
        .transitions("host".into(), "reachability".into())
        .await
        .unwrap();
    assert_eq!(transitions.len(), 2);
    assert!(transitions[1].id > 7);
    assert_eq!(transitions[1].kind, TransitionKind::SeverityChanged);
}

#[tokio::test]
async fn host_pending_problems_and_service_results_persist_independently() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.sqlite3");
    let store = Store::open(&path).unwrap();
    store
        .save(
            "host".into(),
            "reachability".into(),
            result(Status::Warning),
            3,
        )
        .await
        .unwrap();
    let state = store
        .save_host("host".into(), Probe::Tcp, result(Status::Critical), 3)
        .await
        .unwrap();
    assert_eq!(
        state,
        ProblemState {
            confirmed_severity: None,
            pending_severity: Some(Status::Critical),
            pending_attempts: 1,
        }
    );
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let hosts = reopened.latest_hosts().await.unwrap();
    assert_eq!(hosts["host"].probe, Probe::Tcp);
    assert_eq!(hosts["host"].result.output, "host probe output");
    assert_eq!(hosts["host"].problem, state);
    let services = reopened.latest().await.unwrap();
    let service = &services[&("host".into(), "reachability".into())];
    assert_eq!(service.result.status, Status::Warning);
    assert_eq!(service.problem.pending_severity, Some(Status::Warning));
    assert_eq!(
        reopened
            .host_transitions("host".into())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened
            .transitions("host".into(), "reachability".into())
            .await
            .unwrap()
            .len(),
        1
    );
}
