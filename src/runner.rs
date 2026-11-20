use crate::config::Service;
use chrono::{SecondsFormat, Utc};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    process::{ChildStderr, ChildStdout, Command},
};

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

pub async fn execute(
    plugin_dir: &Path,
    service: &Service,
    timeout_seconds: u64,
    max_output_bytes: usize,
) -> CheckResult {
    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let clock = Instant::now();
    let (status, output) = run(plugin_dir, service, timeout_seconds, max_output_bytes).await;
    CheckResult {
        status,
        output,
        started_at,
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        duration_ms: clock.elapsed().as_millis() as u64,
    }
}

async fn run(
    plugin_dir: &Path,
    service: &Service,
    timeout_seconds: u64,
    max_output_bytes: usize,
) -> (Status, String) {
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
    let mut output = CapturedOutput::default();
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_seconds), async {
        let (exit, ()) = tokio::try_join!(
            child.wait(),
            output.drain(&mut stdout, &mut stderr, max_output_bytes)
        )?;
        Ok::<_, std::io::Error>(exit)
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
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if output.truncated {
        text.push_str(&format!("\n[output truncated at {max_output_bytes} bytes]"));
    }
    if let Some(diagnostic) = diagnostic {
        text.push_str(&format!("\nRunner failure: {diagnostic}"));
    }
    (status, text)
}

#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl CapturedOutput {
    async fn drain(
        &mut self,
        stdout: &mut ChildStdout,
        stderr: &mut ChildStderr,
        limit: usize,
    ) -> std::io::Result<()> {
        let mut out_chunk = [0; 8192];
        let mut err_chunk = [0; 8192];
        let mut out_closed = false;
        let mut err_closed = false;
        while !out_closed || !err_closed {
            let (count, is_stderr) = tokio::select! {
                read = stdout.read(&mut out_chunk), if !out_closed => (read?, false),
                read = stderr.read(&mut err_chunk), if !err_closed => (read?, true),
            };
            if count == 0 {
                if is_stderr {
                    err_closed = true;
                } else {
                    out_closed = true;
                }
                continue;
            }
            let retained = count.min(limit - self.stdout.len() - self.stderr.len());
            self.truncated |= retained < count;
            // Keep draining both pipes after the shared budget is exhausted so a
            // noisy plugin cannot deadlock on a full pipe. Scratch space is fixed.
            if is_stderr {
                self.stderr.extend_from_slice(&err_chunk[..retained]);
            } else {
                self.stdout.extend_from_slice(&out_chunk[..retained]);
            }
        }
        Ok(())
    }
}
