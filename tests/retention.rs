use chrono::{DateTime, TimeDelta, Utc};
use ragios::{
    config::Probe,
    runner::{CheckResult, Status},
    store::{ProblemState, Store, TransitionKind},
};
use tempfile::TempDir;

fn now() -> DateTime<Utc> {
    "2026-11-20T12:00:00Z".parse().unwrap()
}

fn result_at(status: Status, finished: DateTime<Utc>) -> CheckResult {
    let stamp = |at: DateTime<Utc>| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    CheckResult {
        status,
        output: "probe output".into(),
        started_at: stamp(finished - TimeDelta::seconds(1)),
        finished_at: stamp(finished),
        duration_ms: 1000,
    }
}

#[tokio::test]
async fn state_changes_older_than_thirty_days_are_pruned_without_losing_current_state() {
    use TransitionKind::{Confirmed, SeverityChangePending};
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
    let old = now() - TimeDelta::days(30) - TimeDelta::seconds(1);
    let boundary = now() - TimeDelta::days(30);
    for at in [old, old, boundary] {
        store
            .save(
                "host".into(),
                "service".into(),
                result_at(Status::Warning, at),
                3,
            )
            .await
            .unwrap();
    }
    store
        .save(
            "host".into(),
            "service".into(),
            result_at(Status::Critical, now() - TimeDelta::days(1)),
            3,
        )
        .await
        .unwrap();
    // A long-standing confirmed problem whose every state change is old.
    for _ in 0..3 {
        store
            .save_host(
                "host".into(),
                Probe::Ping,
                result_at(Status::Critical, old),
                3,
            )
            .await
            .unwrap();
    }

    assert_eq!(store.prune_transitions(now()).await.unwrap(), 5);

    let kinds = store
        .transitions("host".into(), "service".into())
        .await
        .unwrap()
        .iter()
        .map(|transition| transition.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds, [Confirmed, SeverityChangePending]);
    assert!(
        store
            .host_transitions("host".into())
            .await
            .unwrap()
            .is_empty()
    );
    let latest = store.latest().await.unwrap();
    let service = &latest[&("host".into(), "service".into())];
    assert_eq!(service.result.status, Status::Critical);
    assert_eq!(
        service.problem,
        ProblemState {
            confirmed_severity: Some(Status::Warning),
            pending_severity: Some(Status::Critical),
            pending_attempts: 1,
        }
    );
    let hosts = store.latest_hosts().await.unwrap();
    assert!(hosts["host"].is_confirmed_down());
    assert_eq!(
        hosts["host"].result.finished_at,
        result_at(Status::Critical, old).finished_at
    );
}
