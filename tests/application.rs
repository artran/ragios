#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Application {
    child: Child,
    url: String,
    client: reqwest::blocking::Client,
    logs: Option<thread::JoinHandle<()>>,
    stderr_lines: mpsc::Receiver<String>,
}

impl Application {
    fn start(config: &Path) -> Self {
        Self::start_with_env(config, &[])
    }

    fn start_with_env(config: &Path, environment: &[(&str, &str)]) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .no_proxy()
            .build()
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ragios"))
            .arg("--config")
            .arg(config)
            .envs(environment.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut app = Self {
            child,
            url: String::new(),
            client,
            logs: None,
            stderr_lines: receiver,
        };
        let stderr = app.child.stderr.take().unwrap();
        app.logs = Some(thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("{line}");
                let _ = sender.send(line);
            }
        }));
        let announcement = app
            .stderr_lines
            .recv_timeout(Duration::from_secs(5))
            .expect("server did not announce its bound listener");
        app.url = announcement
            .strip_prefix("Ragios listening on ")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        app
    }

    fn request_reload(&mut self) {
        while self.stderr_lines.try_recv().is_ok() {}
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGHUP,
        )
        .unwrap();
    }

    fn reload_outcome(&mut self, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let line = self
                .stderr_lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("server did not report reload outcome");
            if line.starts_with("Configuration reload") {
                assert!(line.contains(expected), "{line}");
                return line;
            }
        }
    }

    fn reload(&mut self, expected: &str) -> String {
        self.request_reload();
        self.reload_outcome(expected)
    }

    fn page(&mut self, path: &str, expected: &str) -> String {
        self.page_where(path, expected, |page| page.contains(expected))
    }

    fn page_where(&mut self, path: &str, expected: &str, matches: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = String::new();
        loop {
            assert!(self.child.try_wait().unwrap().is_none(), "server exited");
            if let Ok(response) = self.client.get(format!("{}{path}", self.url)).send() {
                assert!(response.status().is_success());
                last = response.text().unwrap();
                if matches(&last) {
                    return last;
                }
            }
            assert!(Instant::now() < deadline, "missing {expected:?}: {last}");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(logs) = self.logs.take() {
            let _ = logs.join();
        }
    }
}

fn plugin(dir: &TempDir, script: &str) {
    let path = dir.path().join("check");
    fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn config(dir: &TempDir, extra: &str) -> std::path::PathBuf {
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        format!(
            "bind: 127.0.0.1:0\nplugin_dir: .\ndatabase: state.sqlite3\n{extra}\nhosts:\n  - id: local\n    name: Local host\n    services:\n      - id: application\n        name: Application health\n        executable: check\n        args: []\n"
        ),
    )
    .unwrap();
    path
}

fn configure_concurrent_checks(
    dir: &TempDir,
    path: &Path,
    count: usize,
    allowed: usize,
    hold: f64,
) {
    let executable = dir.path().join("check");
    fs::write(&executable, include_str!("fixtures/check_concurrency.py")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let services: Vec<_> = (0..count)
        .map(|index| {
            let id = format!("check-{index}");
            serde_yaml::to_value(serde_yaml::Mapping::from_iter([
                ("id".into(), id.clone().into()),
                ("name".into(), id.clone().into()),
                ("executable".into(), "check".into()),
                (
                    "args".into(),
                    serde_yaml::to_value([
                        dir.path()
                            .join("endpoint-state")
                            .to_str()
                            .unwrap()
                            .to_owned(),
                        id,
                        allowed.to_string(),
                        hold.to_string(),
                    ])
                    .unwrap(),
                ),
            ]))
            .unwrap()
        })
        .collect();
    let mut services = services;
    if count > 1 {
        let other_services = services.split_off(count / 2);
        let mut other_host = yaml["hosts"][0].clone();
        other_host["id"] = "other-host".into();
        other_host["name"] = "Other host".into();
        other_host["services"] = other_services.into();
        yaml["hosts"].as_sequence_mut().unwrap().push(other_host);
    }
    yaml["hosts"][0]["services"] = services.into();
    fs::write(path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
}

fn with_reachability(path: &Path, probe: &str, executable: &str, args: &[&str]) {
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    yaml["hosts"][0]["reachability"] = serde_yaml::to_value(serde_yaml::Mapping::from_iter([
        ("probe".into(), probe.into()),
        ("executable".into(), executable.into()),
        ("args".into(), serde_yaml::to_value(args).unwrap()),
    ]))
    .unwrap();
    fs::write(path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
}

fn row_for<'a>(page: &'a str, name: &str) -> &'a str {
    page.split("<tr")
        .skip(1)
        .find(|row| row.contains(name))
        .unwrap()
        .split("</tr>")
        .next()
        .unwrap()
}

#[test]
fn reachable_host_and_broken_service_are_displayed_independently() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        "if [ \"$1\" = host ]; then printf '<degraded probe> %s\\n' \"$2\"; exit 1; fi\nprintf 'broken service\\n'; exit 2",
    );
    let path = config(&dir, "confirmation_attempts: 1\nmax_concurrent_checks: 1");
    with_reachability(&path, "tcp", "check", &["host", "target $(literal)"]);
    let mut app = Application::start(&path);
    let page = app.page_where("/", "reachable host and broken service", |page| {
        page.contains("Reachable (degraded; WARNING)") && page.contains("broken service")
    });
    let host = row_for(&page, "Host reachability");
    assert!(host.contains("tcp"));
    assert!(host.contains("&lt;degraded probe&gt; target $(literal)"));
    assert!(host.contains("No confirmed problem"));
    assert!(host.contains("No pending problem"));
    let service = row_for(&page, "Application health");
    assert!(service.contains("Confirmed problem: CRITICAL"));
    assert!(page.contains("does not prove the machine is powered off"));
}

#[test]
fn host_status_mappings_and_runner_failures_never_gate_service_checks() {
    for (name, body, assessment, confirmed, diagnostic) in [
        ("ok", "exit 0", "Reachable (OK)", "No confirmed problem", ""),
        (
            "warning",
            "exit 1",
            "Reachable (degraded; WARNING)",
            "No confirmed problem",
            "",
        ),
        (
            "critical",
            "exit 2",
            "Down (CRITICAL)",
            "Confirmed problem: Down",
            "",
        ),
        (
            "unknown",
            "exit 3",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "",
        ),
        (
            "unexpected",
            "exit 42",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "unexpected exit",
        ),
        (
            "signal",
            "kill -TERM $$",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "unexpected exit",
        ),
        (
            "missing",
            "exit 0",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "Runner failure:",
        ),
        (
            "permission",
            "exit 0",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "Runner failure:",
        ),
        (
            "timeout",
            "exec sleep 20",
            "Undetermined (UNKNOWN)",
            "Confirmed problem: Undetermined",
            "timed out after 1 seconds",
        ),
    ] {
        let dir = TempDir::new().unwrap();
        let counter = dir.path().join("service-counter");
        plugin(
            &dir,
            &format!(
                "if [ \"$1\" = host ]; then printf '<host probe>\\n'; {body}; fi\nn=0\nif [ -f '{counter}' ]; then n=$(cat '{counter}'); fi\nn=$((n+1))\nprintf '%s' \"$n\" > '{counter}'\nprintf 'service observation %s\\n' \"$n\"\nexit 0",
                counter = counter.display()
            ),
        );
        let path = config(
            &dir,
            "interval_seconds: 1\nretry_interval_seconds: 1\nconfirmation_attempts: 1\ntimeout_seconds: 1\nmax_concurrent_checks: 1",
        );
        let executable = match name {
            "missing" => "not-installed",
            "permission" => {
                fs::write(dir.path().join("denied"), "#!/bin/sh\nexit 0").unwrap();
                fs::set_permissions(dir.path().join("denied"), fs::Permissions::from_mode(0o644))
                    .unwrap();
                "denied"
            }
            _ => "check",
        };
        with_reachability(&path, "ping", executable, &["host"]);
        let mut app = Application::start(&path);
        let observed = app.page("/status", assessment);
        let observed_service = row_for(&observed, "Application health");
        let iteration = observed_service
            .split("service observation ")
            .nth(1)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let next_service = format!("service observation {}", iteration + 1);
        let page = app.page_where("/status", name, |page| {
            page.contains(assessment) && page.contains(&next_service)
        });
        let host = row_for(&page, "Host reachability");
        assert!(host.contains(confirmed), "{name}: {host}");
        assert!(host.contains(diagnostic), "{name}: {host}");
        assert!(host.contains("No pending problem"), "{name}: {host}");
        let service = row_for(&page, "Application health");
        assert!(
            service.contains("<td class=\"OK\">OK</td>"),
            "{name}: {service}"
        );
        assert!(
            service.contains("No confirmed problem"),
            "{name}: {service}"
        );
    }
}

#[test]
fn host_confirmation_and_pending_changes_survive_restart_beside_service_problems() {
    let dir = TempDir::new().unwrap();
    let counter = dir.path().join("host-counter");
    let gate = dir.path().join("host-release");
    plugin(
        &dir,
        &format!(
            r#"if [ "$1" != host ]; then echo 'persistent broken service'; exit 2; fi
n=0
if [ -f '{counter}' ]; then n=$(cat '{counter}'); fi
printf '%s' "$((n+1))" > '{counter}'
while [ ! -f '{gate}-'"$n" ]; do [ -d '{directory}' ] || exit 3; sleep 0.02; done
printf 'host observation %s complete\n' "$n"
case "$n" in
  1|4) exit 3;;
  *) exit 2;;
esac"#,
            counter = counter.display(),
            gate = gate.display(),
            directory = dir.path().display()
        ),
    );
    let path = config(
        &dir,
        "interval_seconds: 3\nretry_interval_seconds: 1\nconfirmation_attempts: 2",
    );
    with_reachability(&path, "ping", "check", &["host"]);
    let mut app = Application::start(&path);
    let initial = app.page("/status", "Host reachability");
    assert!(row_for(&initial, "Host reachability").contains("Not yet checked"));
    let mut saved_host = String::new();
    for (n, confirmed, pending) in [
        (0, "No confirmed problem", "Pending problem: Down (1/2)"),
        (1, "Confirmed problem: Undetermined", "No pending problem"),
        (
            2,
            "Confirmed problem: Undetermined",
            "Pending severity change: Down (1/2)",
        ),
        (3, "Confirmed problem: Down", "No pending problem"),
        (
            4,
            "Confirmed problem: Down",
            "Pending severity change: Undetermined (1/2)",
        ),
    ] {
        fs::write(format!("{}-{n}", gate.display()), "").unwrap();
        let page = app.page("/status", &format!("host observation {n} complete"));
        let host = row_for(&page, "Host reachability");
        assert!(host.contains(confirmed), "{host}");
        assert!(host.contains(pending), "{host}");
        saved_host = host.to_owned();
    }
    let page = app.page("/status", "Confirmed problem: CRITICAL");
    assert!(row_for(&page, "Application health").contains("persistent broken service"));
    drop(app);

    let release = dir.path().join("release-after-restart");
    plugin(
        &dir,
        &format!(
            r#"while [ ! -f '{release}' ]; do [ -d '{directory}' ] || exit 3; sleep 0.02; done
if [ "$1" = host ]; then echo 'degraded host recovery'; exit 1; fi
echo 'service still broken'; exit 2"#,
            release = release.display(),
            directory = dir.path().display()
        ),
    );
    let mut restarted = Application::start(&path);
    let restored = restarted.page("/status", "host observation 4 complete");
    assert_eq!(row_for(&restored, "Host reachability"), saved_host);
    assert!(row_for(&restored, "Application health").contains("Confirmed problem: CRITICAL"));
    fs::write(release, "").unwrap();
    let recovered = restarted.page("/status", "degraded host recovery");
    let host = row_for(&recovered, "Host reachability");
    assert!(host.contains("Reachable (degraded; WARNING)"));
    assert!(host.contains("No confirmed problem"));
    assert!(host.contains("No pending problem"));
    assert!(row_for(&recovered, "Application health").contains("Confirmed problem: CRITICAL"));
}

#[test]
fn saved_host_results_keep_their_probe_origin_after_selection_changes() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "echo 'saved ping assessment'; exit 2");
    // A one-second interval makes the saved result due again after restart.
    let path = config(&dir, "confirmation_attempts: 1\ninterval_seconds: 1");
    with_reachability(&path, "ping", "check", &[]);
    let mut first = Application::start(&path);
    first.page("/status", "Confirmed problem: Down");
    drop(first);
    let release = dir.path().join("release");
    plugin(
        &dir,
        &format!(
            "while [ ! -f '{}' ]; do [ -d '{}' ] || exit 3; sleep 0.02; done\necho 'new TCP assessment'; exit 0",
            release.display(),
            dir.path().display()
        ),
    );
    with_reachability(&path, "tcp", "check", &[]);
    let mut restarted = Application::start(&path);
    let restored = restarted.page("/status", "saved ping assessment");
    let host = row_for(&restored, "Host reachability");
    assert!(host.contains("Configured probe: tcp"), "{host}");
    assert!(host.contains("Latest probe: ping"), "{host}");
    assert!(host.contains("Confirmed problem: Down"));
    fs::write(release, "").unwrap();
    let updated = restarted.page("/status", "new TCP assessment");
    let host = row_for(&updated, "Host reachability");
    assert!(host.contains("Latest probe: tcp"), "{host}");
    assert!(host.contains("Reachable (OK)"));
    assert!(host.contains("No confirmed problem"));
}

#[test]
fn host_and_service_checks_share_the_same_global_execution_limit() {
    let dir = TempDir::new().unwrap();
    let path = config(&dir, "max_concurrent_checks: 2");
    configure_concurrent_checks(&dir, &path, 4, 2, 0.5);
    let endpoint = dir.path().join("endpoint-state");
    with_reachability(
        &path,
        "tcp",
        "check",
        &[endpoint.to_str().unwrap(), "host-probe", "2", "0.5"],
    );
    let mut app = Application::start(&path);
    let page = app.page_where("/status", "all host and service checks completed", |page| {
        page.contains("completed=5 ") && !page.contains("Not yet checked")
    });
    assert!(page.contains("Reachable (OK)"));
    assert!(page.contains("peak=2 "));
    assert!(!page.contains("exceeded=True"));
    assert!(!page.contains("overlap=True"));
    assert_eq!(page.matches("class=\"OK\"").count(), 5);
}

#[test]
fn global_concurrency_is_bounded_by_default_and_configured_limits() {
    for (limits, allowed, count) in [
        ("", 20, 24),
        ("max_concurrent_checks: 2\ntimeout_seconds: 3", 2, 6),
    ] {
        let dir = TempDir::new().unwrap();
        let path = config(&dir, limits);
        configure_concurrent_checks(&dir, &path, count, allowed, 1.5);
        let mut app = Application::start(&path);
        let completed = format!("completed={count} ");
        let result = app.page_where("/status", "all checks completed", |page| {
            page.contains(&completed) && !page.contains("Not yet checked")
        });
        assert!(!result.contains("exceeded=True"), "{result}");
        assert!(!result.contains("overlap=True"), "{result}");
        assert!(!result.contains("Not yet checked"), "{result}");
        assert_eq!(result.matches("class=\"OK\"").count(), count);
        assert!(result.contains(&format!("peak={allowed} ")), "{result}");
    }
}

#[test]
fn long_checks_never_overlap_or_catch_up_missed_intervals() {
    let dir = TempDir::new().unwrap();
    let path = config(&dir, "interval_seconds: 1");
    configure_concurrent_checks(&dir, &path, 1, 1, 2.2);
    let started = Instant::now();
    let mut app = Application::start(&path);
    let result = app.page("/status", "completed=2 ");
    assert!(result.contains("peak=1 overlap=False exceeded=False"));
    assert!(result.contains("class=\"OK\""));
    assert!(started.elapsed() >= Duration::from_secs(5));
    // The next interval is a delay after completion, not queued overdue work.
    let stable = result.clone();
    thread::sleep(Duration::from_millis(500));
    assert_eq!(app.page("/status", "completed=2 "), stable);
}

#[test]
fn healthy_checks_keep_progressing_alongside_hung_and_endlessly_noisy_plugins() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        r#"case "$1" in
  hung) exec sleep 20;;
  noisy) exec yes noisy-output;;
  *)
    n=0
    if [ -f "$1" ]; then n=$(cat "$1"); fi
    n=$((n+1))
    printf '%s' "$n" > "$1"
    printf 'healthy iteration %s\n' "$n";;
esac"#,
    );
    let path = config(
        &dir,
        "timeout_seconds: 1\ninterval_seconds: 1\nmax_concurrent_checks: 2\nmax_output_bytes: 128",
    );
    let mut yaml = fs::read_to_string(&path)
        .unwrap()
        .replace("args: []", "args: [hung]");
    yaml.push_str(&format!(
        "      - id: noisy\n        name: Noisy\n        executable: check\n        args: [noisy]\n      - id: healthy\n        name: Healthy\n        executable: check\n        args: ['{}']\n",
        dir.path().join("iterations").display()
    ));
    fs::write(&path, yaml).unwrap();
    let started = Instant::now();
    let mut app = Application::start(&path);
    let result = app.page("/status", "healthy iteration 2");
    assert!(started.elapsed() < Duration::from_secs(7));
    assert_eq!(
        result.matches("<td class=\"UNKNOWN\">UNKNOWN</td>").count(),
        2
    );
    assert_eq!(result.matches("<td class=\"OK\">OK</td>").count(), 1);
    assert!(result.contains("execution timed out after 1 seconds"));
    assert!(result.contains("[output truncated at 128 bytes]"));
    assert!(result.len() < 4000, "noisy output must stay bounded");
}

#[test]
fn timeout_cleans_descendants_even_after_the_direct_plugin_has_exited() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("descendant-ran");
    plugin(
        &dir,
        &format!(
            r#"if [ "$1" = observer ]; then
  sleep 2
  if [ -f '{}' ]; then echo 'descendant leaked'; exit 2; fi
  echo 'descendants stopped'; exit 0
fi
(sleep 2; touch '{}') &
echo 'direct child finished'
exit 0"#,
            marker.display(),
            marker.display()
        ),
    );
    let path = config(
        &dir,
        "timeout_seconds: 1\nmax_concurrent_checks: 1\nretry_interval_seconds: 1",
    );
    // The observer needs a longer deadline than the plugin under test. It polls
    // the marker after restart so both configurations retain the same limits.
    // A short retry interval makes the pending UNKNOWN due again after restart.
    let started = Instant::now();
    let mut app = Application::start(&path);
    let result = app.page("/status", "execution timed out after 1 seconds");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(result.contains("direct child finished"));
    assert!(result.contains("class=\"UNKNOWN\""));
    drop(app);
    let yaml = fs::read_to_string(&path)
        .unwrap()
        .replace("timeout_seconds: 1", "timeout_seconds: 5")
        .replace("args: []", "args: [observer]");
    fs::write(&path, yaml).unwrap();
    let mut app = Application::start(&path);
    let result = app.page("/status", "descendants stopped");
    assert!(result.contains("class=\"OK\""));
}

#[test]
fn installed_python_plugins_keep_their_runtime_dependencies_and_environment() {
    let dir = TempDir::new().unwrap();
    let dependency_dir = dir.path().join("installed dependencies");
    fs::create_dir(&dependency_dir).unwrap();
    fs::write(
        dependency_dir.join("probe_dependency.py"),
        "MESSAGE = 'dependency available'\n",
    )
    .unwrap();
    plugin(&dir, "exit 0");
    fs::write(dir.path().join("check"), "#!/usr/bin/env python3\nimport os\nfrom probe_dependency import MESSAGE\nprint(MESSAGE + ': ' + os.environ['RAGIOS_TEST_VALUE'])\n").unwrap();
    let path = config(&dir, "");
    let mut app = Application::start_with_env(
        &path,
        &[
            ("PYTHONPATH", dependency_dir.to_str().unwrap()),
            ("RAGIOS_TEST_VALUE", "literal $value with spaces; intact"),
        ],
    );
    let result = app.page(
        "/status",
        "dependency available: literal $value with spaces; intact",
    );
    assert!(result.contains("class=\"OK\""));
}

#[test]
fn problem_lifecycle_is_rendered_and_survives_restart() {
    let dir = TempDir::new().unwrap();
    // One-second intervals make each saved result due again after restart.
    let path = config(&dir, "interval_seconds: 1\nretry_interval_seconds: 1");
    let gate = dir.path().join("release");
    let mut previous: Option<(String, String)> = None;
    for (n, (code, confirmed, pending)) in [
        (0, "No confirmed problem", "No pending problem"),
        (1, "No confirmed problem", "Pending problem: WARNING (1/3)"),
        (2, "No confirmed problem", "Pending problem: CRITICAL (2/3)"),
        (3, "Confirmed problem: UNKNOWN", "No pending problem"),
        (
            1,
            "Confirmed problem: UNKNOWN",
            "Pending severity change: WARNING (1/3)",
        ),
        (
            1,
            "Confirmed problem: UNKNOWN",
            "Pending severity change: WARNING (2/3)",
        ),
        (
            2,
            "Confirmed problem: UNKNOWN",
            "Pending severity change: CRITICAL (1/3)",
        ),
        (3, "Confirmed problem: UNKNOWN", "No pending problem"),
        (
            2,
            "Confirmed problem: UNKNOWN",
            "Pending severity change: CRITICAL (1/3)",
        ),
        (
            2,
            "Confirmed problem: UNKNOWN",
            "Pending severity change: CRITICAL (2/3)",
        ),
        (2, "Confirmed problem: CRITICAL", "No pending problem"),
        (0, "No confirmed problem", "No pending problem"),
        (1, "No confirmed problem", "Pending problem: WARNING (1/3)"),
        (0, "No confirmed problem", "No pending problem"),
    ]
    .into_iter()
    .enumerate()
    {
        let _ = fs::remove_file(&gate);
        plugin(
            &dir,
            &format!(
                // Consuming the gate holds later executions, so each restart
                // records exactly one observation.
                "while [ ! -f '{gate}' ]; do [ -d '{dir}' ] || exit 3; sleep 0.02; done\nrm '{gate}'\nprintf 'observation {n} complete\\n'\nexit {code}",
                gate = gate.display(),
                dir = dir.path().display()
            ),
        );
        let mut app = Application::start(&path);
        if let Some((marker, saved)) = &previous {
            assert_eq!(
                &app.page("/status", marker),
                saved,
                "persisted dashboard changed on restart"
            );
        } else {
            app.page("/status", "Not yet checked");
        }
        fs::write(&gate, "").unwrap();
        let marker = format!("observation {n} complete");
        let rendered = app.page("/status", &marker);
        assert!(
            rendered.contains(confirmed),
            "missing {confirmed}: {rendered}"
        );
        assert!(rendered.contains(pending), "missing {pending}: {rendered}");
        let latest = ["OK", "WARNING", "CRITICAL", "UNKNOWN"][code as usize];
        assert!(rendered.contains(&format!("<td class=\"{latest}\">{latest}</td>")));
        previous = Some((marker, rendered));
    }
}

#[test]
fn configurable_attempts_use_retry_timing_only_while_confirmation_is_pending() {
    let dir = TempDir::new().unwrap();
    let counter = dir.path().join("counter");
    plugin(
        &dir,
        &format!(
            r#"n=0
if [ -f '{counter}' ]; then n=$(cat '{counter}'); fi
printf '%s' "$((n+1))" > '{counter}'
printf 'scheduled observation %s complete\n' "$n"
case "$n" in
  0|5) exit 1;;
  1|3) exit 2;;
  2) exit 3;;
  *) exit 0;;
esac"#,
            counter = counter.display()
        ),
    );
    let path = config(
        &dir,
        "interval_seconds: 3\nretry_interval_seconds: 1\nconfirmation_attempts: 2",
    );
    let mut app = Application::start(&path);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
    let mut previous_finished = None;
    for (n, (expected, delay)) in [
        ("Pending problem: WARNING (1/2)", 0),
        ("Confirmed problem: CRITICAL", 1),
        ("Pending severity change: UNKNOWN (1/2)", 3),
        ("No pending problem", 1),
        ("No confirmed problem", 3),
        ("Pending problem: WARNING (1/2)", 3),
        ("No pending problem", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let rendered = app.page("/status", &format!("scheduled observation {n} complete"));
        assert!(rendered.contains(expected), "{rendered}");
        if n == 2 || n == 3 {
            assert!(
                rendered.contains("Confirmed problem: CRITICAL"),
                "{rendered}"
            );
        }
        let latest = runtime.block_on(store.latest()).unwrap();
        let saved = &latest[&("local".into(), "application".into())].result;
        let started = chrono::DateTime::parse_from_rfc3339(&saved.started_at).unwrap();
        if let Some(finished) = previous_finished {
            let gap = started.signed_duration_since(finished).num_milliseconds();
            assert!(
                gap >= delay * 1000 - 20,
                "observation {n}: {gap} ms, expected {delay}s"
            );
            if delay == 1 {
                assert!(gap < 2800, "retry waited for the normal interval: {gap} ms");
            }
        }
        previous_finished = Some(chrono::DateTime::parse_from_rfc3339(&saved.finished_at).unwrap());
    }
}

#[test]
fn split_fleet_runs_resolved_checks_and_renders_stable_identities_by_primary_group() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        "printf 'target=%s port=%s format=%s\\n' \"$1\" \"$2\" \"$3\"\nexit 0",
    );
    fs::create_dir(dir.path().join("fleet")).unwrap();
    fs::write(
        dir.path().join("fleet/checks.yaml"),
        r#"
commands:
  - {id: probe, executable: check, args: ["{{address}}", "{{port}}", "{{{{.Names}}"]}
bundles:
  - id: web
    services:
      - {id: https, name: "HTTPS <response>", command: probe}
groups:
  - {id: prod, name: "Production <fleet>", recipients: [prod@example.invalid]}
  - {id: lab, name: Lab}
"#,
    )
    .unwrap();
    fs::write(
        dir.path().join("fleet/hosts.yaml"),
        r#"
hosts:
  - id: alpha
    name: Shared display name
    address: alpha.invalid
    group: prod
    bundles: [web]
    vars: {port: "443"}
  - id: beta
    name: Shared display name
    address: "beta.invalid; $(touch must-not-exist) | <tag>"
    group: lab
    bundles: [web]
    overrides:
      https:
        name: Custom HTTPS
        args: ["{{address}}", "8443", "{{{{.Names}}"]
"#,
    )
    .unwrap();
    let path = dir.path().join("ragios.yaml");
    fs::write(&path, "bind: 127.0.0.1:0\nplugin_dir: .\ndatabase: state.sqlite3\ninterval_seconds: 1\nrecipients: [global@example.invalid]\nincludes: [fleet/checks.yaml, fleet/hosts.yaml]\n").unwrap();
    let mut app = Application::start(&path);
    app.page("/status", "target=alpha.invalid port=443");
    let fragment = app.page("/status", "port=8443");
    assert!(fragment.contains("<th>Group</th>"));
    assert!(fragment.contains("Production &lt;fleet&gt;"));
    assert!(fragment.contains("<small>prod</small>"));
    assert!(fragment.contains("<small>lab</small>"));
    assert!(fragment.contains("<small>alpha</small>"));
    assert!(fragment.contains("<small>beta</small>"));
    assert_eq!(fragment.matches("<small>https</small>").count(), 2);
    assert!(fragment.contains("HTTPS &lt;response&gt;"));
    assert!(fragment.contains("Custom HTTPS"));
    assert_eq!(fragment.matches("format={{.Names}}").count(), 2);
    assert!(
        fragment.contains("target=beta.invalid; $(touch must-not-exist) | &lt;tag&gt; port=8443")
    );
    assert_eq!(fragment.matches("class=\"OK\"").count(), 2);
    drop(app);

    // Display names and include layout may change without losing persisted identities.
    let hosts = dir.path().join("fleet/hosts.yaml");
    let yaml = fs::read_to_string(&hosts)
        .unwrap()
        .replace("Shared display name", "Renamed host");
    fs::write(hosts, yaml).unwrap();
    let gate = dir.path().join("release");
    plugin(
        &dir,
        &format!(
            "while [ ! -f '{}' ]; do [ -d '{}' ] || exit 3; sleep 0.02; done\nprintf 'new result\\n'",
            gate.display(),
            dir.path().display()
        ),
    );
    let mut restarted = Application::start(&path);
    let restored = restarted.page("/status", "target=alpha.invalid port=443");
    assert!(restored.contains("Renamed host"));
    assert!(restored.contains("port=8443"));
    fs::write(gate, "").unwrap();
    restarted.page("/status", "new result");
}

#[test]
fn host_overrides_control_the_actual_execution_deadline_and_repeat_interval() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        r#"counter="$1"
n=0
if [ -f "$counter" ]; then n=$(cat "$counter"); fi
printf '%s' "$((n+1))" > "$counter"
printf 'fleet iteration %s\n' "$n"
exec sleep 20"#,
    );
    let path = dir.path().join("ragios.yaml");
    let yaml = r#"
bind: 127.0.0.1:0
plugin_dir: .
database: state.sqlite3
interval_seconds: 300
timeout_seconds: 30
# Confirm the timeout immediately so repeats use the normal interval.
confirmation_attempts: 1
bundles:
  - id: basic
    services:
      - {id: health, name: Health, executable: check}
hosts:
  - id: target
    name: Target
    bundles: [basic]
    vars: {counter: '__COUNTER__'}
    overrides:
      health:
        args: ['{{counter}}']
        interval_seconds: 1
        timeout_seconds: 1
"#;
    fs::write(
        &path,
        yaml.replace("__COUNTER__", dir.path().join("counter").to_str().unwrap()),
    )
    .unwrap();
    let mut app = Application::start(&path);
    let first = app.page("/status", "execution timed out after 1 seconds");
    assert!(first.contains("fleet iteration 0"));
    let repeated = app.page("/status", "fleet iteration 1");
    assert!(repeated.contains("execution timed out after 1 seconds"));
    assert!(repeated.contains("class=\"UNKNOWN\""));
}

#[test]
fn operating_system_assigned_listener_serves_the_configured_check() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "printf 'ephemeral listener works\\n'\nexit 0");
    let config = config(&dir, "");
    let mut app = Application::start(&config);
    let result = app.page("/status", "ephemeral listener works");
    assert!(result.contains("class=\"OK\""));
}

#[test]
fn periodic_checks_preserve_plugin_exit_codes_stderr_and_literal_arguments() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        r#"counter="$1"
n=0
if [ -f "$counter" ]; then n=$(cat "$counter"); fi
printf '%s' "$((n+1))" > "$counter"
printf 'iteration %s: %s\n' "$n" "$2"
printf 'diagnostic stderr\n' >&2
case "$n" in
  0) exit 0;;
  1) exit 1;;
  2) exit 2;;
  *) exit 3;;
esac"#,
    );
    let config = config(&dir, "interval_seconds: 1\nretry_interval_seconds: 1");
    let injection_marker = dir.path().join("must-not-exist");
    let literal = format!(
        "argument with spaces; $(touch {}) & | <tag>",
        injection_marker.display()
    );
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    yaml["hosts"][0]["services"][0]["args"] =
        serde_yaml::to_value([dir.path().join("counter").to_str().unwrap(), &literal]).unwrap();
    fs::write(&config, serde_yaml::to_string(&yaml).unwrap()).unwrap();
    let mut app = Application::start(&config);
    for (iteration, status) in [(0, "OK"), (1, "WARNING"), (2, "CRITICAL"), (3, "UNKNOWN")] {
        let result = app.page("/status", &format!("class=\"{status}\""));
        assert!(
            result.contains(&format!(
                "iteration {iteration}: argument with spaces; $(touch"
            )),
            "{result}"
        );
        assert!(result.contains("&amp; | &lt;tag&gt;"));
        assert!(result.contains("[stderr]\ndiagnostic stderr"));
        assert!(!result.contains("Runner failure"));
    }
    assert!(
        !injection_marker.exists(),
        "argument was interpreted by a shell"
    );
}

#[test]
fn output_exactly_at_the_limit_is_not_marked_as_truncated() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "printf abc\nprintf de >&2");
    let path = config(&dir, "max_output_bytes: 5");
    let mut app = Application::start(&path);
    let result = app.page("/status", "class=\"OK\"");
    assert!(result.contains("<pre>abc\n[stderr]\nde</pre>"), "{result}");
    assert!(!result.contains("truncated"));
}

#[test]
fn noisy_plugins_complete_with_a_shared_stdout_stderr_capture_limit() {
    for (limits, expected_bytes) in [("", 65_536), ("max_output_bytes: 1024", 1024)] {
        let dir = TempDir::new().unwrap();
        plugin(
            &dir,
            "head -c 100000 /dev/zero | tr '\\000' x\nhead -c 100000 /dev/zero | tr '\\000' y >&2\nexit 1",
        );
        let config = config(&dir, limits);
        let mut app = Application::start(&config);
        let result = app.page("/status", "class=\"WARNING\"");
        let output = result
            .split("<pre>")
            .nth(1)
            .unwrap()
            .split("</pre>")
            .next()
            .unwrap();
        let (captured, diagnostic) = output
            .split_once("\n[output truncated")
            .expect("truncation must be visible");
        assert_eq!(
            captured.matches('x').count() + captured.matches('y').count(),
            expected_bytes
        );
        assert!(diagnostic.contains(&format!("at {expected_bytes} bytes]")));
        assert!(output.len() <= expected_bytes + 100);
        assert!(!result.contains("Runner failure"));
    }
}

#[test]
fn latest_result_survives_restart_before_the_next_check_finishes() {
    let dir = TempDir::new().unwrap();
    // Fail once, then hold, so the saved result stays the latest one.
    let once = dir.path().join("failed-once");
    plugin(
        &dir,
        &format!(
            "if [ -f '{once}' ]; then while [ -d '{dir}' ]; do sleep 0.02; done; exit 3; fi\ntouch '{once}'\nprintf 'persisted problem\\n'\nexit 1",
            once = once.display(),
            dir = dir.path().display()
        ),
    );
    // A one-second retry interval makes the pending problem due again after restart.
    let path = config(&dir, "retry_interval_seconds: 1");
    let mut first = Application::start(&path);
    let saved = first.page("/status", "persisted problem");
    assert!(saved.contains("WARNING"));
    drop(first);

    let gate = dir.path().join("release");
    plugin(
        &dir,
        &format!(
            "while [ ! -f '{}' ]; do [ -d '{}' ] || exit 3; sleep 0.02; done\nprintf 'new successful result\\n'\nexit 0",
            gate.display(),
            dir.path().display()
        ),
    );
    let mut restarted = Application::start(&path);
    let restored = restarted.page("/status", "persisted problem");
    assert_eq!(
        saved, restored,
        "the full persisted result, including execution times, should survive"
    );
    fs::write(gate, "").unwrap();
    let updated = restarted.page("/status", "new successful result");
    assert!(updated.contains("class=\"OK\""));
    assert!(!updated.contains("persisted problem"));
}

#[test]
fn missing_nonexecutable_abnormal_and_invalid_exit_plugins_never_report_ok() {
    for failure in ["missing", "nonexecutable", "abnormal", "invalid exit"] {
        let dir = TempDir::new().unwrap();
        match failure {
            "missing" => {}
            "nonexecutable" => {
                plugin(&dir, "exit 0");
                fs::set_permissions(dir.path().join("check"), fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
            "abnormal" => plugin(&dir, "kill -TERM $$"),
            "invalid exit" => plugin(&dir, "printf 'plugin failed\\n'\nexit 42"),
            _ => unreachable!(),
        }
        let path = config(&dir, "");
        let mut app = Application::start(&path);
        let result = app.page("/status", "Runner failure:");
        assert!(result.contains("class=\"UNKNOWN\""), "{failure}: {result}");
        assert!(!result.contains("class=\"OK\""));
        if matches!(failure, "abnormal" | "invalid exit") {
            assert!(result.contains("unexpected exit"));
        }
    }
}

#[test]
fn hung_plugin_becomes_unknown_with_a_deadline_diagnostic() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        "printf 'waiting for remote endpoint\\n'\nexec sleep 20",
    );
    let config = config(&dir, "timeout_seconds: 1");
    let started = Instant::now();
    let mut app = Application::start(&config);
    app.page("/", "Not yet checked");
    let result = app.page("/status", "execution timed out after 1 seconds");
    assert!(result.contains("UNKNOWN"));
    assert!(result.contains("waiting for remote endpoint"));
    assert!(!result.contains("class=\"OK\""));
    assert!(started.elapsed() < Duration::from_secs(4));
}

fn update_config(path: &Path, update: impl FnOnce(&mut serde_yaml::Value)) {
    let mut yaml = serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    update(&mut yaml);
    fs::write(path, serde_yaml::to_string(&yaml).unwrap()).unwrap();
}

#[test]
fn reload_adds_changes_and_removes_host_and_service_identities_without_erasing_history() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "echo \"$1\" >> \"$2\"; echo \"$1\"; exit 2");
    let path = config(&dir, "confirmation_attempts: 1");
    let old_log = dir.path().join("old-executions");
    let added_log = dir.path().join("added-executions");
    update_config(&path, |yaml| {
        yaml["hosts"][0]["services"][0]["args"] =
            serde_yaml::to_value(["original", old_log.to_str().unwrap()]).unwrap();
    });
    let mut app = Application::start(&path);
    app.page("/status", "original\n</pre>");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
    let before = runtime
        .block_on(store.transitions("local".into(), "application".into()))
        .unwrap();
    assert_eq!(before.len(), 1);

    update_config(&path, |yaml| {
        let service = &mut yaml["hosts"][0]["services"][0];
        service["args"][0] = "changed".into();
        let mut added_service = service.clone();
        added_service["id"] = "added-service".into();
        added_service["name"] = "Added service".into();
        added_service["interval_seconds"] = 1.into();
        added_service["args"] =
            serde_yaml::to_value(["added", added_log.to_str().unwrap()]).unwrap();
        yaml["hosts"][0]["services"]
            .as_sequence_mut()
            .unwrap()
            .push(added_service.clone());
        let mut host = yaml["hosts"][0].clone();
        host["id"] = "added-host".into();
        host["name"] = "Added host".into();
        host["services"] = vec![added_service].into();
        yaml["hosts"].as_sequence_mut().unwrap().push(host);
    });
    app.reload("Configuration reloaded");
    let added = app.page_where("/status", "all added checks", |page| {
        page.contains("changed\n</pre>") && page.matches("added\n</pre>").count() == 2
    });
    assert!(added.contains("Added host"));
    assert_eq!(added.matches("Confirmed problem: CRITICAL").count(), 3);

    update_config(&path, |yaml| {
        yaml["hosts"].as_sequence_mut().unwrap().truncate(1);
        yaml["hosts"][0]["services"]
            .as_sequence_mut()
            .unwrap()
            .truncate(1);
    });
    app.reload("Configuration reloaded");
    let removed = app.page("/status", "changed\n</pre>");
    assert!(!removed.contains("added-host"));
    assert!(!removed.contains("added-service"));
    let executions = fs::read_to_string(&added_log).unwrap();
    thread::sleep(Duration::from_millis(1300));
    assert_eq!(
        fs::read_to_string(&added_log).unwrap(),
        executions,
        "removed checks kept running"
    );
    let history = runtime
        .block_on(store.transitions("local".into(), "application".into()))
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, before[0].id);
    let retained = runtime.block_on(store.latest()).unwrap();
    assert_eq!(retained.len(), 3, "removal must retain stored state");
    assert_eq!(
        retained[&("added-host".into(), "added-service".into())]
            .result
            .output,
        "added\n"
    );
    assert_eq!(
        runtime
            .block_on(store.transitions("added-host".into(), "added-service".into()))
            .unwrap()
            .len(),
        1
    );
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "plugin did not create {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn reload_during_executions_discards_obsolete_results_without_overlapping_replacements() {
    for readd in [false, true] {
        let dir = TempDir::new().unwrap();
        let gate = dir.path().join("release");
        plugin(
            &dir,
            &format!(
                r#"
case "$1" in
  blocked)
    touch '{dir}/'"$2"'-started'
    while [ ! -f '{gate}' ]; do [ -d '{dir}' ] || exit 3; sleep 0.02; done
    echo obsolete-result; exit 1;;
  fresh)
    [ -f '{gate}' ] || touch '{dir}/overlap'
    echo fresh-"$2"; exit 0;;
  *) echo seed-"$2"; exit 2;;
esac
"#,
                dir = dir.path().display(),
                gate = gate.display()
            ),
        );
        let path = config(&dir, "confirmation_attempts: 1\nmax_concurrent_checks: 2");
        update_config(&path, |yaml| {
            let service = &mut yaml["hosts"][0]["services"][0];
            service["args"] = serde_yaml::to_value(["seed", "application"]).unwrap();
            let mut removed = service.clone();
            removed["id"] = "removed".into();
            removed["name"] = "Removed service".into();
            removed["args"][1] = "removed".into();
            yaml["hosts"][0]["services"]
                .as_sequence_mut()
                .unwrap()
                .push(removed);
        });
        let mut app = Application::start(&path);
        app.page_where("/status", "both seeded results", |page| {
            page.contains("seed-application") && page.contains("seed-removed")
        });
        update_config(&path, |yaml| {
            for service in yaml["hosts"][0]["services"].as_sequence_mut().unwrap() {
                service["args"][0] = "blocked".into();
            }
        });
        app.reload("Configuration reloaded");
        wait_for_file(&dir.path().join("application-started"));
        wait_for_file(&dir.path().join("removed-started"));
        let mut removed = None;
        update_config(&path, |yaml| {
            let services = yaml["hosts"][0]["services"].as_sequence_mut().unwrap();
            removed = services.pop();
            services[0]["args"][0] = "fresh".into();
            // Lowering the global limit must not start replacements until the
            // old executions drain below the new limit.
            yaml["max_concurrent_checks"] = 1.into();
        });
        app.reload("Configuration reloaded");
        let during = app.page("/status", "seed-application");
        assert!(!during.contains("Removed service"));
        assert!(!during.contains("fresh-application"));
        if readd {
            update_config(&path, |yaml| {
                let mut service = removed.take().unwrap();
                service["args"][0] = "fresh".into();
                yaml["hosts"][0]["services"]
                    .as_sequence_mut()
                    .unwrap()
                    .push(service);
            });
            app.reload("Configuration reloaded");
        }
        fs::write(&gate, "").unwrap();
        let after = app.page_where("/status", "replacement results", |page| {
            page.contains("fresh-application") && (!readd || page.contains("fresh-removed"))
        });
        assert!(!after.contains("obsolete-result"));
        assert!(!dir.path().join("overlap").exists());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
        let saved = runtime.block_on(store.latest()).unwrap();
        assert_eq!(
            saved[&("local".into(), "removed".into())].result.output,
            if readd {
                "fresh-removed\n"
            } else {
                "seed-removed\n"
            }
        );
        for id in ["application", "removed"] {
            let history = runtime
                .block_on(store.transitions("local".into(), id.into()))
                .unwrap();
            let expected = if id == "removed" && !readd { 1 } else { 2 };
            assert_eq!(
                history.len(),
                expected,
                "obsolete execution changed problem history"
            );
            assert!(
                history
                    .iter()
                    .all(|transition| transition.result_status != ragios::runner::Status::Warning)
            );
        }
    }
}

#[test]
fn checks_keep_running_while_a_slow_reload_candidate_is_being_validated() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        &format!(
            "n=0; [ ! -f '{0}/count' ] || n=$(cat '{0}/count'); n=$((n+1)); echo $n > '{0}/count'; echo iteration-$n",
            dir.path().display()
        ),
    );
    let path = config(&dir, "interval_seconds: 1");
    let mut app = Application::start(&path);
    app.page("/status", "iteration-1");
    let (release, writer) = block_reload(&dir, &path, &mut app);
    // The reader is now blocked inside validation. Both execution and result
    // publication must continue before the invalid candidate can be rejected.
    app.page("/status", "iteration-2");
    release.send(()).unwrap();
    writer.join().unwrap();
    app.reload_outcome("Configuration reload rejected");
    app.page("/status", "Local host");
}

#[test]
fn ctrl_c_exits_even_while_a_reload_is_blocked_reading_configuration() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "echo ready");
    let path = config(&dir, "");
    let mut app = Application::start(&path);
    app.page("/status", "ready");
    let (release, writer) = block_reload(&dir, &path, &mut app);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(app.child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = app.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Ctrl-C waited for the blocked reload reader"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    release.send(()).unwrap();
    writer.join().unwrap();
}

fn block_reload(
    dir: &TempDir,
    path: &Path,
    app: &mut Application,
) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let fifo = dir.path().join("slow.yaml");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    update_config(path, |yaml| {
        yaml["includes"] = serde_yaml::to_value(["slow.yaml"]).unwrap()
    });
    let (opened, reader_ready) = mpsc::channel();
    let (release, writer_gate) = mpsc::channel();
    let writer = thread::spawn(move || {
        let mut file = fs::OpenOptions::new().write(true).open(fifo).unwrap();
        opened.send(()).unwrap();
        let _ = writer_gate.recv();
        // A shutdown test closes the reader before releasing this writer.
        let _ = file.write_all(b"hosts: [");
    });
    app.request_reload();
    reader_ready
        .recv_timeout(Duration::from_secs(5))
        .expect("reload never opened include");
    (release, writer)
}

#[test]
fn invalid_multifile_reload_leaves_active_definitions_and_checks_untouched() {
    let dir = TempDir::new().unwrap();
    let executions = dir.path().join("executions");
    plugin(
        &dir,
        &format!("echo \"$1\" >> '{}'; echo \"$1\"", executions.display()),
    );
    let path = config(
        &dir,
        "interval_seconds: 1\nincludes: [commands.yaml, hosts.yaml]",
    );
    let commands = dir.path().join("commands.yaml");
    let hosts = dir.path().join("hosts.yaml");
    fs::write(
        &commands,
        "commands: [{id: probe, executable: check, args: [old-output]}]",
    )
    .unwrap();
    fs::write(&hosts, "hosts: []").unwrap();
    update_config(&path, |yaml| {
        let service = yaml["hosts"][0]["services"][0].as_mapping_mut().unwrap();
        service.remove(serde_yaml::Value::from("executable"));
        service.remove(serde_yaml::Value::from("args"));
        service.insert("command".into(), "probe".into());
    });
    let mut app = Application::start(&path);
    let before = app.page("/status", "old-output");
    fs::write(
        &commands,
        "commands: [{id: probe, executable: check, args: [new-output]}]",
    )
    .unwrap();
    fs::write(&hosts, "hosts: [{id: added, name: Added host, services: [{id: new, name: New service, command: missing}]}]").unwrap();
    let error = app.reload("Configuration reload rejected");
    assert!(
        error.contains("hosts.yaml")
            && error.contains("unknown command")
            && error.contains("missing"),
        "{error}"
    );
    let unchanged = app.page_where("/status", "old checks continue after rejection", |page| {
        page.contains("old-output") && page != before
    });
    assert!(!unchanged.contains("Added host"));
    assert!(!unchanged.contains("new-output"));
    let log = fs::read_to_string(&executions).unwrap();
    assert!(log.lines().count() >= 2, "old checks stopped: {log}");
    assert!(
        log.lines().all(|line| line == "old-output"),
        "partial reload: {log}"
    );

    fs::write(&hosts, "hosts: [{id: added, name: Added host, services: [{id: new, name: New service, command: probe}]}]").unwrap();
    app.reload("Configuration reloaded");
    app.page_where("/status", "both new command results", |page| {
        page.contains("Added host") && page.matches("new-output").count() == 2
    });
}

#[test]
fn fleet_reload_applies_commands_bundles_groups_and_intervals_to_subsequent_work() {
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        &format!(
            r#"
n=0
[ ! -f '{dir}/'"$1" ] || n=$(cat '{dir}/'"$1")
n=$((n+1))
echo "$n" > '{dir}/'"$1"
echo "$1 iteration $n"
"#,
            dir = dir.path().display()
        ),
    );
    let path = dir.path().join("ragios.yaml");
    let fragment = dir.path().join("fleet.yaml");
    fs::write(&path, "bind: 127.0.0.1:0\nplugin_dir: .\ndatabase: state.sqlite3\nincludes: [fleet.yaml]\ninterval_seconds: 300\nrecipients: [old@example.invalid]\n").unwrap();
    let fleet = "commands: [{id: probe, executable: check, args: [first]}]\nbundles: [{id: basic, services: [{id: application, name: Original service, command: probe}]}]\ngroups: [{id: old-group, name: Old group}]\nhosts: [{id: local, name: Local host, group: old-group, bundles: [basic]}]\n";
    fs::write(&fragment, fleet).unwrap();
    let mut app = Application::start(&path);
    app.page("/status", "first iteration 1");
    fs::write(
        &fragment,
        fleet
            .replace("[first]", "[second]")
            .replace("Original service", "Updated bundled service")
            .replace("old-group", "new-group")
            .replace("Old group", "New group")
            .replace(
                "name: New group",
                "name: New group, recipients: [group@example.invalid]",
            ),
    )
    .unwrap();
    update_config(&path, |yaml| {
        yaml["interval_seconds"] = 1.into();
        yaml["recipients"] = serde_yaml::to_value(["new@example.invalid"]).unwrap();
    });
    app.reload("Configuration reloaded");
    let updated = app.page("/status", "second iteration 2");
    assert!(updated.contains("New group") && updated.contains("Updated bundled service"));
    assert!(!updated.contains("Old group") && !updated.contains("Original service"));
    assert!(!updated.contains("first iteration"));

    // Change only the interval. This must wake a sleeping check without
    // re-executing an otherwise unchanged command at reload time.
    update_config(&path, |yaml| yaml["interval_seconds"] = 300.into());
    app.reload("Configuration reloaded");
    let paused = app.page("/status", "second iteration");
    thread::sleep(Duration::from_millis(1300));
    assert_eq!(app.page("/status", "second iteration"), paused);
    update_config(&path, |yaml| yaml["interval_seconds"] = 1.into());
    app.reload("Configuration reloaded");
    app.page_where("/status", "interval-only update resumes checking", |page| {
        page != paused
    });
}

#[test]
fn reload_rejects_bind_and_database_changes_without_partial_application() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "echo original");
    let path = config(&dir, "");
    let original = fs::read_to_string(&path).unwrap();
    let mut app = Application::start(&path);
    let before = app.page("/status", "original");
    for (field, value) in [("bind", "127.0.0.1:1"), ("database", "other.sqlite3")] {
        fs::write(&path, &original).unwrap();
        update_config(&path, |yaml| {
            yaml[field] = value.into();
            yaml["hosts"][0]["name"] = "Must not appear".into();
        });
        let error = app.reload("Configuration reload rejected");
        assert!(
            error.contains(field) && error.contains("restart required"),
            "{error}"
        );
        assert_eq!(app.page("/status", "original"), before);
    }
    assert!(!dir.path().join("other.sqlite3").exists());
}

#[test]
fn reload_preserves_results_and_confirmed_problems_when_display_names_change() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "echo retained-problem; exit 2");
    let path = config(&dir, "confirmation_attempts: 1");
    let mut app = Application::start(&path);
    let before = app.page("/status", "Confirmed problem: CRITICAL");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
    let history = || {
        runtime
            .block_on(store.transitions("local".into(), "application".into()))
            .unwrap()
            .into_iter()
            .map(|transition| {
                (
                    transition.id,
                    transition.occurred_at,
                    transition.result_status,
                    transition.kind,
                    transition.state,
                )
            })
            .collect::<Vec<_>>()
    };
    let original_history = history();
    assert_eq!(original_history.len(), 1);
    let renamed = fs::read_to_string(&path)
        .unwrap()
        .replace("Local host", "Renamed host")
        .replace("Application health", "Renamed service");
    fs::write(&path, renamed).unwrap();
    app.reload("Configuration reloaded");
    let after = app.page("/status", "Renamed service");
    assert_eq!(
        after,
        before
            .replace("Local host", "Renamed host")
            .replace("Application health", "Renamed service")
    );
    assert_eq!(history(), original_history);
}

#[test]
fn validation_checks_all_files_without_starting_monitoring_or_opening_sqlite() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "touch started");
    let path = config(&dir, "includes: [commands.yaml]");
    let valid = fs::read_to_string(&path)
        .unwrap()
        .replace("executable: check", "command: probe");
    fs::write(&path, valid).unwrap();
    let fragment = dir.path().join("commands.yaml");
    for (commands, expected) in [
        ("commands: [{id: probe, executable: check}]", None),
        (
            "commands: [{id: other, executable: check}]",
            Some("unknown command"),
        ),
        ("commands: [", Some("parse included configuration")),
    ] {
        fs::write(&fragment, commands).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ragios"))
            .args(["--validate", "--config"])
            .arg(&path)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(output.status.success(), expected.is_none(), "{output:?}");
        if let Some(expected) = expected {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(expected),
                "{output:?}"
            );
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("Configuration valid"));
        }
        assert!(!dir.path().join("started").exists());
        assert!(!dir.path().join("state.sqlite3").exists());
    }
}

#[test]
fn invalid_configuration_fails_before_any_plugin_starts() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("started");
    plugin(&dir, &format!("touch '{}'", marker.display()));
    let config = config(&dir, "");
    let valid = fs::read_to_string(&config).unwrap();
    let duplicate = "      - id: application\n        name: Duplicate\n        executable: check\n";
    let cases = [
        (format!("{valid}{duplicate}"), "duplicate service ID"),
        ("hosts: [".into(), "parse configuration"),
        (format!("{valid}\nemail_enabled: true"), "unknown field"),
        (format!("{valid}\ninterval_seconds: 0"), "interval_seconds"),
        (format!("{valid}\ntimeout_seconds: 0"), "timeout_seconds"),
        (
            format!("{valid}\nmax_concurrent_checks: 0"),
            "max_concurrent_checks must be",
        ),
        (
            format!("{valid}\nmax_output_bytes: 0"),
            "max_output_bytes must be",
        ),
        (
            format!("{valid}\nmax_concurrent_checks: {}", usize::MAX),
            "max_concurrent_checks must be",
        ),
        (
            format!("{valid}\nmax_output_bytes: {}", usize::MAX),
            "max_output_bytes must be",
        ),
        (
            format!("{valid}\nmax_concurrent_checks: -1"),
            "parse configuration",
        ),
        (
            format!("{valid}\nmax_output_bytes: 1.5"),
            "parse configuration",
        ),
        (
            format!("{valid}\nretry_interval_seconds: 0"),
            "retry_interval_seconds",
        ),
        (
            format!("{valid}\nretry_interval_seconds: 4294967296"),
            "retry_interval_seconds",
        ),
        (
            format!("{valid}\nconfirmation_attempts: 0"),
            "confirmation_attempts",
        ),
        (
            format!("{valid}\nconfirmation_attempts: 4294967296"),
            "confirmation_attempts",
        ),
        (
            valid.replace(
                "    name: Local host",
                "    name: Local host\n    group: absent",
            ),
            "unknown group",
        ),
        (
            valid.replace(
                "    name: Local host",
                "    name: Local host\n    overrides: {absent: {args: []}}",
            ),
            "override refers to unknown service",
        ),
        (
            valid.replace("        executable: check", "        command: absent"),
            "unknown command",
        ),
        (valid.replace("id: local", "id: ''"), "host ID"),
        (
            valid.replace("executable: check", "executable: ''"),
            "executable must not be empty",
        ),
        (
            format!(
                "{valid}  - id: local\n    name: Duplicate host\n    services:\n      - id: other\n        name: Other\n        executable: check\n"
            ),
            "duplicate host ID",
        ),
    ];
    for (yaml, expected) in cases {
        fs::write(&config, yaml).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ragios"))
            .arg("--config")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("invalid configuration was accepted: {expected}");
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "expected {expected}: {error}");
        assert!(
            !marker.exists(),
            "validation must finish before checks begin"
        );
        assert!(!dir.path().join("state.sqlite3").exists());
    }
}

#[test]
fn service_result_is_visible_in_the_page_and_htmx_fragment() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("release");
    plugin(
        &dir,
        &format!(
            "while [ ! -f '{}' ]; do [ -d '{}' ] || exit 3; sleep 0.02; done\nprintf '%s\\n' '<script>alert(1)</script> database unavailable'\nexit 2",
            gate.display(),
            dir.path().display()
        ),
    );
    let config = config(&dir, "");
    let mut app = Application::start(&config);
    let initial = app.page("/", "Not yet checked");
    assert!(initial.contains("Local host"));
    assert!(initial.contains("Application health"));
    assert!(initial.contains("hx-get=\"/status\""));
    assert!(initial.contains("/assets/htmx.min.js"));
    let asset = app
        .client
        .get(format!("{}/assets/htmx.min.js", app.url))
        .send()
        .unwrap();
    assert!(asset.status().is_success());
    assert_eq!(
        asset.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert!(asset.text().unwrap().contains("2.0.8"));
    fs::write(gate, "").unwrap();
    let fragment = app.page("/status", "CRITICAL");
    assert!(fragment.contains("&lt;script&gt;alert(1)&lt;/script&gt; database unavailable"));
    assert!(!fragment.contains("<script>alert(1)</script>"));
    assert!(fragment.contains("Finished (UTC)"));
    assert!(fragment.contains("Duration"));
    assert!(!fragment.contains("<!doctype html>"));
    app.page("/", "CRITICAL");
}

fn seed(dir: &TempDir, host: &str, service: &str, status: u8, finished_at: &str, attempts: u32) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
    runtime
        .block_on(store.save(
            host.into(),
            service.into(),
            ragios::runner::CheckResult {
                status: ragios::runner::Status::from_code(Some(status.into())),
                output: format!("seeded {service} at {finished_at}"),
                started_at: finished_at.into(),
                finished_at: finished_at.into(),
                duration_ms: 5,
            },
            attempts,
        ))
        .unwrap();
}

/// Plugins behave by their first argument; `hold` never finishes, so seeded
/// results stay current for the duration of a test.
fn browsing_fleet(dir: &TempDir, hosts: &str) -> std::path::PathBuf {
    plugin(
        dir,
        &format!(
            r#"case "$1" in
  ok) printf 'all fine\n'; exit 0;;
  broken) printf 'broken service\n'; exit 2;;
  *) while [ -d '{}' ]; do sleep 0.05; done; exit 3;;
esac"#,
            dir.path().display()
        ),
    );
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        format!(
            r#"bind: 127.0.0.1:0
plugin_dir: .
database: state.sqlite3
confirmation_attempts: 1
commands:
  - {{id: ok, executable: check, args: [ok]}}
  - {{id: broken, executable: check, args: [broken]}}
  - {{id: hold, executable: check, args: [hold]}}
groups:
  - {{id: prod, name: Production}}
  - {{id: lab, name: "Lab <bench>"}}
hosts:
{hosts}"#
        ),
    )
    .unwrap();
    path
}

#[test]
fn overview_filters_checks_by_host_group_and_problem_status() {
    let dir = TempDir::new().unwrap();
    let path = browsing_fleet(
        &dir,
        r#"  - id: alpha
    name: Alpha
    group: prod
    services:
      - {id: web-ok, name: Web front, command: ok}
      - {id: web-broken, name: Web backend, command: broken}
  - id: beta
    name: Beta
    group: lab
    services:
      - {id: lab-held, name: Lab queue, command: hold}
      - {id: lab-ok, name: Lab cache, command: ok}
  - id: gamma
    name: Gamma
    group: lab
    services:
      - {id: gamma-ok, name: Gamma disk, command: ok}
"#,
    );
    // A pending problem from an earlier run, under a stricter threshold.
    seed(&dir, "beta", "lab-held", 1, "2026-11-20T12:00:00.000Z", 3);
    let mut app = Application::start(&path);
    let all = app.page_where("/", "every result", |page| {
        page.matches("all fine").count() == 3 && page.contains("broken service")
    });
    for name in [
        "Web front",
        "Web backend",
        "Lab queue",
        "Lab cache",
        "Gamma disk",
    ] {
        assert!(all.contains(name), "missing {name}: {all}");
    }
    assert!(all.contains("Lab &lt;bench&gt;"));

    for (query, shown, hidden) in [
        (
            "?group=lab",
            &["Lab queue", "Lab cache", "Gamma disk"][..],
            &["Web front", "Web backend"][..],
        ),
        (
            "?problem=any",
            &["Web backend", "Lab queue"],
            &["Web front", "Lab cache", "Gamma disk", "Gamma"],
        ),
        (
            "?problem=confirmed",
            &["Web backend"],
            &["Web front", "Lab queue", "Lab cache", "Gamma disk"],
        ),
        (
            "?problem=pending",
            &["Lab queue"],
            &["Web front", "Web backend", "Lab cache", "Gamma disk"],
        ),
        (
            "?group=lab&problem=any",
            &["Lab queue"],
            &["Web front", "Web backend", "Lab cache", "Gamma disk"],
        ),
    ] {
        for path in ["/", "/status"] {
            let page = app.page(&format!("{path}{query}"), shown[0]);
            for name in shown {
                assert!(page.contains(name), "{query}: missing {name}: {page}");
            }
            for name in hidden {
                assert!(
                    !page.contains(&format!(">{name}<")),
                    "{query}: unexpected {name}: {page}"
                );
            }
            assert!(!page.contains("No checks match"), "{page}");
        }
    }
    let replaced = app
        .client
        .get(format!("{}/status?group=lab&problem=pending", app.url))
        .header("HX-Trigger", "filters")
        .send()
        .unwrap();
    assert_eq!(
        replaced.headers()["hx-replace-url"],
        "/?group=lab&problem=pending",
        "a filter change keeps the address shareable"
    );
    let polled = app
        .client
        .get(format!("{}/status?group=lab", app.url))
        .header("HX-Trigger", "status")
        .send()
        .unwrap();
    assert!(!polled.headers().contains_key("hx-replace-url"));
    let filtered = app.page("/?group=lab&problem=pending", "Lab queue");
    assert!(
        filtered.contains(r#"<option value="lab" selected>"#),
        "{filtered}"
    );
    assert!(
        filtered.contains(r#"<option value="pending" selected>"#),
        "{filtered}"
    );

    let none = app.page("/?group=prod&problem=pending", "No checks match");
    assert!(!none.contains("Web backend"));
    assert!(none.contains(r#"href="/""#), "offers a way back: {none}");
}

fn link_to<'a>(html: &'a str, prefix: &str) -> &'a str {
    let start = html
        .find(&format!("href=\"{prefix}"))
        .unwrap_or_else(|| panic!("no link to {prefix}: {html}"))
        + "href=\"".len();
    let end = start + html[start..].find('"').unwrap();
    &html[start..end]
}

#[test]
fn details_show_escaped_diagnostics_and_chronological_state_changes() {
    let dir = TempDir::new().unwrap();
    let counter = dir.path().join("counter");
    plugin(
        &dir,
        &format!(
            r#"if [ "$1" = host ]; then printf 'pong\n'; exit 0; fi
n=0
if [ -f '{counter}' ]; then n=$(cat '{counter}'); fi
printf '%s' "$((n+1))" > '{counter}'
printf '<img src=x onerror="alert(%s)"> & observation %s\n' "'x'" "$n"
case "$n" in
  0) exit 1;;
  1|2) exit 2;;
  *) exit 0;;
esac"#,
            counter = counter.display()
        ),
    );
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        r#"bind: 127.0.0.1:0
plugin_dir: .
database: state.sqlite3
interval_seconds: 1
retry_interval_seconds: 1
hosts:
  - id: "edge/01 #1"
    name: Edge router
    reachability: {probe: ping, executable: check, args: [host]}
    services:
      - {id: "api?v=1", name: "API <v1>", executable: check, args: [service]}
"#,
    )
    .unwrap();
    let mut app = Application::start(&path);
    let overview = app.page("/", "Edge router");
    let host_url = link_to(row_for(&overview, "API &lt;v1&gt;"), "/hosts/").to_owned();
    assert_eq!(host_url, "/hosts/edge%2F01%20%231");

    let host = app.page(&host_url, "API &lt;v1&gt;");
    assert!(host.contains("Edge router"));
    assert!(host.contains(r#"href="/""#), "links back to the overview");
    let service_url = link_to(&host, "/hosts/edge%2F01%20%231/services/").to_owned();
    assert_eq!(service_url, "/hosts/edge%2F01%20%231/services/api%3Fv%3D1");
    let reachability_url = link_to(&host, "/hosts/edge%2F01%20%231/reachability").to_owned();

    let detail = app.page(&service_url, "Recovered");
    assert!(detail.contains("API &lt;v1&gt;"));
    assert!(
        detail.contains(&format!(r#"href="{host_url}""#)),
        "links back to the host"
    );
    assert!(
        detail
            .contains("&lt;img src=x onerror=&quot;alert(&#39;x&#39;)&quot;&gt; &amp; observation")
    );
    assert!(!detail.contains("<img"));
    for label in [
        "Started (UTC)",
        "Finished (UTC)",
        "Duration",
        "Result age",
        " ago",
    ] {
        assert!(detail.contains(label), "missing {label}: {detail}");
    }
    let history = &detail[detail.find("State changes").expect("history section")..];
    let positions: Vec<_> = [
        "Pending problem: WARNING (attempt 1)",
        "Pending problem: CRITICAL (attempt 2)",
        "Problem confirmed: CRITICAL",
        "Recovered",
    ]
    .iter()
    .map(|label| {
        history
            .find(label)
            .unwrap_or_else(|| panic!("missing {label}: {history}"))
    })
    .collect();
    assert!(positions.is_sorted(), "not chronological: {history}");

    let reachability = app.page(&reachability_url, "pong");
    assert!(reachability.contains("Reachable (OK)"), "{reachability}");
    assert!(reachability.contains("No state changes in the last 30 days"));

    for missing in [
        "/hosts/nowhere",
        "/hosts/edge%2F01%20%231/services/nothing",
        "/hosts/nowhere/reachability",
    ] {
        let response = app
            .client
            .get(format!("{}{missing}", app.url))
            .send()
            .unwrap();
        assert_eq!(response.status(), 404, "{missing}");
        assert!(response.text().unwrap().contains("Not configured"));
    }
}

#[test]
fn running_server_prunes_old_state_changes_but_keeps_current_problems_visible() {
    let stamp = |days: i64| {
        (chrono::Utc::now() - chrono::TimeDelta::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let dir = TempDir::new().unwrap();
    let path = browsing_fleet(
        &dir,
        r#"  - id: alpha
    name: Alpha
    group: prod
    services:
      - {id: queue, name: Message queue, command: hold}
"#,
    );
    let old = stamp(31);
    for _ in 0..3 {
        seed(&dir, "alpha", "queue", 1, &old, 3);
    }
    let recent = stamp(1);
    seed(&dir, "alpha", "queue", 2, &recent, 3);
    let mut app = Application::start(&path);

    let detail = app.page_where("/hosts/alpha/services/queue", "pruned history", |page| {
        !page.contains(&old)
    });
    assert!(
        detail.contains(r#"<td class="stale">CRITICAL (stale)</td>"#),
        "a day-old result is stale: {detail}"
    );
    assert!(detail.contains("Confirmed problem: WARNING"), "{detail}");
    assert!(
        detail.contains("Pending severity change: CRITICAL (1/"),
        "{detail}"
    );
    assert!(detail.contains("1 d 0 h ago"), "{detail}");
    let history = &detail[detail.find("State changes").unwrap()..];
    assert!(history.contains(&recent));
    assert!(history.contains("Pending severity change: CRITICAL (attempt 1)"));
    assert!(!history.contains("Problem confirmed"), "{history}");

    let overview = app.page("/?problem=confirmed", "Message queue");
    let row = row_for(&overview, "Message queue");
    assert!(
        row.contains(r#"<td class="stale">CRITICAL (stale)</td>"#),
        "{row}"
    );
    assert!(row.contains("Confirmed problem: WARNING"), "{row}");
}

fn runs(log: &Path) -> usize {
    fs::read_to_string(log).map_or(0, |text| text.lines().count())
}

#[test]
fn restart_restores_confirmed_problems_without_rerunning_checks_that_are_not_due() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("runs.log");
    plugin(
        &dir,
        &format!(
            "echo run >> '{}'\nprintf 'service down\\n'\nexit 2",
            log.display()
        ),
    );
    let path = config(&dir, "confirmation_attempts: 1");
    let mut first = Application::start(&path);
    let saved = first.page("/status", "Confirmed problem: CRITICAL");
    drop(first);
    assert_eq!(runs(&log), 1);

    let mut restarted = Application::start(&path);
    let restored = restarted.page("/status", "Confirmed problem: CRITICAL");
    assert_eq!(
        saved, restored,
        "restart should restore the dashboard as it was"
    );
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        runs(&log),
        1,
        "a check that is not yet due must wait for its interval"
    );
    let history = restarted.page("/hosts/local/services/application", "Problem confirmed");
    assert_eq!(
        history.matches("Problem confirmed").count(),
        1,
        "a restored problem is not confirmed again: {history}"
    );
}

#[test]
fn overdue_checks_are_spread_across_the_startup_period_after_restart() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("starts.log");
    let first_round = dir.path().join("first-round");
    fs::write(&first_round, "").unwrap();
    // While `first-round` exists, checks run normally; afterwards each logs its
    // start time once, then holds its execution slot until the test ends.
    plugin(
        &dir,
        &format!(
            r#"if [ -f '{first}' ]; then printf 'first round\n'; exit 0; fi
printf '%s %s\n' "$1" "$(python3 -c 'import time; print(time.time())')" >> '{log}'
while [ -d '{dir}' ]; do sleep 0.05; done
exit 3"#,
            first = first_round.display(),
            log = log.display(),
            dir = dir.path().display()
        ),
    );
    let path = config(&dir, "interval_seconds: 1\nstartup_spread_seconds: 2");
    let mut yaml: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    yaml["hosts"][0]["services"] = serde_yaml::from_str(
        "[{id: a, name: A, executable: check, args: [a]}, {id: b, name: B, executable: check, args: [b]},
          {id: c, name: C, executable: check, args: [c]}, {id: d, name: D, executable: check, args: [d]}]",
    )
    .unwrap();
    fs::write(&path, serde_yaml::to_string(&yaml).unwrap()).unwrap();

    let mut first = Application::start(&path);
    first.page_where("/status", "four first-round results", |page| {
        page.matches("first round").count() == 4
    });
    drop(first);
    fs::remove_file(&first_round).unwrap();
    // Every saved result is now more than one interval old.
    thread::sleep(Duration::from_millis(1200));

    let mut restarted = Application::start(&path);
    restarted.page("/status", "first round");
    let deadline = Instant::now() + Duration::from_secs(5);
    while runs(&log) < 4 {
        assert!(
            Instant::now() < deadline,
            "overdue checks did not all start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(runs(&log), 4, "each overdue check starts exactly once");
    let mut starts: Vec<f64> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| line.split_once(' ').unwrap().1.parse().unwrap())
        .collect();
    starts.sort_by(f64::total_cmp);
    // Four overdue checks over two seconds are half a second apart.
    for gap in starts.windows(2).map(|pair| pair[1] - pair[0]) {
        assert!(
            (0.3..0.8).contains(&gap),
            "uneven startup spacing: {starts:?}"
        );
    }
}

#[test]
fn overdue_checks_are_marked_stale_instead_of_presenting_an_old_ok_as_current() {
    let dir = TempDir::new().unwrap();
    let gate = dir.path().join("release");
    plugin(
        &dir,
        &format!(
            "while [ ! -f '{}' ]; do [ -d '{}' ] || exit 3; sleep 0.02; done\nprintf 'fresh result\\n'\nexit 0",
            gate.display(),
            dir.path().display()
        ),
    );
    let path = config(&dir, "");
    let finished = chrono::Utc::now() - chrono::TimeDelta::hours(2);
    let timestamp = |time: chrono::DateTime<chrono::Utc>| {
        time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    seed(&dir, "local", "application", 0, &timestamp(finished), 3);
    let due = timestamp(finished + chrono::TimeDelta::minutes(5));

    let mut app = Application::start(&path);
    let page = app.page("/status", "seeded application");
    let row = row_for(&page, "Application health");
    assert!(
        !row.contains("class=\"OK\""),
        "an old OK must not look current: {row}"
    );
    assert!(
        row.contains("OK (stale)"),
        "the plugin status stays visible: {row}"
    );
    assert!(
        !row.contains("UNKNOWN"),
        "staleness is not a plugin UNKNOWN: {row}"
    );
    assert!(row.contains(&format!("Overdue since {due}")), "{row}");
    assert!(page.contains("1 check is overdue"), "{page}");
    let detail = app.page("/hosts/local/services/application", "seeded application");
    assert!(detail.contains("Overdue since"), "{detail}");
    assert!(
        detail.contains("2 h 0 min ago"),
        "result age stays separately visible: {detail}"
    );

    fs::write(&gate, "").unwrap();
    let page = app.page("/status", "fresh result");
    let row = row_for(&page, "Application health");
    assert!(row.contains("<td class=\"OK\">OK</td>"), "{row}");
    assert!(!page.contains("overdue"), "{page}");
}

#[test]
fn failures_to_record_results_are_shown_prominently_and_clear_once_monitoring_progresses() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "printf 'healthy\\n'\nexit 0");
    let path = config(&dir, "interval_seconds: 1\nretry_interval_seconds: 1");
    let mut app = Application::start(&path);
    app.page("/status", "healthy");

    let database = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER simulated_fault BEFORE INSERT ON check_results
             BEGIN SELECT RAISE(ABORT, 'simulated <storage> failure'); END;",
        )
        .unwrap();
    let page = app.page("/status", "Scheduler fault");
    assert!(
        page.contains("simulated &lt;storage&gt; failure"),
        "the fault explains itself, escaped as text: {page}"
    );
    let detail = app.page("/hosts/local/services/application", "Scheduler fault");
    assert!(
        detail.contains("simulated &lt;storage&gt; failure"),
        "{detail}"
    );

    database
        .execute_batch("DROP TRIGGER simulated_fault")
        .unwrap();
    app.page_where("/status", "fault cleared", |page| {
        !page.contains("Scheduler fault")
    });
}

#[test]
fn results_timestamped_in_the_future_are_stale_and_checked_again_at_startup() {
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("runs.log");
    plugin(
        &dir,
        &format!(
            "echo run >> '{}'\nwhile [ -d '{}' ]; do sleep 0.05; done\nexit 3",
            log.display(),
            dir.path().display()
        ),
    );
    let path = config(&dir, "");
    // As if the system clock moved back a day after this result was saved.
    let future = (chrono::Utc::now() + chrono::TimeDelta::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    seed(&dir, "local", "application", 0, &future, 3);

    let mut app = Application::start(&path);
    let page = app.page("/status", "seeded application");
    let row = row_for(&page, "Application health");
    assert!(
        row.contains("OK (stale)"),
        "a future result cannot be current: {row}"
    );
    assert!(page.contains("1 check is overdue"), "{page}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while runs(&log) == 0 {
        assert!(
            Instant::now() < deadline,
            "the check did not run at startup"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn checks_that_never_record_a_result_become_overdue_from_when_they_were_first_due() {
    let dir = TempDir::new().unwrap();
    plugin(&dir, "printf 'healthy\\n'\nexit 0");
    let path = config(&dir, "timeout_seconds: 1\nretry_interval_seconds: 1");
    drop(ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
    let database = rusqlite::Connection::open(dir.path().join("state.sqlite3")).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER simulated_fault BEFORE INSERT ON check_results
             BEGIN SELECT RAISE(ABORT, 'simulated storage failure'); END;",
        )
        .unwrap();
    let mut app = Application::start(&path);
    let page = app.page("/status", "Scheduler fault");
    assert!(page.contains("Not yet checked"), "{page}");
    assert!(
        !page.contains("overdue"),
        "not overdue within its deadline and grace: {page}"
    );

    // One-second deadline plus 30 seconds of scheduling grace.
    thread::sleep(Duration::from_secs(32));
    let page = app.page("/status", "1 check is overdue");
    let row = row_for(&page, "Application health");
    assert!(row.contains("Overdue since"), "{row}");
    assert!(row.contains("no result yet"), "{row}");

    database
        .execute_batch("DROP TRIGGER simulated_fault")
        .unwrap();
    let page = app.page("/status", "healthy");
    assert!(!page.contains("overdue"), "{page}");
}

#[test]
fn checks_wait_for_closed_windows_without_becoming_overdue_even_after_a_restart() {
    use chrono::{Datelike, Days, SecondsFormat, TimeDelta, Utc};
    // Keep tomorrow's window closed for the whole test.
    let midnight = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        + Days::new(1);
    if midnight - Utc::now() < TimeDelta::minutes(1) {
        thread::sleep(
            (midnight - Utc::now() + TimeDelta::seconds(1))
                .to_std()
                .unwrap(),
        );
    }
    let tomorrow = Utc::now().date_naive() + Days::new(1);
    let opens = tomorrow.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let day = tomorrow.weekday().to_string().to_lowercase();
    let dir = TempDir::new().unwrap();
    plugin(
        &dir,
        "touch \"$1\"; printf 'ran %s\\n' \"$(basename \"$1\")\"",
    );
    let marker = |name: &str| dir.path().join(name).to_str().unwrap().to_owned();
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        format!(
            r#"bind: 127.0.0.1:0
plugin_dir: .
database: state.sqlite3
check_window: tomorrow
check_windows:
  - id: tomorrow
    ranges: [{{days: [{day}], start: '00:00', end: '24:00'}}]
hosts:
  - id: local
    name: Local host
    services:
      - {{id: open, name: Open window, executable: check, args: ['{open}'], check_window: always}}
      - {{id: new, name: Never checked, executable: check, args: ['{new}']}}
      - {{id: saved, name: Saved earlier, executable: check, args: ['{saved}']}}
"#,
            open = marker("open"),
            new = marker("new"),
            saved = marker("saved"),
        ),
    )
    .unwrap();
    // Saved an hour ago and due five minutes later, while the window was closed.
    let earlier = (Utc::now() - TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    seed(&dir, "local", "saved", 0, &earlier, 3);
    let due = format!("Due {}", opens.to_rfc3339_opts(SecondsFormat::Millis, true));

    let mut app = Application::start(&path);
    let page = app.page("/", "ran open");
    assert!(row_for(&page, "Never checked").contains(&due), "{page}");
    let saved = row_for(&page, "Saved earlier");
    assert!(saved.contains(&due), "{saved}");
    assert!(
        saved.contains("class=\"OK\""),
        "a closed window keeps results current: {saved}"
    );
    assert!(!page.contains("overdue"), "{page}");
    let detail = app.page("/hosts/local/services/saved", "Result age");
    assert!(detail.contains(&earlier));
    drop(app);

    // A restart outside the window resumes without catching up.
    let mut restarted = Application::start(&path);
    let page = restarted.page("/", "Saved earlier");
    assert!(row_for(&page, "Saved earlier").contains(&due), "{page}");
    thread::sleep(Duration::from_secs(1));
    assert!(fs::exists(marker("open")).unwrap());
    for name in ["new", "saved"] {
        assert!(
            !fs::exists(marker(name)).unwrap(),
            "{name} ran in a closed window"
        );
    }

    // Removing the window applies the interval deadline restored at startup.
    update_config(&path, |yaml| yaml["check_window"] = "always".into());
    restarted.reload("Configuration reloaded");
    restarted.page_where("/", "both released checks", |page| {
        page.contains("ran saved") && page.contains("ran new")
    });
}

#[test]
fn checks_waiting_for_a_free_slot_start_oldest_deadline_first() {
    use chrono::{SecondsFormat, TimeDelta, Utc};
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("order.log");
    plugin(
        &dir,
        &format!(
            "printf '%s\\n' \"$1\" >> '{}'\n[ \"$1\" = blocker ] && sleep 2\nexit 0",
            log.display()
        ),
    );
    let path = dir.path().join("ragios.yaml");
    fs::write(
        &path,
        "bind: 127.0.0.1:0
plugin_dir: .
database: state.sqlite3
max_concurrent_checks: 1
startup_spread_seconds: 1
hosts:
  - id: local
    name: Local host
    services:
      - {id: blocker, name: Blocker, executable: check, args: [blocker]}
      - {id: z-older, name: Older, executable: check, args: [z-older]}
      - {id: a-newer, name: Newer, executable: check, args: [a-newer]}
",
    )
    .unwrap();
    let ago = |minutes| {
        (Utc::now() - TimeDelta::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Millis, true)
    };
    seed(&dir, "local", "z-older", 0, &ago(20), 3);
    seed(&dir, "local", "a-newer", 0, &ago(10), 3);
    // Both restored checks fall due while the never-checked blocker holds the
    // only slot, so they compete for it when it frees.
    let _app = Application::start(&path);
    let deadline = Instant::now() + Duration::from_secs(10);
    let order = loop {
        let order = fs::read_to_string(&log).unwrap_or_default();
        if order.lines().count() >= 3 || Instant::now() > deadline {
            break order;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        ["blocker", "z-older", "a-newer"]
    );
}
