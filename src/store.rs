mod browsing;
mod notifications;
mod schema;
mod silences;

pub use browsing::{CheckPath, CheckSnapshot, NotificationSnapshot, NotificationSuppression};

pub use notifications::{DeliveryOutcome, Notification};
pub use silences::{CreateSilence, Silence, SilenceScope};

use crate::{
    config::{Config, Probe},
    runner::{CheckResult, Reachability, Status},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use rusqlite::{
    Connection, OptionalExtension, Row, params,
    types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
};

pub type LatestResults = HashMap<(String, String), CheckState>;
pub type LatestHosts = HashMap<String, HostState>;

pub const TRANSITION_RETENTION: TimeDelta = TimeDelta::days(30);

pub struct HostState {
    pub probe: Probe,
    pub result: CheckResult,
    pub problem: ProblemState,
    pub episode: Option<ProblemEpisode>,
}

impl HostState {
    pub fn reachability(&self) -> Reachability {
        Reachability::from_status(self.result.status)
    }

    /// Eligibility for host-down notification suppression. Neither an
    /// unconfirmed failure nor confirmed undetermined reachability qualifies.
    pub fn is_confirmed_down(&self) -> bool {
        self.problem.confirmed_severity == Some(Status::Critical)
    }
}

pub struct CheckState {
    pub result: CheckResult,
    pub problem: ProblemState,
    pub episode: Option<ProblemEpisode>,
}

/// Current confirmed episode, independent of check history and email delivery.
pub struct ProblemEpisode {
    pub token: String,
    pub acknowledged: bool,
}

pub enum AcknowledgeOutcome {
    Acknowledged,
    NotConfigured,
    NoMatchingProblem,
}

/// A pending severity is either an initial failure or a candidate change to a
/// confirmed problem. It never replaces the confirmed severity until confirmed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProblemState {
    pub confirmed_severity: Option<Status>,
    pub pending_severity: Option<Status>,
    pub pending_attempts: u32,
}

impl ProblemState {
    pub fn needs_retry(self) -> bool {
        self.pending_severity.is_some()
    }

    fn advance(self, status: Status, confirmation_attempts: u32) -> Self {
        if status == Status::Ok {
            return Self::default();
        }
        if self.confirmed_severity == Some(status) {
            return Self {
                pending_severity: None,
                pending_attempts: 0,
                ..self
            };
        }
        let attempts = if self.confirmed_severity.is_none() || self.pending_severity == Some(status)
        {
            self.pending_attempts.saturating_add(1)
        } else {
            1
        };
        if attempts >= confirmation_attempts {
            Self {
                confirmed_severity: Some(status),
                ..Self::default()
            }
        } else {
            Self {
                confirmed_severity: self.confirmed_severity,
                pending_severity: Some(status),
                pending_attempts: attempts,
            }
        }
    }

    fn transition_from(self, previous: Self) -> Option<TransitionKind> {
        if self == previous {
            return None;
        }
        Some(if self == Self::default() {
            TransitionKind::Recovery
        } else if previous.confirmed_severity.is_none() {
            if self.confirmed_severity.is_some() {
                TransitionKind::Confirmed
            } else {
                TransitionKind::Pending
            }
        } else if self.confirmed_severity != previous.confirmed_severity {
            TransitionKind::SeverityChanged
        } else if self.pending_severity.is_some() {
            TransitionKind::SeverityChangePending
        } else {
            TransitionKind::SeverityChangeCancelled
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionKind {
    Pending,
    Confirmed,
    SeverityChangePending,
    SeverityChanged,
    SeverityChangeCancelled,
    Recovery,
}

impl TransitionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::SeverityChangePending => "severity_change_pending",
            Self::SeverityChanged => "severity_changed",
            Self::SeverityChangeCancelled => "severity_change_cancelled",
            Self::Recovery => "recovery",
        }
    }
}

impl FromSql for TransitionKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let name = value.as_str()?;
        [
            Self::Pending,
            Self::Confirmed,
            Self::SeverityChangePending,
            Self::SeverityChanged,
            Self::SeverityChangeCancelled,
            Self::Recovery,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == name)
        .ok_or_else(|| {
            FromSqlError::Other(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown transition kind {name:?}"),
                )
                .into(),
            )
        })
    }
}

pub struct Transition {
    pub id: i64,
    pub occurred_at: String,
    pub result_status: Status,
    pub kind: TransitionKind,
    pub state: ProblemState,
}

#[derive(Clone)]
pub struct Store(Arc<Mutex<Connection>>, Option<Arc<File>>);

// Host checks have their own namespace, never a reserved user service ID.
#[derive(Clone, PartialEq, Eq)]
pub enum CheckTarget {
    Host(Probe),
    Service(String),
}

#[derive(Clone, Copy)]
enum CheckKind {
    Host,
    Service,
}

impl CheckKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Service => "service",
        }
    }
}

impl CheckTarget {
    fn kind(&self) -> CheckKind {
        match self {
            Self::Host(_) => CheckKind::Host,
            Self::Service(_) => CheckKind::Service,
        }
    }

    fn service_id(&self) -> &str {
        match self {
            Self::Host(_) => "",
            Self::Service(id) => id,
        }
    }

    fn probe(&self) -> Option<Probe> {
        match self {
            Self::Host(probe) => Some(*probe),
            Self::Service(_) => None,
        }
    }
}

struct StoredCheck {
    host_id: String,
    service_id: String,
    state: CheckState,
    probe: Option<Probe>,
}

const CHECK_STATE_SELECT: &str = "SELECT r.host_id, r.service_id, r.status, r.output, r.started_at,
    r.finished_at, r.duration_ms, p.confirmed_severity, p.pending_severity, p.pending_attempts,
    r.probe, e.token, COALESCE(e.acknowledged, 0)
    FROM check_results r JOIN check_problems p
        ON r.host_id = p.host_id AND r.service_id = p.service_id AND r.check_kind = p.check_kind
    LEFT JOIN check_problem_episodes e ON r.host_id = e.host_id AND r.service_id = e.service_id";

impl StoredCheck {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            host_id: row.get(0)?,
            service_id: row.get(1)?,
            state: CheckState {
                result: CheckResult {
                    status: Status::from_code(Some(row.get(2)?)),
                    output: row.get(3)?,
                    started_at: row.get(4)?,
                    finished_at: row.get(5)?,
                    duration_ms: row.get(6)?,
                },
                problem: read_problem(row, 7)?,
                episode: match row.get(11)? {
                    Some(token) => Some(ProblemEpisode {
                        token,
                        acknowledged: row.get(12)?,
                    }),
                    None => None,
                },
            },
            probe: row.get(10)?,
        })
    }

    fn into_host(self) -> Result<(String, HostState)> {
        Ok((
            self.host_id,
            HostState {
                probe: self.probe.context("host result missing probe origin")?,
                result: self.state.result,
                problem: self.state.problem,
                episode: self.state.episode,
            },
        ))
    }
}

impl Store {
    async fn with_connection<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            operation(&mut connection)
        })
        .await?
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut connection = Connection::open(path)
            .with_context(|| format!("open SQLite database {}", path.display()))?;
        schema::initialize(&mut connection)?;
        Ok(Self(Arc::new(Mutex::new(connection)), None))
    }

    /// Only a server holding this advisory lock may run the delivery worker.
    /// Keep a separate lock file: SQLite can use advisory locks on the database
    /// itself. Never unlink the lock file, as contenders may already have it open.
    pub fn open_server(path: &Path) -> Result<Self> {
        crate::config::validate_database_path(path)?;
        // Resolve aliases before deriving the lock path. Create an empty file
        // on first start without opening SQLite before the lock is acquired.
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open SQLite database {}", path.display()))?;
        let database = path
            .canonicalize()
            .context("resolve database path for server lock")?;
        let mut lock_path = database.as_os_str().to_os_string();
        lock_path.push(".ragios.lock");
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .context("open Ragios server lock file")?;
        file.try_lock().with_context(|| format!("acquire exclusive Ragios server lock for database {} (another instance may be running)", database.display()))?;
        let mut store = Self::open(&database)?;
        store.1 = Some(Arc::new(file));
        Ok(store)
    }

    /// Persist the result, problem state, and any transition atomically. The
    /// returned state determines whether the next execution needs retry timing.
    pub async fn save(
        &self,
        host_id: String,
        service_id: String,
        result: CheckResult,
        confirmation_attempts: u32,
    ) -> Result<ProblemState> {
        self.save_check(
            host_id,
            CheckTarget::Service(service_id),
            result,
            confirmation_attempts,
        )
        .await
    }

    pub async fn save_host(
        &self,
        host_id: String,
        probe: Probe,
        result: CheckResult,
        confirmation_attempts: u32,
    ) -> Result<ProblemState> {
        self.save_check(
            host_id,
            CheckTarget::Host(probe),
            result,
            confirmation_attempts,
        )
        .await
    }

    pub async fn save_check(
        &self,
        host_id: String,
        target: CheckTarget,
        result: CheckResult,
        confirmation_attempts: u32,
    ) -> Result<ProblemState> {
        self.save_check_result(host_id, target, result, confirmation_attempts, None)
            .await
    }

    /// Record using the scheduler's current configuration, after it rejects
    /// obsolete execution generations. Notification policy stays in its module.
    pub async fn save_with_notifications(
        &self,
        host_id: String,
        target: CheckTarget,
        result: CheckResult,
        config: Arc<Config>,
    ) -> Result<ProblemState> {
        self.save_check_result(
            host_id,
            target,
            result,
            config.confirmation_attempts,
            Some(config),
        )
        .await
    }

    async fn save_check_result(
        &self,
        host_id: String,
        target: CheckTarget,
        result: CheckResult,
        confirmation_attempts: u32,
        config: Option<Arc<Config>>,
    ) -> Result<ProblemState> {
        ensure!(
            confirmation_attempts > 0,
            "confirmation_attempts must be positive"
        );
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<ProblemState> {
            let mut connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            let transaction = connection.transaction()?;
            let kind = target.kind().as_str();
            let service_id = target.service_id();
            let previous = transaction
                .query_row(
                    "SELECT confirmed_severity, pending_severity, pending_attempts
                     FROM check_problems WHERE host_id = ?1 AND service_id = ?2 AND check_kind = ?3",
                    params![host_id, service_id, kind],
                    |row| read_problem(row, 0),
                )
                .optional()?
                .unwrap_or_default();
            let assessment = if matches!(target, CheckTarget::Host(_))
                && Reachability::from_status(result.status) == Reachability::Reachable
            {
                Status::Ok
            } else {
                result.status
            };
            let state = previous.advance(assessment, confirmation_attempts);
            transaction.execute(
                "INSERT INTO check_results (host_id, service_id, status, output, started_at, finished_at, duration_ms, check_kind, probe)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(check_kind, host_id, service_id) DO UPDATE SET
                    status=excluded.status, output=excluded.output,
                    started_at=excluded.started_at, finished_at=excluded.finished_at,
                    duration_ms=excluded.duration_ms, probe=excluded.probe",
                params![
                    host_id,
                    service_id,
                    status_code(result.status),
                    result.output,
                    result.started_at,
                    result.finished_at,
                    result.duration_ms,
                    kind,
                    target.probe().map(Probe::label)
                ],
            )?;
            transaction.execute(
                "INSERT INTO check_problems (host_id, service_id, confirmed_severity, pending_severity, pending_attempts, check_kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(check_kind, host_id, service_id) DO UPDATE SET
                    confirmed_severity=excluded.confirmed_severity,
                    pending_severity=excluded.pending_severity,
                    pending_attempts=excluded.pending_attempts",
                params![
                    host_id,
                    service_id,
                    state.confirmed_severity.map(status_code),
                    state.pending_severity.map(status_code),
                    state.pending_attempts,
                    kind
                ],
            )?;
            if let Some(kind) = state.transition_from(previous) {
                if kind == TransitionKind::Confirmed {
                    transaction.execute(
                        "INSERT INTO check_problem_episodes (host_id, service_id, token)
                         VALUES (?1, ?2, lower(hex(randomblob(16))))",
                        params![host_id, service_id],
                    )?;
                }
                transaction.execute(
                    "INSERT INTO check_transitions
                     (host_id, service_id, occurred_at, result_status, kind,
                      confirmed_severity, pending_severity, pending_attempts, check_kind)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        host_id,
                        service_id,
                        result.finished_at,
                        status_code(result.status),
                        kind.as_str(),
                        state.confirmed_severity.map(status_code),
                        state.pending_severity.map(status_code),
                        state.pending_attempts,
                        target.kind().as_str()
                    ],
                )?;
                notifications::record(&transaction, (&host_id, &target), kind, previous.confirmed_severity, assessment, &result.finished_at, config.as_deref())?;
            }
            transaction.commit()?;
            Ok(state)
        })
        .await?
    }

    pub async fn acknowledge_problem(
        &self,
        active: tokio::sync::watch::Receiver<Arc<Config>>,
        host_id: String,
        service_id: Option<String>,
        token: String,
    ) -> Result<AcknowledgeOutcome> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            // Keep the active identity and current episode stable until the write completes.
            let config = active.borrow();
            let configured = config.hosts.iter().any(|host| {
                host.id == host_id
                    && match &service_id {
                        Some(id) => {
                            !id.is_empty() && host.services.iter().any(|service| service.id == *id)
                        }
                        None => host.reachability.is_some(),
                    }
            });
            if !configured {
                return Ok(AcknowledgeOutcome::NotConfigured);
            }
            let updated = connection.execute(
                "UPDATE check_problem_episodes SET acknowledged = 1
                 WHERE host_id = ?1 AND service_id = ?2 AND token = ?3",
                params![host_id, service_id.as_deref().unwrap_or_default(), token],
            )?;
            Ok(if updated == 0 {
                AcknowledgeOutcome::NoMatchingProblem
            } else {
                AcknowledgeOutcome::Acknowledged
            })
        })
        .await?
    }

    pub async fn latest(&self) -> Result<LatestResults> {
        Ok(self
            .latest_checks(CheckKind::Service)
            .await?
            .into_iter()
            .map(|saved| ((saved.host_id, saved.service_id), saved.state))
            .collect())
    }

    pub async fn latest_hosts(&self) -> Result<LatestHosts> {
        self.latest_checks(CheckKind::Host)
            .await?
            .into_iter()
            .map(StoredCheck::into_host)
            .collect()
    }

    async fn latest_checks(&self, kind: CheckKind) -> Result<Vec<StoredCheck>> {
        self.with_connection(move |db| {
            let mut query = db.prepare(&format!("{CHECK_STATE_SELECT} WHERE r.check_kind = ?1"))?;
            let rows = query.query_map([kind.as_str()], StoredCheck::read)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Delete state changes older than the retention period before `now`.
    /// Latest results and problem states are current state, never history,
    /// so a long-standing problem survives even when all its changes are old.
    pub async fn prune_transitions(&self, now: DateTime<Utc>) -> Result<usize> {
        let cutoff = (now - TRANSITION_RETENTION).to_rfc3339_opts(SecondsFormat::Millis, true);
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<usize> {
            let connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            // Timestamps share one fixed-width UTC format, so text order is time order.
            Ok(connection.execute(
                "DELETE FROM check_transitions WHERE occurred_at < ?1",
                [cutoff],
            )?)
        })
        .await?
    }

    /// Ordered lifecycle transitions, including pending streak progress. Stable
    /// confirmed results and initial OK results do not create transitions.
    pub async fn transitions(
        &self,
        host_id: String,
        service_id: String,
    ) -> Result<Vec<Transition>> {
        self.with_connection(move |db| {
            read_transitions(db, CheckKind::Service, &host_id, &service_id)
        })
        .await
    }

    pub async fn host_transitions(&self, host_id: String) -> Result<Vec<Transition>> {
        self.with_connection(move |db| read_transitions(db, CheckKind::Host, &host_id, ""))
            .await
    }
}

fn read_transitions(
    db: &Connection,
    kind: CheckKind,
    host_id: &str,
    service_id: &str,
) -> Result<Vec<Transition>> {
    let mut query = db.prepare(
        "SELECT id, occurred_at, result_status, kind,
                confirmed_severity, pending_severity, pending_attempts
         FROM check_transitions WHERE host_id = ?1 AND service_id = ?2 AND check_kind = ?3 ORDER BY id",
    )?;
    let rows = query.query_map(params![host_id, service_id, kind.as_str()], |row| {
        Ok(Transition {
            id: row.get(0)?,
            occurred_at: row.get(1)?,
            result_status: Status::from_code(Some(row.get(2)?)),
            kind: row.get(3)?,
            state: read_problem(row, 4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

impl FromSql for Probe {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let label = value.as_str()?;
        [Probe::Ping, Probe::Tcp]
            .into_iter()
            .find(|probe| probe.label() == label)
            .ok_or_else(|| {
                FromSqlError::Other(
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown probe {label:?}"),
                    )
                    .into(),
                )
            })
    }
}

fn status_code(status: Status) -> i32 {
    match status {
        Status::Ok => 0,
        Status::Warning => 1,
        Status::Critical => 2,
        Status::Unknown => 3,
    }
}

fn read_problem(row: &Row<'_>, offset: usize) -> rusqlite::Result<ProblemState> {
    Ok(ProblemState {
        confirmed_severity: row
            .get::<_, Option<i32>>(offset)?
            .map(|code| Status::from_code(Some(code))),
        pending_severity: row
            .get::<_, Option<i32>>(offset + 1)?
            .map(|code| Status::from_code(Some(code))),
        pending_attempts: row.get(offset + 2)?,
    })
}
