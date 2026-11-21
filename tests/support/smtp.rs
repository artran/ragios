use super::*;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::{
    io,
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const USER: &str = "fake-user";
const PASSWORD: &str = "fake-password-never-in-diagnostics";
const SERVICE: &str = "/hosts/local/services/application";
const DELIVERY: &str = "/notifications/status";

#[test]
fn confirmed_host_down_holds_service_email_until_reachability_returns() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    fs::copy(dir.path().join("check"), dir.path().join("probe")).unwrap();
    let probe = dir.path().join("probe");
    let script = fs::read_to_string(&probe).unwrap();
    fs::write(&probe, script.replace("< observation", "< reachability")).unwrap();
    with_reachability(&path, "ping", "probe", &[]);
    let reachability = "/hosts/local/reachability";
    let host_result = |status, text| {
        fs::write(
            dir.path().join("reachability"),
            format!("{status} {text}\n"),
        )
        .unwrap();
    };
    host_result(0, "initially reachable");
    observation(&dir, 0, "initially healthy");
    let mut app = Application::start(&path);
    app.page(reachability, "initially reachable");
    app.page(SERVICE, "initially healthy");

    host_result(2, "confirmed down sample");
    request_check(&mut app, reachability, "confirmed down sample");
    relay
        .receive("observer@example.invalid", "Confirmed problem: Down")
        .respond(250);
    check(&mut app, &dir, 2, "hidden service failure");
    check(&mut app, &dir, 0, "hidden service recovery");
    check(&mut app, &dir, 1, "still broken while down");
    app.page(DELIVERY, "Held: host confirmed down");
    relay.expect_quiet(Duration::from_millis(400));

    host_result(3, "undetermined sample");
    request_check(&mut app, reachability, "undetermined sample");
    relay
        .receive(
            "observer@example.invalid",
            "Confirmed reachability change: Undetermined",
        )
        .respond(250);
    relay
        .receive("observer@example.invalid", "Confirmed problem: WARNING")
        .respond(250);
    host_result(1, "degraded but reachable sample");
    request_check(&mut app, reachability, "degraded but reachable sample");
    relay
        .receive("observer@example.invalid", "Recovery: Reachable")
        .respond(250);
    relay.expect_quiet(Duration::from_millis(400));
}

#[test]
fn smtp_credentials_must_be_in_protected_files() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("password");
    fs::write(&credentials, format!("{PASSWORD}\n")).unwrap();
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o644)).unwrap();
    private_file(&dir, "username", USER);
    let path = config(&dir, &smtp_yaml("127.0.0.1", 1, "implicit"));
    let validate = || {
        Command::new(env!("CARGO_BIN_EXE_ragios"))
            .args(["--validate", "--config"])
            .arg(&path)
            .output()
            .unwrap()
    };
    let invalid = validate();
    assert!(!invalid.status.success());
    let stderr = String::from_utf8(invalid.stderr).unwrap();
    assert!(stderr.contains("owner-only"), "{stderr}");
    assert!(!stderr.contains(PASSWORD));
    fs::set_permissions(&credentials, fs::Permissions::from_mode(0o600)).unwrap();
    let valid = validate();
    assert!(valid.status.success(), "{:?}", valid);
}

#[test]
fn confirmed_notifications_retry_across_restart_and_delivered_mail_is_not_replayed() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "starttls");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 2");
    observation(&dir, 1, "first warning");
    let mut app = Application::start(&path);
    app.page(SERVICE, "Pending problem: WARNING (attempt 1)");
    app.page(DELIVERY, "No notification deliveries recorded");
    relay.expect_quiet(Duration::from_millis(300));

    check(&mut app, &dir, 1, "confirmed warning sample");
    let first = relay.receive("observer@example.invalid", "Confirmed problem: WARNING");
    // DATA is still waiting for a reply while the dashboard records new results.
    check(&mut app, &dir, 2, "pending critical sample");
    app.page(SERVICE, "Pending severity change: CRITICAL (1/2)");
    first.respond(451);
    let page = app.page(DELIVERY, "SMTP rejected delivery (reply 451)");
    assert!(page.contains("retrying"));
    assert!(!page.contains(PASSWORD) && !page.contains(USER));
    drop(app);

    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: WARNING")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td><td>2</td>");
    assert!(
        !app.stderr_lines
            .try_iter()
            .any(|line| line.contains(PASSWORD) || line.contains(USER))
    );
    drop(app);

    let mut app = Application::start(&path);
    app.page(DELIVERY, "<td>delivered</td><td>2</td>");
    relay.expect_quiet(Duration::from_millis(350));
    check(&mut app, &dir, 2, "confirmed critical sample");
    relay
        .receive(
            "observer@example.invalid",
            "Confirmed severity change: CRITICAL",
        )
        .respond(250);
    app.page_where(DELIVERY, "two delivered notifications", |page| {
        page.matches("<td>delivered</td>").count() == 2
    });
    check(&mut app, &dir, 0, "healthy sample");
    relay
        .receive("observer@example.invalid", "Recovery: OK")
        .respond(250);
    app.page_where(DELIVERY, "three delivered notifications", |page| {
        page.matches("<td>delivered</td>").count() == 3
    });
}

#[test]
fn recovered_or_superseded_notifications_are_not_replayed() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    observation(&dir, 3, "unknown sample");
    let mut app = Application::start(&path);
    let unknown = relay.receive("observer@example.invalid", "Confirmed problem: UNKNOWN");
    check(&mut app, &dir, 0, "cleared before delivery");
    unknown.respond(451);
    app.page_where(
        DELIVERY,
        "undelivered problem and ineligible recovery discarded",
        |page| page.matches("<td>discarded</td>").count() == 2,
    );
    drop(app);

    let mut app = Application::start(&path);
    check(&mut app, &dir, 2, "new critical problem");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td>");
    check(&mut app, &dir, 1, "warning supersedes critical");
    let warning = relay.receive(
        "observer@example.invalid",
        "Confirmed severity change: WARNING",
    );
    check(&mut app, &dir, 0, "healthy during warning delivery");
    warning.respond(451);
    let recovery = relay.receive("observer@example.invalid", "Recovery: OK");
    check(&mut app, &dir, 3, "problem recurs before recovery delivery");
    recovery.respond(451);
    relay
        .receive("observer@example.invalid", "Confirmed problem: UNKNOWN")
        .respond(250);
    app.page_where(DELIVERY, "four obsolete deliveries discarded", |page| {
        page.matches("<td>discarded</td>").count() == 4
    });
    relay.expect_quiet(Duration::from_millis(350));
}

#[test]
fn smtp_uses_resolved_host_group_and_global_recipients_and_can_be_disabled() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    let yaml = fs::read_to_string(&path).unwrap().replace(
        "hosts:\n",
        "groups:\n  - {id: ops, name: Ops, recipients: [group@example.invalid]}\nhosts:\n",
    );
    fs::write(
        &path,
        format!(
            "{yaml}
  - id: grouped
    name: Grouped host
    group: ops
    services: [{{id: application, name: Application, executable: check}}]
  - id: owner
    name: Owned host
    group: ops
    recipients: [owner@example.invalid, owner@example.invalid]
    services: [{{id: application, name: Application, executable: check}}]
  - id: quiet
    name: Quiet host
    group: ops
    recipients: []
    services: [{{id: application, name: Application, executable: check}}]
"
        ),
    )
    .unwrap();
    observation(&dir, 0, "initial healthy routing sample");
    let mut app = Application::start(&path);
    for (host, expected) in [
        ("local", "observer"),
        ("grouped", "group"),
        ("owner", "owner"),
    ] {
        let service = format!("/hosts/{host}/services/application");
        app.page(&service, "initial healthy routing sample");
        observation(&dir, 2, &format!("failure on {host}"));
        request_check(&mut app, &service, &format!("failure on {host}"));
        let message = relay.receive(
            &format!("{expected}@example.invalid"),
            "Confirmed problem: CRITICAL",
        );
        assert!(message.data.contains(&format!("Host: {host}")));
        message.respond(250);
    }
    request_check(
        &mut app,
        "/hosts/quiet/services/application",
        "failure on owner",
    );
    relay.expect_quiet(Duration::from_millis(350));
    app.page_where(DELIVERY, "three routed deliveries", |page| {
        page.matches("<td>delivered</td>").count() == 3
    });
    let yaml = fs::read_to_string(&path).unwrap();
    let smtp = format!(
        "{}  ca_file: relay-ca.pem\n",
        smtp_yaml("127.0.0.1", relay.port, relay.mode)
    );
    assert!(yaml.contains(&smtp));
    fs::write(&path, yaml.replace(&smtp, "email: {mode: disabled}\n")).unwrap();
    app.reload("reloaded");
    app.page(DELIVERY, "SMTP email disabled");
    check(&mut app, &dir, 0, "healthy while disabled");
    check(&mut app, &dir, 3, "unknown while disabled");
    relay.expect_quiet(Duration::from_millis(350));
}

#[test]
fn recovery_goes_only_to_recipients_whose_problem_mail_was_accepted() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    let yaml = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        yaml.replace(
            "recipients: [observer@example.invalid]",
            "recipients: [denied@example.invalid, observer@example.invalid]",
        ),
    )
    .unwrap();
    observation(&dir, 2, "initial confirmed problem");
    let mut app = Application::start(&path);
    relay
        .receive("denied@example.invalid", "Confirmed problem: CRITICAL")
        .respond(550);
    app.page(DELIVERY, "SMTP rejected delivery (reply 550)");
    let accepted = relay.receive("observer@example.invalid", "Confirmed problem: CRITICAL");
    // The SMTP acceptance arrives during graceful shutdown. It must be saved
    // so the restart does not send that problem again.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(app.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    accepted.respond(250);
    assert!(app.child.wait().unwrap().success());
    drop(app);

    let mut app = Application::start(&path);
    let page = app.page(DELIVERY, "<td>delivered</td><td>1</td>");
    assert!(page.contains("<td>failed</td><td>1</td>"));
    check(&mut app, &dir, 0, "recovered from the confirmed problem");
    relay
        .receive("observer@example.invalid", "Recovery: OK")
        .respond(250);
    app.page_where(DELIVERY, "two delivered notifications", |page| {
        page.matches("<td>delivered</td>").count() == 2
    });
    relay.expect_quiet(Duration::from_millis(350));
}

#[test]
fn smtp_reload_applies_new_relay_and_rejects_a_url_as_the_server() {
    let dir = TempDir::new().unwrap();
    let first = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &first, "confirmation_attempts: 1");
    observation(&dir, 0, "healthy before reload");
    let mut app = Application::start(&path);
    app.page(SERVICE, "healthy before reload");

    let second = TestRelay::listen(&dir, "starttls");
    let yaml = fs::read_to_string(&path).unwrap().replace(
        &smtp_yaml("127.0.0.1", first.port, first.mode),
        &smtp_yaml("127.0.0.1", second.port, second.mode),
    );
    fs::write(&path, &yaml).unwrap();
    app.reload("reloaded");
    fs::write(
        &path,
        yaml.replace("server: 127.0.0.1", "server: smtp://relay.example"),
    )
    .unwrap();
    assert!(app.reload("rejected").contains("email.server"));
    check(&mut app, &dir, 2, "problem after reload");
    second
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td>");
    first.expect_quiet(Duration::from_millis(300));
}

#[test]
fn another_server_cannot_resume_the_same_outbox() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    observation(&dir, 2, "problem in flight");
    let mut first = Application::start(&path);
    let delivery = relay.receive("observer@example.invalid", "Confirmed problem: CRITICAL");

    // Even a different database pathname must contend for the same lock.
    std::os::unix::fs::symlink("state.sqlite3", dir.path().join("alias.sqlite3")).unwrap();
    let alternate = dir.path().join("alternate.yaml");
    let yaml = fs::read_to_string(&path).unwrap();
    fs::write(
        &alternate,
        yaml.replace("database: state.sqlite3", "database: alias.sqlite3"),
    )
    .unwrap();
    let mut second = Command::new(env!("CARGO_BIN_EXE_ragios"))
        .arg("--config")
        .arg(alternate)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while second.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if second.try_wait().unwrap().is_none() {
        second.kill().unwrap();
    }
    let result = second.wait_with_output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("exclusive Ragios server lock"));
    delivery.respond(250);
    first.page(DELIVERY, "<td>delivered</td><td>1</td>");
    drop(first);
    let mut restarted = Application::start(&path);
    restarted.page(DELIVERY, "<td>delivered</td><td>1</td>");
    relay.expect_quiet(Duration::from_millis(350));
}

#[test]
fn database_uri_is_rejected_before_creating_a_file() {
    let dir = TempDir::new().unwrap();
    let path = config(&dir, "");
    let yaml = fs::read_to_string(&path).unwrap();
    let uri = "file:state.sqlite3?mode=rwc";
    fs::write(
        &path,
        yaml.replace("database: state.sqlite3", &format!("database: {uri}")),
    )
    .unwrap();
    // Reserve an HTTP port so this startup test exits promptly even if the
    // URI were incorrectly accepted and startup reached the listener.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let yaml = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        yaml.replace("127.0.0.1:0", &listener.local_addr().unwrap().to_string()),
    )
    .unwrap();
    for argument in ["ragios.yaml", "./ragios.yaml", path.to_str().unwrap()] {
        let result = Command::new(env!("CARGO_BIN_EXE_ragios"))
            .current_dir(dir.path())
            .args(["--config", argument])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(
            stderr.contains("database must be a filesystem path"),
            "{stderr}"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn current_notification_retries_after_smtp_credentials_are_corrected() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    private_file(&dir, "password", "incorrect");
    observation(&dir, 2, "problem before credential repair");
    let mut app = Application::start(&path);
    let page = app.page(DELIVERY, "SMTP rejected authentication (reply 535)");
    assert!(page.contains("<td>retrying</td>"));
    assert!(!page.contains("incorrect"));
    private_file(&dir, "password", PASSWORD);
    app.reload("reloaded");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td><td>2</td>");
}

#[tokio::test]
async fn dispatch_rechecks_confirmed_state_and_pruning_keeps_active_recovery_evidence() {
    use ragios::{
        config::Config,
        runner::{CheckResult, Status},
        store::{CheckTarget, DeliveryOutcome, Store},
    };
    let dir = TempDir::new().unwrap();
    private_file(&dir, "username", USER);
    private_file(&dir, "password", PASSWORD);
    let path = config(
        &dir,
        &format!(
            "{}recipients: [observer@example.invalid]",
            smtp_yaml("127.0.0.1", 1, "implicit")
        ),
    );
    let config = Arc::new(Config::load(&path).unwrap());
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let store = Store::open_server(&config.database).unwrap();
    let old = chrono::Utc::now() - chrono::TimeDelta::days(40);
    let save = |status| {
        let timestamp = old.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store.save_with_notifications(
            "local".into(),
            CheckTarget::Service("application".into()),
            CheckResult {
                status,
                output: String::new(),
                started_at: timestamp.clone(),
                finished_at: timestamp,
                duration_ms: 0,
            },
            1,
            vec!["observer@example.invalid".into()],
        )
    };
    save(Status::Critical).await.unwrap();
    let candidate = store
        .next_notification(active.clone())
        .await
        .unwrap()
        .unwrap();
    let before_discard = chrono::Utc::now().timestamp_millis();
    save(Status::Ok).await.unwrap();
    let records = store.recent_deliveries().await.unwrap();
    assert_eq!(records[0].state, "discarded");
    assert!(
        chrono::DateTime::parse_from_rfc3339(&records[0].updated_at)
            .unwrap()
            .timestamp_millis()
            >= before_discard
    );
    // Leave the row selectable, isolating the dispatch recheck from the
    // normal superseding write that already marks this delivery discarded.
    rusqlite::Connection::open(&config.database)
        .unwrap()
        .execute(
            "UPDATE notification_deliveries SET state = 'queued' WHERE event_id = ?1",
            [candidate.event_id],
        )
        .unwrap();
    assert!(
        store
            .begin_notification(candidate, active.clone(), config.clone())
            .await
            .unwrap()
            .is_none()
    );
    save(Status::Warning).await.unwrap();
    let candidate = store
        .next_notification(active.clone())
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification(candidate, active.clone(), config.clone())
        .await
        .unwrap()
        .unwrap();
    store
        .finish_notification(sending, DeliveryOutcome::Delivered)
        .await
        .unwrap();

    let future = chrono::Utc::now() + chrono::TimeDelta::days(31);
    store.prune_notifications(future).await.unwrap();
    let remaining = store.recent_deliveries().await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].notification.label(),
        "Confirmed problem: WARNING"
    );
    save(Status::Ok).await.unwrap();
    store.prune_notifications(future).await.unwrap();
    let recovery = store
        .next_notification(active.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovery.label(), "Recovery: OK");
    let sending = store
        .begin_notification(recovery, active, config)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_notification(sending, DeliveryOutcome::Delivered)
        .await
        .unwrap();
    store.prune_notifications(future).await.unwrap();
    assert!(store.recent_deliveries().await.unwrap().is_empty());
}

fn check(app: &mut Application, dir: &TempDir, status: i32, text: &str) {
    observation(dir, status, text);
    request_check(app, SERVICE, text);
}

fn request_check(app: &mut Application, service: &str, text: &str) {
    let page = app.page(service, "Check now");
    let response = app
        .check_now(service, check_now_token(&page))
        .send()
        .unwrap();
    assert!(response.text().unwrap().contains("Check started"));
    app.page(&format!("{service}/status"), text);
}

fn observation(dir: &TempDir, status: i32, text: &str) {
    fs::write(
        dir.path().join("observation.next"),
        format!("{status} {text}\n"),
    )
    .unwrap();
    fs::rename(
        dir.path().join("observation.next"),
        dir.path().join("observation"),
    )
    .unwrap();
}

fn notification_config(dir: &TempDir, relay: &TestRelay, extra: &str) -> std::path::PathBuf {
    private_file(dir, "username", USER);
    private_file(dir, "password", PASSWORD);
    plugin(
        dir,
        "cd \"${0%/*}\" || exit 3\nread -r status text < observation\nprintf '%s\\n' \"$text\"\nexit \"$status\"",
    );
    config(
        dir,
        &format!(
            "interval_seconds: 300\nretry_interval_seconds: 300\nstartup_spread_seconds: 0\nrecipients: [observer@example.invalid]\n{}  ca_file: relay-ca.pem\n{extra}",
            smtp_yaml("127.0.0.1", relay.port, relay.mode)
        ),
    )
}

fn private_file(dir: &TempDir, name: &str, value: &str) {
    let path = dir.path().join(name);
    fs::write(&path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn smtp_yaml(server: &str, port: u16, tls: &str) -> String {
    format!(
        "email:\n  mode: smtp\n  server: {server}\n  port: {port}\n  tls: {tls}\n  from: monitor@example.invalid\n  username_file: username\n  password_file: password\n"
    )
}

struct TestRelay {
    port: u16,
    mode: &'static str,
    messages: mpsc::Receiver<Attempt>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

struct Attempt {
    recipient: String,
    data: String,
    reply: mpsc::Sender<u16>,
}

impl Attempt {
    fn respond(self, code: u16) {
        self.reply.send(code).unwrap();
    }
}

impl TestRelay {
    fn listen(dir: &TempDir, mode: &'static str) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().unwrap();
        let root = ca.self_signed(&ca_key).unwrap();
        fs::write(dir.path().join("relay-ca.pem"), root.pem()).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        let tls = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
                )
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (events, messages) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let thread = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => handle_smtp(stream, mode, tls.clone(), &events),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("test SMTP accept: {error}"),
                }
            }
        });
        Self {
            port,
            mode,
            messages,
            stop,
            thread: Some(thread),
        }
    }

    fn receive(&self, recipient: &str, label: &str) -> Attempt {
        let mail = self
            .messages
            .recv_timeout(Duration::from_secs(8))
            .expect("expected SMTP DATA");
        assert_eq!(mail.recipient, format!("RCPT TO:<{recipient}>"));
        assert!(
            mail.data.contains(&format!("Subject: Ragios: {label}")),
            "{}",
            mail.data
        );
        mail
    }

    fn expect_quiet(&self, duration: Duration) {
        assert!(
            matches!(
                self.messages.recv_timeout(duration),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "SMTP relay received unexpected mail or stopped"
        );
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn smtp_line(stream: &mut impl BufRead) -> String {
    let mut line = String::new();
    assert!(
        stream.read_line(&mut line).unwrap() > 0,
        "unexpected SMTP EOF"
    );
    line.trim_end_matches("\r\n").into()
}

fn smtp_reply(stream: &mut impl Write, reply: &str) {
    write!(stream, "{reply}\r\n").unwrap();
    stream.flush().unwrap();
}

fn handle_smtp(
    stream: TcpStream,
    mode: &str,
    tls: Arc<ServerConfig>,
    events: &mpsc::Sender<Attempt>,
) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let stream = if mode == "starttls" {
        let mut clear = BufReader::new(stream);
        smtp_reply(clear.get_mut(), "220 localhost test relay");
        assert!(smtp_line(&mut clear).starts_with("EHLO "));
        smtp_reply(
            clear.get_mut(),
            "250-localhost\r\n250-STARTTLS\r\n250 AUTH PLAIN",
        );
        assert_eq!(
            smtp_line(&mut clear),
            "STARTTLS",
            "must not authenticate in cleartext"
        );
        smtp_reply(clear.get_mut(), "220 Begin TLS");
        assert!(clear.buffer().is_empty());
        clear.into_inner()
    } else {
        stream
    };
    let mut secure = BufReader::new(StreamOwned::new(
        ServerConnection::new(tls).unwrap(),
        stream,
    ));
    if mode == "implicit" {
        smtp_reply(secure.get_mut(), "220 localhost test relay");
    }
    assert!(smtp_line(&mut secure).starts_with("EHLO "));
    smtp_reply(secure.get_mut(), "250-localhost\r\n250 AUTH PLAIN");
    let auth = smtp_line(&mut secure);
    assert!(auth.starts_with("AUTH PLAIN "));
    if auth != "AUTH PLAIN AGZha2UtdXNlcgBmYWtlLXBhc3N3b3JkLW5ldmVyLWluLWRpYWdub3N0aWNz" {
        smtp_reply(secure.get_mut(), "535 incorrect credentials");
        return;
    }
    smtp_reply(secure.get_mut(), "235 Authenticated");
    assert_eq!(
        smtp_line(&mut secure),
        "MAIL FROM:<monitor@example.invalid>"
    );
    smtp_reply(secure.get_mut(), "250 Sender accepted");
    let recipient = smtp_line(&mut secure);
    smtp_reply(secure.get_mut(), "250 Recipient accepted");
    assert_eq!(smtp_line(&mut secure), "DATA");
    smtp_reply(secure.get_mut(), "354 End with dot");
    let mut data = String::new();
    loop {
        let line = smtp_line(&mut secure);
        if line == "." {
            break;
        }
        data.push_str(&line);
        data.push('\n');
    }
    let (reply, answer) = mpsc::channel();
    events
        .send(Attempt {
            recipient,
            data,
            reply,
        })
        .unwrap();
    if let Ok(code) = answer.recv_timeout(Duration::from_secs(6)) {
        let text = if code == 250 {
            "accepted".into()
        } else {
            format!("raw remote text echoing {USER} and {PASSWORD}")
        };
        smtp_reply(secure.get_mut(), &format!("{code} {text}"));
    }
    // The worker may pool connections, but the fixture accepts exactly one DATA per connection.
    secure.get_mut().conn.send_close_notify();
    let _ = secure.get_mut().flush();
}
