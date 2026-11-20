#![cfg(unix)]

use std::{
    fs,
    io::{BufRead, BufReader},
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
        let mut app = Self {
            child,
            url: String::new(),
            client,
            logs: None,
        };
        let stderr = app.child.stderr.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        app.logs = Some(thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if let Some(address) = line.strip_prefix("Ragios listening on ") {
                    let _ = sender.send(address.split(';').next().unwrap().to_owned());
                } else {
                    eprintln!("{line}");
                }
            }
        }));
        app.url = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("server did not announce its bound listener");
        app
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
    assert_eq!(result.matches("class=\"UNKNOWN\"").count(), 2);
    assert_eq!(result.matches("class=\"OK\"").count(), 1);
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
    let path = config(&dir, "timeout_seconds: 1\nmax_concurrent_checks: 1");
    // The observer needs a longer deadline than the plugin under test. It polls
    // the marker after restart so both configurations retain the same limits.
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
    let config = config(&dir, "interval_seconds: 1");
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
    plugin(&dir, "printf 'persisted problem\\n'\nexit 1");
    let path = config(&dir, "");
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
    let path = config(&dir, "");
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
