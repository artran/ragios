use crate::runner::{CheckResult, Status};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, Row, params,
    types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

pub type LatestResults = HashMap<(String, String), ServiceState>;

pub struct ServiceState {
    pub result: CheckResult,
    pub problem: ProblemState,
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
pub struct Store(Arc<Mutex<Connection>>);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("open SQLite database {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS latest_results (
                host_id TEXT NOT NULL,
                service_id TEXT NOT NULL,
                status INTEGER NOT NULL,
                output TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                PRIMARY KEY (host_id, service_id)
             );
             CREATE TABLE IF NOT EXISTS problem_states (
                host_id TEXT NOT NULL,
                service_id TEXT NOT NULL,
                confirmed_severity INTEGER,
                pending_severity INTEGER,
                pending_attempts INTEGER NOT NULL,
                PRIMARY KEY (host_id, service_id)
             );
             CREATE TABLE IF NOT EXISTS problem_transitions (
                id INTEGER PRIMARY KEY,
                host_id TEXT NOT NULL,
                service_id TEXT NOT NULL,
                occurred_at TEXT NOT NULL,
                result_status INTEGER NOT NULL,
                kind TEXT NOT NULL,
                confirmed_severity INTEGER,
                pending_severity INTEGER,
                pending_attempts INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS transitions_by_service
                ON problem_transitions(host_id, service_id, id);
             INSERT INTO problem_states
                SELECT host_id, service_id, NULL,
                       CASE WHEN status = 0 THEN NULL ELSE status END,
                       CASE WHEN status = 0 THEN 0 ELSE 1 END
                FROM latest_results WHERE true
                ON CONFLICT(host_id, service_id) DO NOTHING;",
        )?;
        Ok(Self(Arc::new(Mutex::new(connection))))
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
            let previous = transaction
                .query_row(
                    "SELECT confirmed_severity, pending_severity, pending_attempts
                     FROM problem_states WHERE host_id = ?1 AND service_id = ?2",
                    params![host_id, service_id],
                    |row| read_problem(row, 0),
                )
                .optional()?
                .unwrap_or_default();
            let state = previous.advance(result.status, confirmation_attempts);
            transaction.execute(
                "INSERT INTO latest_results VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(host_id, service_id) DO UPDATE SET
                    status=excluded.status, output=excluded.output,
                    started_at=excluded.started_at, finished_at=excluded.finished_at,
                    duration_ms=excluded.duration_ms",
                params![
                    host_id,
                    service_id,
                    status_code(result.status),
                    result.output,
                    result.started_at,
                    result.finished_at,
                    result.duration_ms
                ],
            )?;
            transaction.execute(
                "INSERT INTO problem_states VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(host_id, service_id) DO UPDATE SET
                    confirmed_severity=excluded.confirmed_severity,
                    pending_severity=excluded.pending_severity,
                    pending_attempts=excluded.pending_attempts",
                params![
                    host_id,
                    service_id,
                    state.confirmed_severity.map(status_code),
                    state.pending_severity.map(status_code),
                    state.pending_attempts
                ],
            )?;
            if let Some(kind) = state.transition_from(previous) {
                transaction.execute(
                    "INSERT INTO problem_transitions
                     (host_id, service_id, occurred_at, result_status, kind,
                      confirmed_severity, pending_severity, pending_attempts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        host_id,
                        service_id,
                        result.finished_at,
                        status_code(result.status),
                        kind.as_str(),
                        state.confirmed_severity.map(status_code),
                        state.pending_severity.map(status_code),
                        state.pending_attempts
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(state)
        })
        .await?
    }

    pub async fn latest(&self) -> Result<LatestResults> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<LatestResults> {
            let connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            let mut query = connection.prepare(
                "SELECT r.host_id, r.service_id, r.status, r.output, r.started_at,
                        r.finished_at, r.duration_ms,
                        p.confirmed_severity, p.pending_severity, p.pending_attempts
                 FROM latest_results r JOIN problem_states p
                    ON r.host_id = p.host_id AND r.service_id = p.service_id",
            )?;
            let rows = query.query_map([], |row| {
                Ok((
                    (row.get(0)?, row.get(1)?),
                    ServiceState {
                        result: CheckResult {
                            status: Status::from_code(Some(row.get(2)?)),
                            output: row.get(3)?,
                            started_at: row.get(4)?,
                            finished_at: row.get(5)?,
                            duration_ms: row.get(6)?,
                        },
                        problem: read_problem(row, 7)?,
                    },
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
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
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<Transition>> {
            let connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            let mut query = connection.prepare(
                "SELECT id, occurred_at, result_status, kind,
                        confirmed_severity, pending_severity, pending_attempts
                 FROM problem_transitions WHERE host_id = ?1 AND service_id = ?2 ORDER BY id",
            )?;
            let rows = query.query_map(params![host_id, service_id], |row| {
                Ok(Transition {
                    id: row.get(0)?,
                    occurred_at: row.get(1)?,
                    result_status: Status::from_code(Some(row.get(2)?)),
                    kind: row.get(3)?,
                    state: read_problem(row, 4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await?
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
