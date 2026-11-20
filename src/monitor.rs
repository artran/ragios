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
    sync::{oneshot, watch},
    task::JoinSet,
    time::Instant,
};

pub type Key = (String, String);

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
    /// spread across the startup period.
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
                    schedule::due_at(
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

    fn next(&self) -> Option<(Key, Instant)> {
        if self.tasks.len() >= self.config.max_concurrent_checks {
            return None;
        }
        let (now, wall) = (Instant::now(), Utc::now());
        self.checks
            .iter()
            .filter(|(key, _)| !self.running.contains(*key))
            .map(|(key, check)| (key.clone(), check.start_at(&self.config, now, wall)))
            .min_by_key(|(_, deadline)| *deadline)
    }

    fn start(&mut self, key: Key) {
        let scheduled = &self.checks[&key];
        // A long wait can outlast a wall-clock step. Re-check the window so a
        // check never starts while it is closed; the next pass reschedules it.
        if !schedule::window_wait(&scheduled.check, Utc::now()).is_zero() {
            return;
        }
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
                .save_check(
                    key.0.clone(),
                    scheduled.target.clone(),
                    result,
                    self.config.confirmation_attempts,
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
) -> Result<()> {
    let mut monitor = Monitor {
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
    if let Err(error) = monitor.restore().await {
        // Keep the fault visible until every check has saved a result again.
        monitor.unsaved = monitor.checks.keys().cloned().collect();
        monitor.report_fault(format!(
            "cannot restore the saved schedule, so every check starts now: {error:#}"
        ));
    }
    let mut validation: Option<oneshot::Receiver<Result<Config>>> = None;
    loop {
        let next = monitor.next();
        tokio::select! {
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
                        monitor.replace(config.clone());
                        published.send_replace(config);
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
                    Some((_, deadline)) => tokio::time::sleep_until(*deadline).await,
                    None => std::future::pending().await,
                }
            } => monitor.start(next.expect("eligible check").0),
        }
    }
}
