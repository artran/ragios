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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

const USER: &str = "fake-user";
const PASSWORD: &str = "fake-password-never-in-diagnostics";
const SERVICE: &str = "/hosts/local/services/application";
const DELIVERY: &str = "/notifications/status";

#[test]
fn shadow_mode_records_disabled_decisions_without_contacting_smtp_after_restart() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 2");
    let smtp_yaml = fs::read_to_string(&path).unwrap();
    update_config(&path, |yaml| {
        yaml["email"] = serde_yaml::from_str("{mode: disabled}").unwrap();
    });
    observation(&dir, 0, "healthy in shadow mode");
    let mut app = Application::start(&path);
    app.page(DELIVERY, "SMTP email disabled");
    app.page(SERVICE, "healthy in shadow mode");
    check(&mut app, &dir, 2, "first shadow failure");
    assert!(
        !app.page(SERVICE, "first shadow failure")
            .contains("Email skipped: disabled")
    );
    check(&mut app, &dir, 2, "confirmed shadow failure");
    app.page(SERVICE, "Email skipped: disabled");
    drop(app);

    let mut app = Application::start(&path);
    let history = app.page(SERVICE, "confirmed shadow failure");
    assert_eq!(history.matches("Email skipped: disabled").count(), 1);
    app.page(DELIVERY, "SMTP email disabled");
    check(&mut app, &dir, 0, "recovered in shadow mode");
    let history = app.page(SERVICE, "recovered in shadow mode");
    assert_eq!(history.matches("Email skipped: disabled").count(), 2);
    check(&mut app, &dir, 2, "first sample of another shadow failure");
    check(&mut app, &dir, 2, "shadow problem still open at enablement");
    app.page(DELIVERY, "No notification deliveries recorded");
    assert_eq!(relay.connections.load(Ordering::Relaxed), 0);

    fs::write(&path, smtp_yaml).unwrap();
    app.reload("reloaded");
    app.page(DELIVERY, "SMTP email enabled");
    relay.expect_quiet(Duration::from_millis(400));
    assert_eq!(relay.connections.load(Ordering::Relaxed), 0);
    check(
        &mut app,
        &dir,
        0,
        "recovered from the unnotified shadow problem",
    );
    check(&mut app, &dir, 2, "first active failure");
    check(&mut app, &dir, 2, "confirmed active failure");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td><td>1</td>");
}

#[test]
fn shadow_cutover_releases_only_eligible_mail_and_recovers_after_restart() {
    use chrono::{Datelike, TimeDelta, Utc};

    const PROBE: &str = "/hosts/local/reachability";
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "starttls");
    // Make the window closed regardless of the day CI runs. A YAML reload,
    // rather than waiting for a calendar boundary, opens it for the service.
    let closed_day = (Utc::now() + TimeDelta::days(3)).weekday().to_string();
    let path = notification_config(
        &dir,
        &relay,
        &format!(
            "confirmation_attempts: 2\nreminder_interval_seconds: 1\nnotification_window: staffed\n\
             check_windows:\n  - id: staffed\n    ranges: [{{days: [{}], start: '00:00', end: '24:00'}}]",
            closed_day.to_lowercase()
        ),
    );
    install_probe(&dir, &path);
    update_config(&path, |yaml| {
        yaml["hosts"][0]["reachability"]["notification_window"] = "staffed".into();
        yaml["groups"] =
            serde_yaml::from_str("[{id: ops, name: Ops}, {id: web, name: Web}]").unwrap();
        yaml["hosts"][0]["group"] = "ops".into();
    });
    host_observation(&dir, 0, "initial reachable host");
    observation(&dir, 0, "initial healthy service");
    let mut app = Application::start(&path);
    app.page(PROBE, "initial reachable host");
    app.page(SERVICE, "initial healthy service");

    check(&mut app, &dir, 2, "first failing sample");
    app.page(SERVICE, "Pending problem: CRITICAL (attempt 1)");
    app.page(DELIVERY, "No notification deliveries recorded");
    check(&mut app, &dir, 2, "confirmed failing sample");
    app.page(
        DELIVERY,
        "local / application: Outside notification window (staffed)",
    );
    relay.expect_quiet(Duration::from_millis(350));

    for text in ["first failed reachability", "confirmed failed reachability"] {
        host_observation(&dir, 2, text);
        request_check(&mut app, PROBE, text);
    }
    update_config(&path, |yaml| yaml["notification_window"] = "always".into());
    app.reload("reloaded");
    let delivery = app.page(DELIVERY, "Held: host confirmed down");
    assert!(!delivery.contains("local / application: Outside notification window"));
    relay.expect_quiet(Duration::from_millis(350));

    let page = app.page("/silences", "Create silence");
    let silence = |app: &Application, scope: &str, expires: TimeDelta, reason: &str| {
        assert!(
            app.client
                .post(format!("{}/silences", app.url))
                .form(&[
                    ("csrf", check_now_token(&page)),
                    ("scope", scope),
                    ("expiry", &(Utc::now() + expires).to_rfc3339()),
                    ("reason", reason),
                ])
                .send()
                .unwrap()
                .status()
                .is_success()
        );
    };
    silence(
        &app,
        r#"{"group":"ops"}"#,
        TimeDelta::hours(1),
        "group work",
    );
    host_observation(&dir, 0, "reachability recovered during maintenance");
    request_check(&mut app, PROBE, "reachability recovered during maintenance");
    app.page_where(DELIVERY, "host hold cleared", |page| {
        !page.contains("Held: host confirmed down")
    });

    relay.expect_quiet(Duration::from_millis(350));
    silence(
        &app,
        r#"{"service":{"host":"local","service":"application"}}"#,
        TimeDelta::seconds(5),
        "service work",
    );
    let silences = app.page("/silences/status", "service work");
    assert_eq!(silences.matches("<td>Active</td>").count(), 2);
    update_config(&path, |yaml| yaml["hosts"][0]["group"] = "web".into());
    app.reload("reloaded");
    // The group silence no longer applies, but the service silence still does.
    relay.expect_quiet(Duration::from_millis(350));
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(451);
    app.page("/silences/status", "<td>service work</td><td>Expired</td>");
    app.page(DELIVERY, "<td>retrying</td><td>1</td>");
    let page = app.page(SERVICE, "Acknowledge problem");
    assert!(
        acknowledge(&app, SERVICE, check_now_token(&page), episode_token(&page))
            .send()
            .unwrap()
            .status()
            .is_success()
    );
    drop(app);

    let mut app = Application::start(&path);
    app.page(SERVICE, "Acknowledged");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td><td>2</td>");
    relay.expect_quiet(Duration::from_millis(1500));
    check(&mut app, &dir, 0, "service recovered after restart");
    relay
        .receive("observer@example.invalid", "Recovery: OK")
        .respond(250);
    app.page_where(DELIVERY, "problem and recovery accepted", |page| {
        page.matches("<td>delivered</td>").count() == 2
    });
    assert!(
        !app.page(SERVICE, "No confirmed problem")
            .contains("Acknowledged")
    );
}

#[test]
fn dashboard_silences_are_scoped_protected_and_survive_restart() {
    use chrono::{TimeDelta, Utc};

    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    install_probe(&dir, &path);
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    yaml["groups"] = serde_yaml::from_str("[{id: ops, name: Ops}]").unwrap();
    yaml["hosts"][0]["group"] = "ops".into();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    host_observation(&dir, 0, "host initially reachable");
    observation(&dir, 0, "service initially healthy");
    let mut app = Application::start(&path);
    app.page(SERVICE, "service initially healthy");
    app.page("/hosts/local/reachability", "host initially reachable");
    let page = app.page("/silences", "Create silence");
    let response = app
        .client
        .get(format!("{}/silences", app.url))
        .send()
        .unwrap();
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    let token = check_now_token(&page);
    let service = r#"{"service":{"host":"local","service":"application"}}"#;
    let host = r#"{"host":"local"}"#;
    let group = r#"{"group":"ops"}"#;
    let future = (Utc::now() + TimeDelta::hours(1)).to_rfc3339();
    let past = (Utc::now() - TimeDelta::minutes(1)).to_rfc3339();
    let submit = |scope: &str, expiry: &str, csrf: &str| {
        app.client.post(format!("{}/silences", app.url)).form(&[
            ("csrf", csrf),
            ("scope", scope),
            ("expiry", expiry),
            ("reason", "<maintenance>"),
        ])
    };
    assert_eq!(
        submit(service, &future, "wrong").send().unwrap().status(),
        403
    );
    assert_eq!(
        submit(service, &future, token)
            .header("Origin", "https://foreign.invalid")
            .send()
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        submit(service, &future, token)
            .header("Sec-Fetch-Site", "cross-site")
            .send()
            .unwrap()
            .status(),
        403
    );
    for (scope, expiry) in [
        (r#"{"host":"missing"}"#, future.as_str()),
        (service, &past),
        (service, "never"),
    ] {
        assert_eq!(submit(scope, expiry, token).send().unwrap().status(), 400);
    }
    assert!(
        submit(service, &future, token)
            .header("Origin", &app.url)
            .send()
            .unwrap()
            .status()
            .is_success()
    );
    check(&mut app, &dir, 2, "confirmed service during own silence");
    relay.expect_quiet(Duration::from_millis(400));
    // The service silence must not touch reachability.
    host_observation(&dir, 3, "host undetermined independently");
    request_check(
        &mut app,
        "/hosts/local/reachability",
        "host undetermined independently",
    );
    relay
        .receive(
            "observer@example.invalid",
            "Confirmed problem: Undetermined",
        )
        .respond(250);
    for scope in [host, group] {
        assert!(
            app.client
                .post(format!("{}/silences", app.url))
                .form(&[
                    ("csrf", token),
                    ("scope", scope),
                    ("expiry", &future),
                    ("reason", "<maintenance>"),
                ])
                .send()
                .unwrap()
                .status()
                .is_success()
        );
    }
    drop(app);
    let mut app = Application::start(&path);
    let page = app.page("/silences", "&lt;maintenance&gt;");
    assert_eq!(page.matches("<td>Active</td>").count(), 3);
    assert!(!page.contains("<maintenance>"));
    host_observation(&dir, 0, "host recovered under silence");
    request_check(
        &mut app,
        "/hosts/local/reachability",
        "host recovered under silence",
    );
    check(&mut app, &dir, 0, "service recovered under silence");
    relay.expect_quiet(Duration::from_millis(400));
}

#[test]
fn group_silence_holds_scheduled_results_until_expiry_then_smtp_sends_the_recovery() {
    use chrono::{TimeDelta, Utc};

    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    let yaml = fs::read_to_string(&path).unwrap().replacen(
        "interval_seconds: 300",
        "interval_seconds: 1",
        1,
    );
    fs::write(&path, yaml).unwrap();
    observation(&dir, 2, "problem before maintenance");
    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td>");
    let page = app.page("/silences", "Create silence");
    let group = ragios::config::Config::load(&path).unwrap().hosts[0]
        .group
        .clone();
    let scope = serde_json::to_string(&ragios::store::SilenceScope::Group(group)).unwrap();
    let expires = Utc::now() + TimeDelta::seconds(5);
    let response = app
        .client
        .post(format!("{}/silences", app.url))
        .form(&[
            ("scope", scope.as_str()),
            ("expiry", &expires.to_rfc3339()),
            ("csrf", check_now_token(&page)),
        ])
        .send()
        .unwrap();
    assert!(response.status().is_success());
    observation(&dir, 1, "scheduled severity change during maintenance");
    app.page(SERVICE, "scheduled severity change during maintenance");
    observation(&dir, 0, "scheduled recovery during maintenance");
    app.page(SERVICE, "scheduled recovery during maintenance");
    assert!(
        Utc::now() < expires,
        "periodic results should update before expiry"
    );
    relay.expect_quiet(Duration::from_millis(300));
    let recovery = relay.receive("observer@example.invalid", "Recovery: OK");
    assert!(Utc::now().timestamp_millis() >= expires.timestamp_millis());
    recovery.respond(250);
    app.page("/silences/status", "<td>Expired</td>");
    relay.expect_quiet(Duration::from_millis(300));
}

#[test]
fn moving_out_of_a_silenced_group_does_not_replay_an_already_delivered_severity() {
    use chrono::{TimeDelta, Utc};

    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    observation(&dir, 2, "first critical result");
    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td>");
    let page = app.page("/silences", "Create silence");
    let group = ragios::config::Config::load(&path).unwrap().hosts[0]
        .group
        .clone();
    let scope = serde_json::to_string(&ragios::store::SilenceScope::Group(group)).unwrap();
    assert!(
        app.client
            .post(format!("{}/silences", app.url))
            .form(&[
                ("scope", scope.as_str()),
                ("expiry", &(Utc::now() + TimeDelta::hours(1)).to_rfc3339()),
                ("csrf", check_now_token(&page)),
            ])
            .send()
            .unwrap()
            .status()
            .is_success()
    );
    check(&mut app, &dir, 1, "warning during maintenance");
    check(&mut app, &dir, 2, "critical again during maintenance");
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    yaml["groups"] = serde_yaml::from_str("[{id: other, name: Other}]").unwrap();
    yaml["hosts"][0]["group"] = "other".into();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    app.reload("reloaded");
    app.page(
        DELIVERY,
        "Current severity already delivered before suppression",
    );
    relay.expect_quiet(Duration::from_millis(400));
}

#[test]
fn reload_marks_silenced_mail_even_if_smtp_is_busy_until_after_expiry() {
    use chrono::{TimeDelta, Utc};

    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    install_probe(&dir, &path);
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    yaml["groups"] = serde_yaml::from_str("[{id: ops, name: Ops}]").unwrap();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    observation(&dir, 2, "first critical");
    host_observation(&dir, 0, "reachable initially");
    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td>");
    let reachability = "/hosts/local/reachability";
    app.page(reachability, "reachable initially");
    host_observation(&dir, 3, "reachability mail in flight");
    request_check(&mut app, reachability, "reachability mail in flight");
    let in_flight = relay.receive(
        "observer@example.invalid",
        "Confirmed problem: Undetermined",
    );
    check(&mut app, &dir, 1, "queued warning");
    check(&mut app, &dir, 2, "queued return to critical");

    let page = app.page("/silences", "Create silence");
    let expiry = (Utc::now() + TimeDelta::seconds(4)).to_rfc3339();
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(format!("{}/silences", app.url))
        .form(&[
            ("csrf", check_now_token(&page)),
            ("scope", r#"{"group":"ops"}"#),
            ("expiry", &expiry),
        ])
        .send()
        .unwrap();
    assert_eq!(response.status(), 303);
    let framing = response.headers().get("x-frame-options").cloned();
    yaml["hosts"][0]["group"] = "ops".into();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    app.reload("reloaded");
    app.page("/silences/status", "<td>Active</td>");
    app.page("/silences/status", "<td>Expired</td>");
    in_flight.respond(250);
    app.page(
        DELIVERY,
        "Current severity already delivered before suppression",
    );
    relay.expect_quiet(Duration::from_millis(400));
    assert_eq!(framing.unwrap(), "DENY");
}

#[test]
fn confirmed_host_down_holds_service_email_until_reachability_returns() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(&dir, &relay, "confirmation_attempts: 1");
    install_probe(&dir, &path);
    let reachability = "/hosts/local/reachability";
    let host_result = |status, text| host_observation(&dir, status, text);
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
        .receive("observer@example.invalid", "Confirmed problem: WARNING")
        .respond(250);
    relay
        .receive(
            "observer@example.invalid",
            "Confirmed reachability change: Undetermined",
        )
        .respond(250);
    host_result(1, "degraded but reachable sample");
    request_check(&mut app, reachability, "degraded but reachable sample");
    relay
        .receive("observer@example.invalid", "Recovery: Reachable")
        .respond(250);
    relay.expect_quiet(Duration::from_millis(400));
}

#[test]
fn smtp_sends_recurring_host_reminders_but_no_service_mail_during_a_prolonged_outage() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(
        &dir,
        &relay,
        "confirmation_attempts: 1\nreminder_interval_seconds: 1",
    );
    install_probe(&dir, &path);
    host_observation(&dir, 2, "host outage");
    observation(&dir, 0, "initially healthy");
    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: Down")
        .respond(250);
    check(&mut app, &dir, 2, "held during outage");
    for _ in 0..2 {
        relay
            .receive("observer@example.invalid", "Reminder: Down")
            .respond(250);
    }
    app.page(DELIVERY, "Reminder: Down");
    let reachability = "/hosts/local/reachability";
    let page = app.page(reachability, "Acknowledge problem");
    assert!(
        acknowledge(
            &app,
            reachability,
            super::check_now_token(&page),
            episode_token(&page)
        )
        .send()
        .unwrap()
        .status()
        .is_success()
    );
    app.page(reachability, "Acknowledged");
    check(&mut app, &dir, 1, "severity changed while host is down");
    app.page(DELIVERY, "Held: host confirmed down");
    assert!(
        !app.page(SERVICE, "Confirmed problem: WARNING")
            .contains("Acknowledged")
    );
    relay.expect_quiet(Duration::from_millis(1500));
}

fn episode_token(page: &str) -> &str {
    page.split_once("name=\"episode\" value=\"")
        .expect("acknowledgement form has an episode token")
        .1
        .split('"')
        .next()
        .unwrap()
}

fn acknowledge(
    app: &Application,
    target: &str,
    csrf: &str,
    episode: &str,
) -> reqwest::blocking::RequestBuilder {
    app.client
        .post(format!("{}{target}/acknowledge", app.url))
        .form(&[("csrf", csrf), ("episode", episode)])
}

#[test]
fn dashboard_acknowledgement_survives_restart_and_stops_only_reminders() {
    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let path = notification_config(
        &dir,
        &relay,
        "confirmation_attempts: 1\nreminder_interval_seconds: 2",
    );
    observation(&dir, 2, "current failure");
    let mut app = Application::start(&path);
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    let page = app.page(SERVICE, "Acknowledge problem");
    let csrf = super::check_now_token(&page);
    let episode = episode_token(&page);
    let acknowledge =
        |app: &Application, target: &str, csrf: &str| acknowledge(app, target, csrf, episode);
    assert_eq!(
        acknowledge(&app, SERVICE, csrf)
            .header("sec-fetch-site", "cross-site")
            .send()
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        acknowledge(&app, SERVICE, "invalid")
            .send()
            .unwrap()
            .status(),
        403
    );
    for target in [
        "/hosts/unknown/services/application",
        "/hosts/local/services/missing",
        "/hosts/local/reachability",
    ] {
        assert_eq!(
            acknowledge(&app, target, csrf).send().unwrap().status(),
            404
        );
    }
    assert!(
        acknowledge(&app, SERVICE, csrf)
            .send()
            .unwrap()
            .status()
            .is_success()
    );
    app.page("/status", "Acknowledged");
    drop(app);
    let mut app = Application::start(&path);
    let after_restart = app.page(SERVICE, "Acknowledged");
    let csrf = super::check_now_token(&after_restart);
    check(&mut app, &dir, 2, "same problem after manual check");
    app.page(SERVICE, "Acknowledged");
    check(&mut app, &dir, 1, "confirmed change");
    relay
        .receive(
            "observer@example.invalid",
            "Confirmed severity change: WARNING",
        )
        .respond(250);
    app.page(SERVICE, "Acknowledged");
    relay.expect_quiet(Duration::from_millis(2300));
    check(&mut app, &dir, 0, "recovered after acknowledgement");
    relay
        .receive("observer@example.invalid", "Recovery: OK")
        .respond(250);
    assert!(
        !app.page(SERVICE, "No confirmed problem")
            .contains("Acknowledged")
    );
    assert_eq!(
        acknowledge(&app, SERVICE, csrf).send().unwrap().status(),
        409
    );
    check(&mut app, &dir, 2, "new episode");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    assert_eq!(
        acknowledge(&app, SERVICE, csrf).send().unwrap().status(),
        409
    );
    relay
        .receive("observer@example.invalid", "Reminder: CRITICAL")
        .respond(250);
}

#[test]
fn reopening_notification_window_sends_only_the_current_problem() {
    use chrono::Datelike;

    let dir = TempDir::new().unwrap();
    let relay = TestRelay::listen(&dir, "implicit");
    let closed_day = (chrono::Utc::now() + chrono::TimeDelta::days(3))
        .weekday()
        .to_string()
        .to_lowercase();
    let path = notification_config(
        &dir,
        &relay,
        &format!(
            "confirmation_attempts: 1\nnotification_window: staffed\ncheck_windows:\n  - id: staffed\n    ranges: [{{days: [{closed_day}], start: '00:00', end: '24:00'}}]"
        ),
    );
    observation(&dir, 2, "first problem outside hours");
    let mut app = Application::start(&path);
    app.page(SERVICE, "first problem outside hours");
    app.page(DELIVERY, "Outside notification window");
    check(&mut app, &dir, 0, "recovered outside hours");
    check(&mut app, &dir, 1, "new problem outside hours");
    check(&mut app, &dir, 2, "still current on reopening");
    relay.expect_quiet(Duration::from_millis(400));

    let closed = fs::read_to_string(&path).unwrap();
    let opened = closed.replace(
        "notification_window: staffed",
        "notification_window: always",
    );
    fs::write(&path, &opened).unwrap();
    app.reload("reloaded");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(451);
    app.page(DELIVERY, "<td>retrying</td><td>1</td>");

    fs::write(&path, closed).unwrap();
    app.reload("reloaded");
    app.page(DELIVERY, "Outside notification window");
    relay.expect_quiet(Duration::from_millis(3300));
    fs::write(&path, opened).unwrap();
    app.reload("reloaded");
    relay
        .receive("observer@example.invalid", "Confirmed problem: CRITICAL")
        .respond(250);
    app.page(DELIVERY, "<td>delivered</td><td>2</td>");
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
async fn silence_expiry_releases_only_current_eligible_notifications() {
    use chrono::{TimeDelta, Utc};
    use ragios::{
        config::Probe,
        runner::Status::{Critical, Ok as Healthy, Unknown, Warning},
        store::{CheckTarget, CreateSilence, DeliveryOutcome, Silence, SilenceScope, Store},
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
    with_reachability(&path, "ping", "check", &[]);
    // Use the configured default group, whatever its internal ID is.
    let config = single_attempt_config(&path);
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let zero = Utc::now();
    let at = |minute| zero + TimeDelta::minutes(minute);
    let service = || CheckTarget::Service("application".into());
    let host = || CheckTarget::Host(Probe::Ping);
    let next = |store: Store, minute| {
        let active = active.clone();
        async move {
            store
                .next_notification_at(active, at(minute))
                .await
                .unwrap()
        }
    };
    let begin = |store: Store, notification, minute| {
        let active = active.clone();
        let config = config.clone();
        async move {
            store
                .begin_notification_at(notification, active, config, at(minute))
                .await
                .unwrap()
        }
    };
    let deliver = async |store: Store, minute, label: &'static str| {
        let queued = next(store.clone(), minute).await.unwrap();
        let sending = begin(store.clone(), queued, minute).await.unwrap();
        assert_eq!(sending.label(), label);
        store
            .finish_notification_at(sending, DeliveryOutcome::Delivered, at(minute))
            .await
            .unwrap();
    };
    let create = |store: Store, scope, minute| {
        let active = active.clone();
        async move {
            assert!(matches!(
                store
                    .create_silence(
                        Silence {
                            scope,
                            expires_at: at(minute),
                            reason: None
                        },
                        active
                    )
                    .await
                    .unwrap(),
                CreateSilence::Created
            ));
        }
    };
    let store = Store::open_server(&config.database).unwrap();
    save_configured_observation(&store, config.clone(), service(), Critical, at(0)).await;
    deliver(store.clone(), 1, "Confirmed problem: CRITICAL").await;
    // Simulate a candidate picked immediately before a host silence was created.
    save_configured_observation(&store, config.clone(), host(), Unknown, at(1)).await;
    let candidate = next(store.clone(), 2).await.unwrap();
    create(store.clone(), SilenceScope::Host("local".into()), 10).await;
    assert!(begin(store.clone(), candidate, 2).await.is_none());
    save_configured_observation(&store, config.clone(), service(), Warning, at(3)).await;
    assert!(next(store.clone(), 4).await.is_none());

    create(
        store.clone(),
        SilenceScope::Group(config.hosts[0].group.clone()),
        20,
    )
    .await;
    create(
        store.clone(),
        SilenceScope::Service {
            host: "local".into(),
            service: "application".into(),
        },
        70,
    )
    .await;
    save_configured_observation(&store, config.clone(), host(), Healthy, at(5)).await;
    save_configured_observation(&store, config.clone(), host(), Unknown, at(6)).await;
    save_configured_observation(&store, config.clone(), service(), Critical, at(7)).await;
    drop(store);
    let store = Store::open_server(&config.database).unwrap();
    store.resume_notifications().await.unwrap();
    assert!(
        next(store.clone(), 10).await.is_none(),
        "group still holds reachability"
    );
    deliver(store.clone(), 20, "Confirmed problem: Undetermined").await;
    assert!(
        next(store.clone(), 20).await.is_none(),
        "service silence still holds the change"
    );
    assert!(next(store.clone(), 69).await.is_none());
    let current = next(store.clone(), 70).await.unwrap();
    assert!(
        begin(store.clone(), current, 70).await.is_none(),
        "severity already delivered"
    );
    deliver(store.clone(), 70, "Reminder: CRITICAL").await;
    assert!(
        next(store.clone(), 70).await.is_none(),
        "no obsolete transitions or reminders"
    );
    save_configured_observation(&store, config.clone(), service(), Healthy, at(71)).await;
    deliver(store.clone(), 72, "Recovery: OK").await;
}

#[tokio::test]
async fn reminder_cadence_and_queued_delivery_survive_restart_and_host_suppression() {
    use chrono::{TimeDelta, Utc};
    use ragios::{
        config::Probe,
        runner::Status::{Critical, Ok as Healthy, Warning},
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
    with_reachability(&path, "ping", "check", &[]);
    let config = single_attempt_config(&path);
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let initial = Utc::now();
    let at = |seconds| initial + TimeDelta::seconds(seconds);
    let host = || CheckTarget::Host(Probe::Ping);
    let service = || CheckTarget::Service("application".into());
    let mut store = Store::open_server(&config.database).unwrap();
    save_configured_observation(&store, config.clone(), service(), Critical, initial).await;

    let deliver = |store: Store, seconds, label: &'static str| {
        let active = active.clone();
        let config = config.clone();
        async move {
            let next = store
                .next_notification_at(active.clone(), at(seconds))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(next.label(), label);
            let sending = store
                .begin_notification(next, active, config)
                .await
                .unwrap()
                .unwrap();
            store
                .finish_notification_at(sending, DeliveryOutcome::Delivered, at(seconds))
                .await
                .unwrap();
        }
    };
    deliver(store.clone(), 1, "Confirmed problem: CRITICAL").await;
    drop(store);
    store = Store::open_server(&config.database).unwrap();
    store.resume_notifications().await.unwrap();
    assert!(
        store
            .next_notification_at(active.clone(), at(3600))
            .await
            .unwrap()
            .is_none()
    );
    let pending = store
        .next_notification_at(active.clone(), at(3601))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.label(), "Reminder: CRITICAL");
    drop(store);
    store = Store::open_server(&config.database).unwrap();
    store.resume_notifications().await.unwrap();
    assert_eq!(
        store
            .next_notification_at(active.clone(), at(3601))
            .await
            .unwrap()
            .unwrap()
            .event_id,
        pending.event_id
    );

    save_configured_observation(&store, config.clone(), host(), Critical, at(3601)).await;
    assert!(
        store
            .begin_notification(pending, active.clone(), config.clone())
            .await
            .unwrap()
            .is_none()
    );
    deliver(store.clone(), 3602, "Confirmed problem: Down").await;
    save_configured_observation(&store, config.clone(), service(), Warning, at(3700)).await;
    save_configured_observation(&store, config.clone(), service(), Critical, at(3800)).await;
    deliver(store.clone(), 10_000, "Reminder: Down").await;
    assert!(
        store
            .next_notification_at(active.clone(), at(10_000))
            .await
            .unwrap()
            .is_none()
    );
    save_configured_observation(&store, config.clone(), host(), Healthy, at(10_001)).await;
    // Older released mail gets a dispatch turn before newer unheld mail.
    let due = store
        .next_notification_at(active.clone(), at(10_001))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(due.label(), "Confirmed severity change: CRITICAL");
    // Reconciliation drops a return to the last delivered severity and rearms
    // exactly one overdue reminder, not the intermediate severity changes.
    assert!(
        store
            .begin_notification(due, active.clone(), config.clone())
            .await
            .unwrap()
            .is_none()
    );
    deliver(store.clone(), 10_001, "Recovery: Reachable").await;
    deliver(store.clone(), 10_001, "Reminder: CRITICAL").await;
    assert!(
        store
            .next_notification_at(active.clone(), at(10_002))
            .await
            .unwrap()
            .is_none()
    );
    save_configured_observation(&store, config.clone(), service(), Healthy, at(10_003)).await;
    deliver(store.clone(), 10_003, "Recovery: OK").await;
    assert!(
        store
            .next_notification_at(active, at(20_000))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn recovery_during_rollback_clears_acknowledgement_before_reupgrade() {
    use chrono::Utc;
    use ragios::{
        config::Config,
        runner::Status::Critical,
        store::{AcknowledgeOutcome, Store},
    };

    for confirmed_during_rollback in [false, true] {
        let dir = TempDir::new().unwrap();
        let path = config(&dir, "confirmation_attempts: 1");
        let config = Arc::new(Config::load(&path).unwrap());
        let (_publisher, active) = tokio::sync::watch::channel(config.clone());
        let key = ("local".to_owned(), "application".to_owned());
        let store = Store::open(&config.database).unwrap();
        save_configured_observation(
            &store,
            config.clone(),
            application_target(),
            Critical,
            Utc::now(),
        )
        .await;
        let original = store.latest().await.unwrap()[&key]
            .episode
            .as_ref()
            .unwrap()
            .token
            .clone();
        assert!(matches!(
            store
                .acknowledge_problem(
                    active.clone(),
                    key.0.clone(),
                    Some(key.1.clone()),
                    original.clone()
                )
                .await
                .unwrap(),
            AcknowledgeOutcome::Acknowledged
        ));
        drop(store);

        // The previous binary updates check_problems but never writes to the
        // episode table. Reproduce its confirmed-state writes directly.
        let legacy = rusqlite::Connection::open(&config.database).unwrap();
        let write_severity = |severity: Option<i32>| {
            legacy
                .execute(
                    "UPDATE check_problems SET confirmed_severity = ?1
                 WHERE check_kind = 'service' AND host_id = ?2 AND service_id = ?3",
                    rusqlite::params![severity, key.0, key.1],
                )
                .unwrap();
        };
        write_severity(None);
        if confirmed_during_rollback {
            write_severity(Some(2));
        }
        drop(legacy);

        let store = Store::open(&config.database).unwrap();
        if !confirmed_during_rollback {
            assert!(store.latest().await.unwrap()[&key].episode.is_none());
            save_configured_observation(
                &store,
                config.clone(),
                application_target(),
                Critical,
                Utc::now(),
            )
            .await;
        }
        let latest = store.latest().await.unwrap();
        let successor = latest[&key].episode.as_ref().unwrap();
        assert!(!successor.acknowledged);
        assert_ne!(successor.token, original);
    }
}

#[tokio::test]
async fn acknowledgement_blocks_an_already_selected_reminder_at_dispatch() {
    use chrono::{TimeDelta, Utc};
    use ragios::{
        runner::Status::{Critical, Warning},
        store::{AcknowledgeOutcome, DeliveryOutcome, Store},
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
    let config = single_attempt_config(&path);
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let store = Store::open_server(&config.database).unwrap();
    let observed_at = Utc::now();
    save_configured_observation(
        &store,
        config.clone(),
        application_target(),
        Critical,
        observed_at,
    )
    .await;
    let delivered_at = observed_at + TimeDelta::seconds(1);
    let initial = store
        .next_notification_at(active.clone(), delivered_at)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.label(), "Confirmed problem: CRITICAL");
    let initial = store
        .begin_notification_at(initial, active.clone(), config.clone(), delivered_at)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_notification_at(initial, DeliveryOutcome::Delivered, delivered_at)
        .await
        .unwrap();

    let due_at = delivered_at + TimeDelta::hours(1);
    let reminder = store
        .next_notification_at(active.clone(), due_at)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reminder.label(), "Reminder: CRITICAL");
    let host = "local".to_owned();
    let service = "application".to_owned();
    let latest = store.latest().await.unwrap();
    let episode = latest[&(host.clone(), service.clone())]
        .episode
        .as_ref()
        .unwrap();
    assert!(matches!(
        store
            .acknowledge_problem(active.clone(), host, Some(service), episode.token.clone())
            .await
            .unwrap(),
        AcknowledgeOutcome::Acknowledged
    ));
    assert!(
        store
            .begin_notification_at(reminder, active.clone(), config.clone(), due_at)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .next_notification_at(active.clone(), due_at + TimeDelta::hours(24))
            .await
            .unwrap()
            .is_none()
    );
    // The same Acknowledgement must not suppress a later confirmed change.
    let changed_at = due_at + TimeDelta::hours(24);
    save_configured_observation(
        &store,
        config.clone(),
        application_target(),
        Warning,
        changed_at,
    )
    .await;
    let changed = store
        .next_notification_at(active.clone(), changed_at)
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification_at(changed, active, config, changed_at)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sending.label(), "Confirmed severity change: WARNING");
}

#[tokio::test]
async fn retry_rechecks_window_at_dispatch_across_the_spring_clock_change() {
    use chrono::TimeDelta;
    use ragios::{runner::Status::Critical, store::DeliveryOutcome};

    let (_dir, store, config) = windowed_store("01:30", "02:30");
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    // 01:59:55 EST; the clock jumps from 01:59:59 EST to 03:00:00 EDT.
    let initial = utc("2099-03-08T06:59:55Z");
    save_configured_observation(
        &store,
        config.clone(),
        application_target(),
        Critical,
        initial,
    )
    .await;
    let first = store
        .next_notification_at(active.clone(), initial)
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification_at(first, active.clone(), config.clone(), initial)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_notification_at(
            sending,
            DeliveryOutcome::Retry("SMTP reply 451".into()),
            initial,
        )
        .await
        .unwrap();
    let retry_due = initial + TimeDelta::seconds(3);
    let retry = store
        .next_notification_at(active.clone(), retry_due)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.attempts, 1);
    let closed = utc("2099-03-08T07:00:00Z");
    assert!(
        store
            .begin_notification_at(retry, active.clone(), config.clone(), closed)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .next_notification_at(active.clone(), closed)
            .await
            .unwrap()
            .is_none()
    );

    let reopened = utc("2099-03-15T05:30:00Z"); // 01:30 EDT the following Sunday.
    assert!(
        store
            .next_notification_at(active.clone(), reopened - TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );
    let retry = store
        .next_notification_at(active.clone(), reopened)
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification_at(retry, active, config, reopened)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sending.attempts, 2);
    assert_eq!(sending.label(), "Confirmed problem: CRITICAL");
}

#[tokio::test]
async fn group_reload_records_holds_while_another_notification_is_in_flight() {
    use chrono::{TimeDelta, Utc};
    use ragios::{
        config::Probe,
        runner::Status::{Critical, Unknown, Warning},
        store::{CheckTarget, CreateSilence, DeliveryOutcome, Silence, SilenceScope, Store},
    };

    let dir = TempDir::new().unwrap();
    private_file(&dir, "username", USER);
    private_file(&dir, "password", PASSWORD);
    let path = config(
        &dir,
        &format!(
            "{}recipients: [observer@example.invalid]\ngroups: [{{id: ops, name: Ops}}]",
            smtp_yaml("127.0.0.1", 1, "implicit"),
        ),
    );
    with_reachability(&path, "ping", "check", &[]);
    let config = single_attempt_config(&path);
    let (published, active) = tokio::sync::watch::channel(config.clone());
    let store = Store::open_server(&config.database).unwrap();
    let now = Utc::now() + TimeDelta::seconds(1);
    save_configured_observation(&store, config.clone(), application_target(), Critical, now).await;
    let first = store
        .next_notification_at(active.clone(), now)
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification_at(first, active.clone(), config.clone(), now)
        .await
        .unwrap()
        .unwrap();
    store
        .finish_notification_at(sending, DeliveryOutcome::Delivered, now)
        .await
        .unwrap();

    save_configured_observation(
        &store,
        config.clone(),
        CheckTarget::Host(Probe::Ping),
        Unknown,
        now,
    )
    .await;
    let reachability_candidate = store
        .next_notification_at(active.clone(), now)
        .await
        .unwrap()
        .unwrap();
    let in_flight = store
        .begin_notification_at(reachability_candidate, active.clone(), config.clone(), now)
        .await
        .unwrap()
        .unwrap();
    save_configured_observation(&store, config.clone(), application_target(), Warning, now).await;
    save_configured_observation(&store, config.clone(), application_target(), Critical, now).await;
    let expiry = now + TimeDelta::minutes(1);
    assert!(matches!(
        store
            .create_silence(
                Silence {
                    scope: SilenceScope::Group("ops".into()),
                    expires_at: expiry,
                    reason: None,
                },
                active.clone()
            )
            .await
            .unwrap(),
        CreateSilence::Created
    ));

    let mut moved = (*config).clone();
    moved.hosts[0].group = "ops".into();
    let moved = Arc::new(moved);
    // No worker poll occurs during the Silence. Publication itself must save
    // the hold, even though the SMTP attempt has not finished yet.
    store
        .publish_notification_config(moved.clone(), published)
        .await
        .unwrap();
    assert_eq!(active.borrow().hosts[0].group, "ops");
    store
        .finish_notification_at(in_flight, DeliveryOutcome::Delivered, expiry)
        .await
        .unwrap();
    let pending = store
        .next_notification_at(active.clone(), expiry)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .begin_notification_at(pending, active.clone(), moved, expiry)
            .await
            .unwrap()
            .is_none(),
        "the recipient already knows CRITICAL; do not replay suppressed changes"
    );
    assert!(
        store
            .next_notification_at(active, expiry)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .recent_deliveries()
            .await
            .unwrap()
            .iter()
            .any(|record| record.diagnostic.as_deref()
                == Some("Current severity already delivered before suppression"))
    );
}

#[tokio::test]
async fn a_silence_releases_mail_only_when_the_notification_window_is_open() {
    use ragios::{
        runner::Status::Critical,
        store::{CreateSilence, Silence, SilenceScope},
    };

    let (_dir, store, config) = windowed_store("01:15", "01:45");
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let open = utc("2099-11-01T05:15:00Z");
    let expiry = utc("2099-11-01T05:50:00Z");
    let reopen = utc("2099-11-01T06:15:00Z");
    save_configured_observation(&store, config.clone(), application_target(), Critical, open).await;
    let candidate = store
        .next_notification_at(active.clone(), open)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        store
            .create_silence(
                Silence {
                    scope: SilenceScope::Host("local".into()),
                    expires_at: expiry,
                    reason: None,
                },
                active.clone()
            )
            .await
            .unwrap(),
        CreateSilence::Created
    ));
    assert!(
        store
            .begin_notification_at(candidate, active.clone(), config.clone(), open)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .next_notification_at(active.clone(), open)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .next_notification_at(active.clone(), expiry)
            .await
            .unwrap()
            .is_none()
    );
    let candidate = store
        .next_notification_at(active.clone(), reopen)
        .await
        .unwrap()
        .unwrap();
    let sending = store
        .begin_notification_at(candidate, active, config, reopen)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sending.label(), "Confirmed problem: CRITICAL");
}

#[tokio::test]
async fn reopening_reconciles_severity_reminders_and_recovery_during_fall_clock_change() {
    use chrono::TimeDelta;
    use ragios::{
        config::Probe,
        runner::Status::{Critical, Ok as Healthy, Warning},
        store::{CheckTarget, DeliveryOutcome, Store},
    };

    let (_dir, store, config) = windowed_store("01:15", "01:45");
    let (_publisher, active) = tokio::sync::watch::channel(config.clone());
    let save_service = |store: Store, status, at| {
        let config = config.clone();
        async move {
            save_configured_observation(&store, config, application_target(), status, at).await
        }
    };
    let next = |at| store.next_notification_at(active.clone(), at);
    let deliver = |store: Store, at, label: &'static str| {
        let active = active.clone();
        let config = config.clone();
        async move {
            let candidate = store
                .next_notification_at(active.clone(), at)
                .await
                .unwrap()
                .unwrap();
            let sending = store
                .begin_notification_at(candidate, active, config, at)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(sending.label(), label);
            store
                .finish_notification_at(sending, DeliveryOutcome::Delivered, at)
                .await
                .unwrap();
        }
    };
    let first_open = utc("2099-11-01T05:15:00Z"); // 01:15 EDT
    let first_close = utc("2099-11-01T05:45:00Z");
    let second_open = utc("2099-11-01T06:15:00Z"); // 01:15 EST
    save_service(store.clone(), Critical, first_open).await;
    assert!(
        next(first_open - TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );
    deliver(store.clone(), first_open, "Confirmed problem: CRITICAL").await;
    // A reminder is overdue, but the end is exclusive.
    assert!(next(first_close).await.unwrap().is_none());
    save_service(store.clone(), Warning, first_close).await;
    save_service(store.clone(), Critical, first_close + TimeDelta::seconds(1)).await;
    assert!(
        next(second_open - TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );

    let candidate = next(second_open).await.unwrap().unwrap();
    assert!(
        store
            .begin_notification_at(candidate, active.clone(), config.clone(), second_open)
            .await
            .unwrap()
            .is_none()
    ); // No severity change: the recipient already knows CRITICAL.

    // Even though the window is now open and the reminder is due, confirmed
    // host down holds service mail until reachability recovers.
    let host = || CheckTarget::Host(Probe::Ping);
    save_configured_observation(&store, config.clone(), host(), Critical, second_open).await;
    deliver(store.clone(), second_open, "Confirmed problem: Down").await;
    assert!(next(second_open).await.unwrap().is_none());
    save_configured_observation(
        &store,
        config.clone(),
        host(),
        Healthy,
        second_open + TimeDelta::seconds(1),
    )
    .await;
    deliver(
        store.clone(),
        second_open + TimeDelta::seconds(1),
        "Recovery: Reachable",
    )
    .await;
    deliver(
        store.clone(),
        second_open + TimeDelta::seconds(1),
        "Reminder: CRITICAL",
    )
    .await;
    assert!(
        next(second_open + TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );

    let second_close = utc("2099-11-01T06:45:00Z");
    save_service(store.clone(), Warning, second_close).await;
    save_service(store.clone(), Healthy, second_close + TimeDelta::seconds(1)).await;
    assert!(
        next(second_close + TimeDelta::seconds(1))
            .await
            .unwrap()
            .is_none()
    );
    let next_week = utc("2099-11-08T06:15:00Z");
    deliver(store.clone(), next_week, "Recovery: OK").await;
    assert!(next(next_week).await.unwrap().is_none());
}

fn utc(iso: &str) -> chrono::DateTime<chrono::Utc> {
    iso.parse().unwrap()
}

fn application_target() -> ragios::store::CheckTarget {
    ragios::store::CheckTarget::Service("application".into())
}

fn windowed_store(
    start: &str,
    end: &str,
) -> (TempDir, ragios::store::Store, Arc<ragios::config::Config>) {
    let dir = TempDir::new().unwrap();
    private_file(&dir, "username", USER);
    private_file(&dir, "password", PASSWORD);
    let path = config(
        &dir,
        &format!(
            "{}recipients: [observer@example.invalid]\nreminder_interval_seconds: 600\nnotification_window: staffed\ncheck_windows:\n  - id: staffed\n    timezone: America/New_York\n    ranges: [{{days: [sun], start: '{start}', end: '{end}'}}]",
            smtp_yaml("127.0.0.1", 1, "implicit"),
        ),
    );
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    yaml["hosts"][0]["reachability"] = serde_yaml::Mapping::from_iter([
        ("probe".into(), "ping".into()),
        ("executable".into(), "check".into()),
        ("notification_window".into(), "always".into()),
    ])
    .into();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    let config = single_attempt_config(&path);
    assert!(
        config.hosts[0]
            .reachability
            .as_ref()
            .unwrap()
            .notification_window
            .is_none()
    );
    let store = ragios::store::Store::open_server(&config.database).unwrap();
    (dir, store, config)
}

#[tokio::test]
async fn upgrade_reconciles_service_deliveries_for_a_host_already_confirmed_down() {
    use chrono::{TimeDelta, Utc};
    use ragios::{
        config::Probe,
        runner::Status::{Critical, Ok as Healthy, Unknown, Warning},
        store::{CheckTarget, DeliveryOutcome, Store},
    };

    for final_severity in [Critical, Unknown] {
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
        with_reachability(&path, "ping", "check", &[]);
        let config = single_attempt_config(&path);
        let (_publisher, active) = tokio::sync::watch::channel(config.clone());
        let store = Store::open_server(&config.database).unwrap();
        let at = Utc::now() + TimeDelta::minutes(1);
        let service = || CheckTarget::Service("application".into());
        save_configured_observation(&store, config.clone(), service(), Critical, at).await;
        let initial = store
            .next_notification_at(active.clone(), at)
            .await
            .unwrap()
            .unwrap();
        let sending = store
            .begin_notification(initial, active.clone(), config.clone())
            .await
            .unwrap()
            .unwrap();
        store
            .finish_notification_at(sending, DeliveryOutcome::Delivered, at)
            .await
            .unwrap();
        save_configured_observation(&store, config.clone(), service(), Warning, at).await;
        save_configured_observation(&store, config.clone(), service(), final_severity, at).await;
        // The old version persisted host state without enqueuing host mail.
        let mut without_mail = (*config).clone();
        without_mail.smtp = None;
        save_configured_observation(
            &store,
            Arc::new(without_mail),
            CheckTarget::Host(Probe::Ping),
            Critical,
            at,
        )
        .await;
        drop(store);
        rusqlite::Connection::open(&config.database)
            .unwrap()
            .execute_batch(
                "DROP INDEX notification_reminders_by_event;
             DROP INDEX notification_reminders_due;
             ALTER TABLE notification_events DROP COLUMN reminder_for;
             ALTER TABLE notification_deliveries DROP COLUMN remind_from;
             ALTER TABLE notification_deliveries DROP COLUMN held;",
            )
            .unwrap();

        let store = Store::open_server(&config.database).unwrap();
        // Recovery can be recorded before the delivery worker polls at startup.
        save_configured_observation(
            &store,
            config.clone(),
            CheckTarget::Host(Probe::Ping),
            Healthy,
            at,
        )
        .await;
        let pending = store
            .next_notification_at(active.clone(), at)
            .await
            .unwrap()
            .unwrap();
        let sending = store
            .begin_notification(pending, active.clone(), config.clone())
            .await
            .unwrap();
        if final_severity == Critical {
            assert!(
                sending.is_none(),
                "the accepted severity should not be sent again"
            );
            assert!(
                store
                    .next_notification_at(active.clone(), at)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                store
                    .next_notification_at(active, at + TimeDelta::hours(1))
                    .await
                    .unwrap()
                    .unwrap()
                    .label(),
                "Reminder: CRITICAL"
            );
        } else {
            assert_eq!(
                sending.unwrap().label(),
                "Confirmed severity change: UNKNOWN"
            );
        }
    }
}

fn single_attempt_config(path: &std::path::Path) -> Arc<ragios::config::Config> {
    let mut config = ragios::config::Config::load(path).unwrap();
    config.confirmation_attempts = 1;
    Arc::new(config)
}

async fn save_configured_observation(
    store: &ragios::store::Store,
    config: Arc<ragios::config::Config>,
    target: ragios::store::CheckTarget,
    status: ragios::runner::Status,
    at: chrono::DateTime<chrono::Utc>,
) {
    let timestamp = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    store
        .save_with_notifications(
            "local".into(),
            target,
            ragios::runner::CheckResult {
                status,
                output: String::new(),
                started_at: timestamp.clone(),
                finished_at: timestamp,
                duration_ms: 0,
            },
            config,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn dispatch_rechecks_confirmed_state_and_pruning_keeps_active_recovery_evidence() {
    use ragios::{
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
    let config = single_attempt_config(&path);
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
            config.clone(),
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

fn install_probe(dir: &TempDir, path: &Path) {
    let probe = dir.path().join("probe");
    fs::copy(dir.path().join("check"), &probe).unwrap();
    let script = fs::read_to_string(&probe).unwrap();
    fs::write(&probe, script.replace("< observation", "< reachability")).unwrap();
    with_reachability(path, "ping", "probe", &[]);
}

fn host_observation(dir: &TempDir, status: i32, text: &str) {
    fs::write(
        dir.path().join("reachability"),
        format!("{status} {text}\n"),
    )
    .unwrap();
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
    connections: Arc<AtomicUsize>,
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
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = connections.clone();
        let thread = thread::spawn(move || {
            while !running.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.fetch_add(1, Ordering::Relaxed);
                        handle_smtp(stream, mode, tls.clone(), &events);
                    }
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
            connections,
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
