use super::{CheckPath, CheckSnapshot, NotificationSuppression, Store};
use crate::{
    config::{Config, Probe, ReachabilityCheck},
    runner::{CheckResult, Status},
    store::{AcknowledgeOutcome, CheckState, CheckTarget, TransitionKind},
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;
use tokio::sync::watch;

const LIMIT: Duration = Duration::from_secs(5);

fn config(dir: &Path) -> Config {
    let path = dir.join("ragios.yaml");
    std::fs::write(
        &path,
        r#"
plugin_dir: plugins
database: checks.sqlite3
confirmation_attempts: 1
hosts:
  - id: host
    name: Original Host
    services:
      - id: service
        name: Example service
        executable: /bin/true
"#,
    )
    .unwrap();
    Config::load(&path).unwrap()
}

fn pause_before_read(
    store: &Store,
    table: &'static str,
) -> (oneshot::Receiver<()>, mpsc::Sender<()>) {
    // Pause the reader as SQLite prepares the next SELECT. A different WAL
    // connection can commit while the reader is paused, without sleeping.
    let (reached, observe) = oneshot::channel();
    let (resume, paused) = mpsc::channel();
    let mut reached = Some(reached);
    store
        .0
        .lock()
        .unwrap()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read { table_name, .. } if table_name == table)
                && let Some(reached) = reached.take()
            {
                let _ = reached.send(());
                if paused.recv_timeout(LIMIT).is_err() {
                    return Authorization::Deny;
                }
            }
            Authorization::Allow
        }));
    (observe, resume)
}

fn result(status: Status, output: &str) -> CheckResult {
    CheckResult {
        status,
        output: output.into(),
        started_at: "2026-11-21T12:00:00.000Z".into(),
        finished_at: "2026-11-21T12:00:01.000Z".into(),
        duration_ms: 1000,
    }
}

async fn save(store: &Store, status: Status, output: &str) {
    store
        .save("host".into(), "service".into(), result(status, output), 1)
        .await
        .unwrap();
}

fn service_path() -> Option<CheckPath> {
    Some(CheckPath::Service("host".into(), "service".into()))
}

fn service(snapshot: &CheckSnapshot) -> &CheckState {
    &snapshot.services[&("host".into(), "service".into())]
}

#[tokio::test]
async fn detail_does_not_mix_state_and_history_across_a_check_result_commit() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let reader = Store::open(&config.database).unwrap();
    let writer = Store::open(&config.database).unwrap();
    let (_published, active) = watch::channel(Arc::new(config));
    save(&writer, Status::Critical, "failed").await;
    let before = reader.browse_checks(active.clone(), None).await.unwrap();
    let token = service(&before).episode.as_ref().unwrap().token.clone();
    assert!(matches!(
        writer
            .acknowledge_problem(active.clone(), "host".into(), Some("service".into()), token)
            .await
            .unwrap(),
        AcknowledgeOutcome::Acknowledged
    ));

    let (reached, resume) = pause_before_read(&reader, "check_transitions");
    let next = reader.clone();
    let browsing = tokio::spawn(async move { next.browse_checks(active, service_path()).await });
    tokio::time::timeout(LIMIT, reached).await.unwrap().unwrap();
    save(&writer, Status::Ok, "recovered").await;
    resume.send(()).unwrap();

    let detail = browsing.await.unwrap().unwrap();
    assert_eq!(service(&detail).result.output, "failed");
    assert_eq!(
        service(&detail).problem.confirmed_severity,
        Some(Status::Critical)
    );
    assert!(service(&detail).episode.as_ref().unwrap().acknowledged);
    assert_eq!(
        detail
            .transitions
            .iter()
            .map(|t| t.kind)
            .collect::<Vec<_>>(),
        [TransitionKind::Confirmed]
    );
}

#[tokio::test]
async fn detail_retries_if_configuration_is_published_during_the_history_read() {
    let dir = tempfile::tempdir().unwrap();
    let original = config(dir.path());
    let reader = Store::open(&original.database).unwrap();
    let writer = Store::open(&original.database).unwrap();
    let mut replacement = original.clone();
    replacement.hosts[0].name = "Reloaded Host".into();
    let (published, active) = watch::channel(Arc::new(original));
    save(&writer, Status::Critical, "old check result").await;

    let (reached, resume) = pause_before_read(&reader, "check_transitions");
    let browsing = tokio::spawn(async move { reader.browse_checks(active, service_path()).await });
    tokio::time::timeout(LIMIT, reached).await.unwrap().unwrap();
    writer
        .publish_notification_config(Arc::new(replacement), published)
        .await
        .unwrap();
    save(&writer, Status::Ok, "reloaded check result").await;
    resume.send(()).unwrap();

    let detail = browsing.await.unwrap().unwrap();
    assert_eq!(detail.config.hosts[0].name, "Reloaded Host");
    assert_eq!(service(&detail).result.output, "reloaded check result");
    assert_eq!(
        detail.transitions.last().unwrap().kind,
        TransitionKind::Recovery
    );
}

#[tokio::test]
async fn notification_browsing_does_not_mix_deliveries_and_suppression() {
    let dir = tempfile::tempdir().unwrap();
    config(dir.path());
    for name in ["username", "password"] {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.path().join(name))
            .unwrap()
            .write_all(b"unused-test-credential")
            .unwrap();
    }
    let path = dir.path().join("ragios.yaml");
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            br#"
recipients: [ops@example.invalid]
email:
  mode: smtp
  server: relay.example.invalid
  tls: implicit
  from: ragios@example.invalid
  username_file: username
  password_file: password
"#,
        )
        .unwrap();
    let mut config = Config::load(&path).unwrap();
    config.hosts[0].reachability = Some(ReachabilityCheck {
        probe: Probe::Ping,
        check: config.hosts[0].services[0].check.clone(),
        notification_window: None,
    });
    let config = Arc::new(config);
    let reader = Store::open(&config.database).unwrap();
    let writer = Store::open(&config.database).unwrap();
    let (_published, active) = watch::channel(config.clone());

    let (reached, resume) = pause_before_read(&reader, "check_problems");
    let next = reader.clone();
    let pinned = active.clone();
    let browsing = tokio::spawn(async move { next.browse_notifications(pinned).await });
    tokio::time::timeout(LIMIT, reached).await.unwrap().unwrap();
    writer
        .save_with_notifications(
            "host".into(),
            CheckTarget::Host(Probe::Ping),
            result(Status::Critical, "host down"),
            config,
        )
        .await
        .unwrap();
    resume.send(()).unwrap();

    let before = browsing.await.unwrap().unwrap();
    assert!(before.deliveries.is_empty());
    assert!(before.suppressions.is_empty());

    let after = reader.browse_notifications(active).await.unwrap();
    assert_eq!(after.deliveries.len(), 1);
    assert_eq!(after.deliveries[0].notification.host_id, "host");
    assert!(
        matches!(&after.suppressions[..], [NotificationSuppression::HostDown { host_id }] if host_id == "host")
    );
}
