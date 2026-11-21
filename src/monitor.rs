use anyhow::{Context, Result, ensure};
use chrono::{DateTime, TimeDelta, Utc};
use ragios::{
    config::{Check, Config},
    runner::{self, CheckResult},
    schedule,
    store::{CheckTarget, ProblemState, Store},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
    time::Instant,
};

pub type Key = (String, String);

pub struct CheckRequest {
    pub key: Key,
    pub reply: oneshot::Sender<CheckRequestOutcome>,
}

pub enum CheckRequestOutcome {
    Started,
    AlreadyRunning,
    Busy,
    NotConfigured,
}

/// The longest the scheduler sleeps before recomputing start times from the
/// wall clock. Shorter than the shortest check window (one minute), so a
/// wall-clock step during a long wait cannot skip a window entirely.
const WALL_CLOCK_RECHECK: Duration = Duration::from_secs(30);

/// A failure that stops monitoring from making recorded progress. It persists
/// until every check whose result could not be saved has saved one again.
#[derive(Clone)]
pub struct SchedulerFault {
    pub since: DateTime<Utc>,
    pub message: String,
}

/// What operators need to know about the scheduler beyond saved results.
#[derive(Clone, Default)]
pub struct SchedulerStatus {
    pub fault: Option<SchedulerFault>,
    /// When each configured check without a saved result first became due.
    pub unchecked_since: HashMap<Key, DateTime<Utc>>,
}

struct ScheduledCheck {
    target: CheckTarget,
    check: Check,
    generation: u64,
    eligible: Instant,
    finished: Option<Instant>,
    retry: bool,
}

impl ScheduledCheck {
    fn deadline(&self, config: &Config) -> Instant {
        self.finished.map_or(self.eligible, |finished| {
            finished
                + Duration::from_secs(if self.retry {
                    config.retry_interval_seconds
                } else {
                    self.check.interval_seconds
                })
        })
    }

    /// When the check may next start: its deadline, or later while its window
    /// is closed. Catch-up after a restart therefore never runs through a
    /// closed window.
    fn start_at(&self, config: &Config, now: Instant, wall: DateTime<Utc>) -> Instant {
        let deadline = self.deadline(config).max(now);
        let at = wall + TimeDelta::from_std(deadline - now).expect("bounded deadline");
        deadline + schedule::window_wait(&self.check, at)
    }
}

// Host reachability checks use an empty second key element; service IDs are
// never empty, so the two namespaces never collide.
fn check_key(host_id: &str, target: &CheckTarget) -> Key {
    (
        host_id.to_string(),
        match target {
            CheckTarget::Host(_) => String::new(),
            CheckTarget::Service(id) => id.clone(),
        },
    )
}

// One owner serializes definition changes and result commits. Executions only
// return observations; they cannot publish definitions or write stale results.
struct Monitor {
    config: Arc<Config>,
    store: Store,
    checks: BTreeMap<Key, ScheduledCheck>,
    running: HashSet<Key>,
    tasks: JoinSet<(Key, u64, CheckResult)>,
    /// The check and generation behind each task, so a task that panics
    /// without returning its key can still be attributed.
    executions: HashMap<tokio::task::Id, (Key, u64)>,
    generation: u64,
    status: watch::Sender<SchedulerStatus>,
    /// Checks whose latest result could not be saved.
    unsaved: HashSet<Key>,
}

impl Monitor {
    fn new(config: Arc<Config>, store: Store, status: watch::Sender<SchedulerStatus>) -> Self {
        let mut monitor = Self {
            config: config.clone(),
            store,
            checks: BTreeMap::new(),
            running: HashSet::new(),
            tasks: JoinSet::new(),
            executions: HashMap::new(),
            generation: 0,
            status,
            unsaved: HashSet::new(),
        };
        monitor.replace(config);
        monitor
    }

    fn replace(&mut self, config: Arc<Config>) {
        let mut checks = BTreeMap::new();
        let mut added = Vec::new();
        for host in &config.hosts {
            let definitions = host
                .reachability
                .iter()
                .map(|probe| (CheckTarget::Host(probe.probe), &probe.check))
                .chain(
                    host.services
                        .iter()
                        .map(|service| (CheckTarget::Service(service.id.clone()), &service.check)),
                );
            for (target, check) in definitions {
                let key = check_key(&host.id, &target);
                if !self.checks.contains_key(&key) {
                    added.push(key.clone());
                }
                let previous = self.checks.remove(&key).filter(|old| {
                    self.config.plugin_dir == config.plugin_dir
                        && self.config.max_output_bytes == config.max_output_bytes
                        && old.target == target
                        && old.check.executable == check.executable
                        && old.check.args == check.args
                        && old.check.timeout_seconds == check.timeout_seconds
                });
                let mut scheduled = previous.unwrap_or_else(|| {
                    self.generation += 1;
                    ScheduledCheck {
                        target: target.clone(),
                        check: check.clone(),
                        generation: self.generation,
                        eligible: Instant::now(),
                        finished: None,
                        retry: false,
                    }
                });
                scheduled.check = check.clone();
                checks.insert(key, scheduled);
            }
        }
        // A removed check can no longer clear its fault by saving. An added
        // check is due now; one with a saved result is never shown as unchecked.
        self.unsaved.retain(|key| checks.contains_key(key));
        let now = Utc::now();
        self.status.send_modify(|status| {
            let unchecked = &mut status.unchecked_since;
            unchecked.retain(|key, _| checks.contains_key(key));
            for key in added {
                unchecked.entry(key).or_insert(now);
            }
        });
        self.checks = checks;
        self.config = config;
    }

    /// Resume the saved schedule instead of checking everything at once. Each
    /// check falls due one interval after its saved result; overdue checks are
    /// spread across the startup period. Windows are applied only when a check
    /// is about to start, so a reloaded window change takes effect from the
    /// preserved interval deadline.
    async fn restore(&mut self) -> Result<()> {
        let services = self.store.latest().await?;
        let hosts = self.store.latest_hosts().await?;
        let now = Utc::now();
        let saved = |key: &Key| -> Option<(&CheckResult, ProblemState)> {
            if key.1.is_empty() {
                hosts
                    .get(&key.0)
                    .map(|state| (&state.result, state.problem))
            } else {
                services
                    .get(key)
                    .map(|state| (&state.result, state.problem))
            }
        };
        let due = self
            .checks
            .iter()
            .map(|(key, scheduled)| {
                // A result without a known due time is checked like a new one.
                let due = saved(key).and_then(|(result, problem)| {
                    schedule::interval_due_at(
                        &scheduled.check,
                        self.config.retry_interval_seconds,
                        result.finished()?,
                        problem,
                        now,
                    )
                });
                (key.clone(), due)
            })
            .collect();
        self.status.send_modify(|status| {
            status.unchecked_since.retain(|key, _| saved(key).is_none());
        });
        let started = Instant::now();
        let spread = Duration::from_secs(self.config.startup_spread_seconds);
        for (key, delay) in schedule::startup_delays(due, now, spread) {
            self.checks.get_mut(&key).expect("scheduled check").eligible = started + delay;
        }
        Ok(())
    }

    fn next(&self, now: Instant, wall: DateTime<Utc>) -> Option<(Key, Instant)> {
        if self.tasks.len() >= self.config.max_concurrent_checks {
            return None;
        }
        // Overdue checks share the current instant as their start time, so the
        // oldest deadline goes first.
        self.checks
            .iter()
            .filter(|(key, _)| !self.running.contains(*key))
            .map(|(key, check)| {
                let start = check.start_at(&self.config, now, wall);
                (key.clone(), start, check.deadline(&self.config))
            })
            .min_by_key(|&(_, start, deadline)| (start, deadline))
            .map(|(key, start, _)| (key, start))
    }

    fn check_now(&mut self, key: Key) -> CheckRequestOutcome {
        // Resolve against the scheduler's active definitions, including reloads.
        if !self.checks.contains_key(&key) {
            return CheckRequestOutcome::NotConfigured;
        }
        if self.running.contains(&key) {
            return CheckRequestOutcome::AlreadyRunning;
        }
        if self.tasks.len() >= self.config.max_concurrent_checks {
            return CheckRequestOutcome::Busy;
        }
        self.start(key);
        CheckRequestOutcome::Started
    }

    fn start(&mut self, key: Key) {
        let scheduled = &self.checks[&key];
        let generation = scheduled.generation;
        let check = scheduled.check.clone();
        let config = self.config.clone();
        self.running.insert(key.clone());
        let task = self.tasks.spawn({
            let key = key.clone();
            async move {
                let result =
                    runner::execute(&config.plugin_dir, &check, config.max_output_bytes).await;
                (key, generation, result)
            }
        });
        self.executions.insert(task.id(), (key, generation));
    }

    /// An execution that stopped without a result is a fault, not a reason to
    /// stop every other check. Its check is retried at retry timing.
    fn abandon(&mut self, key: Key, generation: u64, error: tokio::task::JoinError) {
        self.running.remove(&key);
        if let Some(scheduled) = self.checks.get_mut(&key)
            && scheduled.generation == generation
        {
            scheduled.finished = Some(Instant::now());
            scheduled.retry = true;
            self.unsaved.insert(key);
        }
        self.report_fault(format!("a check execution stopped unexpectedly: {error}"));
    }

    async fn complete(&mut self, key: Key, generation: u64, result: CheckResult) {
        self.running.remove(&key);
        if let Some(scheduled) = self.checks.get_mut(&key)
            && scheduled.generation == generation
        {
            let saved = self
                .store
                .save_with_notifications(
                    key.0.clone(),
                    scheduled.target.clone(),
                    result,
                    self.config.clone(),
                )
                .await;
            scheduled.finished = Some(Instant::now());
            match saved {
                Ok(problem) => {
                    scheduled.retry = problem.needs_retry();
                    self.unsaved.remove(&key);
                    let recovered = self.unsaved.is_empty();
                    self.status.send_if_modified(|status| {
                        let checked = status.unchecked_since.remove(&key).is_some();
                        let cleared = recovered && status.fault.take().is_some();
                        checked || cleared
                    });
                }
                Err(error) => {
                    // The observation is lost; try again at retry timing
                    // rather than stopping every other check.
                    scheduled.retry = true;
                    self.unsaved.insert(key);
                    self.report_fault(format!("cannot record check results: {error:#}"));
                }
            }
        }
    }

    /// Report a fault in the log and to operators, keeping its first occurrence.
    fn report_fault(&self, message: String) {
        eprintln!("Scheduler fault: {message}");
        self.status.send_modify(|status| {
            let since = status
                .fault
                .as_ref()
                .map_or_else(Utc::now, |fault| fault.since);
            status.fault = Some(SchedulerFault { since, message });
        });
    }
}

pub async fn run(
    path: PathBuf,
    config: Arc<Config>,
    store: Store,
    published: watch::Sender<Arc<Config>>,
    status: watch::Sender<SchedulerStatus>,
    mut reloads: tokio::signal::unix::Signal,
    mut requests: mpsc::Receiver<CheckRequest>,
) -> Result<()> {
    let mut monitor = Monitor::new(config, store, status);
    if let Err(error) = monitor.restore().await {
        // Keep the fault visible until every check has saved a result again.
        monitor.unsaved = monitor.checks.keys().cloned().collect();
        monitor.report_fault(format!(
            "cannot restore the saved schedule, so every check starts now: {error:#}"
        ));
    }
    let mut validation: Option<oneshot::Receiver<Result<Config>>> = None;
    loop {
        let next = monitor.next(Instant::now(), Utc::now());
        tokio::select! {
            Some(request) = requests.recv() => {
                if !request.reply.is_closed() {
                    let _ = request.reply.send(monitor.check_now(request.key));
                }
            }
            Some(_) = reloads.recv(), if validation.is_none() => {
                let path = path.clone();
                let (sender, receiver) = oneshot::channel();
                // One read-only worker keeps monitoring responsive. Unlike
                // spawn_blocking, this thread cannot hold runtime shutdown
                // hostage if a filesystem read stalls. Only this owner can
                // accept the candidate; the worker never changes active state.
                match std::thread::Builder::new().name("config-validation".into()).spawn(move || {
                    let _ = sender.send(Config::load(&path));
                }) {
                    Ok(_) => validation = Some(receiver),
                    Err(error) => eprintln!("Configuration reload rejected: start validation: {error}"),
                }
            }
            candidate = async { validation.as_mut().expect("pending validation").await }, if validation.is_some() => {
                validation = None;
                let candidate = candidate.context("configuration validation thread stopped")
                    .and_then(|candidate| candidate);
                let candidate = candidate.and_then(|config| {
                    ensure!(config.bind == monitor.config.bind, "bind changed; restart required");
                    ensure!(config.database == monitor.config.database, "database changed; restart required");
                    Ok(Arc::new(config))
                });
                match candidate {
                    Ok(config) => {
                        if let Err(error) = monitor.store.publish_notification_config(config.clone(), published.clone()).await {
                            eprintln!("Configuration reload rejected: cannot persist silence holds: {error:#}");
                            continue;
                        }
                        monitor.replace(config);
                        eprintln!("Configuration reloaded");
                    }
                    Err(error) => eprintln!("Configuration reload rejected: {error:#}"),
                }
            }
            joined = monitor.tasks.join_next_with_id(), if !monitor.tasks.is_empty() => {
                match joined.context("missing execution")? {
                    Ok((id, (key, generation, result))) => {
                        monitor.executions.remove(&id);
                        monitor.complete(key, generation, result).await;
                    }
                    Err(error) => {
                        let (key, generation) = monitor.executions.remove(&error.id())
                            .context("unknown execution stopped")?;
                        monitor.abandon(key, generation, error);
                    }
                }
            }
            () = async {
                match &next {
                    Some((_, start)) => {
                        tokio::time::sleep_until((*start).min(Instant::now() + WALL_CLOCK_RECHECK)).await
                    }
                    None => std::future::pending().await,
                }
            } => {
                let (key, start) = next.expect("eligible check");
                // A long wait can outlast a wall-clock step. Re-check the
                // scheduled window immediately before launching the check.
                if start <= Instant::now()
                    && schedule::window_wait(&monitor.checks[&key].check, Utc::now()).is_zero()
                {
                    monitor.start(key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[tokio::test]
    async fn a_weekday_only_check_can_run_manually_on_saturday() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("check");
        fs::write(&executable, "#!/bin/sh\nprintf 'executed on request\\n'\n").unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("ragios.yaml");
        fs::write(
            &path,
            r#"
plugin_dir: .
database: state.sqlite3
check_window: weekdays
check_windows:
  - id: weekdays
    timezone: UTC
    ranges: [{days: [mon, tue, wed, thu, fri], start: '09:00', end: '17:00'}]
hosts:
  - id: local
    name: Local
    services:
      - {id: application, name: Application, executable: check, args: []}
"#,
        )
        .unwrap();
        let config = Arc::new(Config::load(&path).unwrap());
        let store = Store::open(&config.database).unwrap();
        let (status, _) = watch::channel(SchedulerStatus::default());
        let mut monitor = Monitor::new(config, store, status);
        let now = Instant::now();
        let saturday = Utc.with_ymd_and_hms(2026, 11, 21, 12, 0, 0).unwrap();
        let (key, scheduled) = monitor.next(now, saturday).unwrap();
        assert_eq!(scheduled - now, Duration::from_secs(45 * 3600)); // Monday 09:00
        assert!(matches!(
            monitor.check_now(key),
            CheckRequestOutcome::Started
        ));
        let (_, _, result) = monitor.tasks.join_next().await.unwrap().unwrap();
        assert_eq!(result.output.trim(), "executed on request");
    }
}
