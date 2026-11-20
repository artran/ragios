use crate::runner::{CheckResult, Status};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

pub type LatestResults = HashMap<(String, String), CheckResult>;

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
             );",
        )?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }

    pub async fn save(
        &self,
        host_id: String,
        service_id: String,
        result: CheckResult,
    ) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let connection = store
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
            let code = match result.status {
                Status::Ok => 0,
                Status::Warning => 1,
                Status::Critical => 2,
                Status::Unknown => 3,
            };
            connection.execute(
                "INSERT INTO latest_results VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(host_id, service_id) DO UPDATE SET
                    status=excluded.status, output=excluded.output,
                    started_at=excluded.started_at, finished_at=excluded.finished_at,
                    duration_ms=excluded.duration_ms",
                params![
                    host_id,
                    service_id,
                    code,
                    result.output,
                    result.started_at,
                    result.finished_at,
                    result.duration_ms
                ],
            )?;
            Ok(())
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
                "SELECT host_id, service_id, status, output, started_at, finished_at, duration_ms
                 FROM latest_results",
            )?;
            let rows = query.query_map([], |row| {
                Ok((
                    (row.get(0)?, row.get(1)?),
                    CheckResult {
                        status: Status::from_code(Some(row.get(2)?)),
                        output: row.get(3)?,
                        started_at: row.get(4)?,
                        finished_at: row.get(5)?,
                        duration_ms: row.get(6)?,
                    },
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await?
    }
}
