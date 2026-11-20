use crate::config::Service;
use chrono::{SecondsFormat, Utc};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Clone, Copy)]
pub enum Status {
    Ok,
    Warning,
    Critical,
    Unknown,
}

impl Status {
    pub fn from_code(code: Option<i32>) -> Self {
        match code {
            Some(0) => Self::Ok,
            Some(1) => Self::Warning,
            Some(2) => Self::Critical,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
            Self::Unknown => "UNKNOWN",
        }
    }
}

pub struct CheckResult {
    pub status: Status,
    pub output: String,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
}

pub async fn execute(plugin_dir: &Path, service: &Service, timeout_seconds: u64) -> CheckResult {
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let clock = Instant::now();
    let (status, output) = run(plugin_dir, service, timeout_seconds).await;
    CheckResult {
        status,
        output,
        started_at,
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        duration_ms: clock.elapsed().as_millis() as u64,
    }
}

async fn run(plugin_dir: &Path, service: &Service, timeout_seconds: u64) -> (Status, String) {
    let mut command = Command::new(plugin_dir.join(&service.executable));
    command
        .args(&service.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (Status::Unknown, format!("Runner failure: {error}")),
    };
    #[cfg(unix)]
    let process_group = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_seconds), async {
        let (exit, stdout, stderr) = tokio::join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        stdout?;
        stderr?;
        exit
    })
    .await;
    let (status, diagnostic) = match outcome {
        Ok(Ok(exit)) if matches!(exit.code(), Some(0..=3)) => {
            (Status::from_code(exit.code()), None)
        }
        Ok(Ok(exit)) => (Status::Unknown, Some(format!("unexpected exit {exit}"))),
        other => {
            // Kill the execution's process group on Unix, then reap the direct child.
            #[cfg(unix)]
            if let Some(pid) = process_group {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill().await;
            let diagnostic = match other {
                Err(_) => format!("execution timed out after {timeout_seconds} seconds"),
                Ok(Err(error)) => format!("process I/O failed: {error}"),
                _ => unreachable!(),
            };
            (Status::Unknown, Some(diagnostic))
        }
    };
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&err));
    }
    if let Some(diagnostic) = diagnostic {
        text.push_str(&format!("\nRunner failure: {diagnostic}"));
    }
    (status, text)
}
