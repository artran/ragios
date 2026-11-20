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
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(1))
            .no_proxy()
            .build()
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ragios"))
            .arg("--config")
            .arg(config)
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
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = String::new();
        loop {
            assert!(self.child.try_wait().unwrap().is_none(), "server exited");
            if let Ok(response) = self.client.get(format!("{}{path}", self.url)).send() {
                assert!(response.status().is_success());
                last = response.text().unwrap();
                if last.contains(expected) {
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
