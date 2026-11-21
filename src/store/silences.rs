use super::Store;
use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS silences (
        id INTEGER PRIMARY KEY,
        host_id TEXT,
        service_id TEXT,
        group_id TEXT,
        expires_at INTEGER NOT NULL,
        reason TEXT,
        CHECK ((host_id IS NOT NULL AND group_id IS NULL) OR
               (host_id IS NULL AND service_id IS NULL AND group_id IS NOT NULL))
    );
    CREATE INDEX IF NOT EXISTS silences_by_expiry ON silences(expires_at DESC);";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SilenceScope {
    Service { host: String, service: String },
    Host(String),
    Group(String),
}

impl SilenceScope {
    fn configured(&self, config: &Config) -> bool {
        match self {
            Self::Service { host, service } => config.hosts.iter().any(|candidate| {
                candidate.id == *host && candidate.services.iter().any(|item| item.id == *service)
            }),
            Self::Host(host) => config.hosts.iter().any(|candidate| candidate.id == *host),
            Self::Group(group) => config.groups.iter().any(|candidate| candidate.id == *group),
        }
    }
}

pub struct Silence {
    pub scope: SilenceScope,
    pub expires_at: DateTime<Utc>,
    pub reason: Option<String>,
}

pub enum CreateSilence {
    Created,
    InvalidScope,
    InvalidExpiry,
}

impl Store {
    pub async fn create_silence(
        &self,
        silence: Silence,
        active: watch::Receiver<Arc<Config>>,
    ) -> Result<CreateSilence> {
        self.notification_db(move |db| {
            let config = active.borrow();
            if !silence.scope.configured(&config) {
                return Ok(CreateSilence::InvalidScope);
            }
            let expiry = silence.expires_at.timestamp_millis();
            if expiry <= Utc::now().timestamp_millis() {
                return Ok(CreateSilence::InvalidExpiry);
            }
            let (host, service, group) = match &silence.scope {
                SilenceScope::Service { host, service } => (Some(host), Some(service), None),
                SilenceScope::Host(host) => (Some(host), None, None),
                SilenceScope::Group(group) => (None, None, Some(group)),
            };
            db.execute(
                "INSERT INTO silences (host_id, service_id, group_id, expires_at, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host, service, group, expiry, silence.reason],
            )?;
            Ok(CreateSilence::Created)
        })
        .await
    }

    /// Keep all active silences visible, plus the 100 most recently expired.
    pub async fn silences_at(&self, now: DateTime<Utc>) -> Result<Vec<Silence>> {
        self.notification_db(move |db| {
            let mut statement = db.prepare(
                "SELECT host_id, service_id, group_id, expires_at, reason FROM silences
                 WHERE expires_at > ?1 OR id IN (
                    SELECT id FROM silences WHERE expires_at <= ?1 ORDER BY expires_at DESC, id DESC LIMIT 100
                 ) ORDER BY expires_at DESC, id DESC",
            )?;
            let rows = statement.query_map([now.timestamp_millis()], read_silence)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }
}

fn read_silence(row: &Row<'_>) -> rusqlite::Result<Silence> {
    let scope = match (row.get(0)?, row.get(1)?) {
        (Some(host), Some(service)) => SilenceScope::Service { host, service },
        (Some(host), None) => SilenceScope::Host(host),
        (None, _) => SilenceScope::Group(row.get(2)?),
    };
    let expires_at = DateTime::from_timestamp_millis(row.get(3)?)
        .ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(3, row.get_unwrap(3)))?;
    Ok(Silence {
        scope,
        expires_at,
        reason: row.get(4)?,
    })
}

// Both query paths resolve group membership against the active configuration.
// json_each compares IDs as values, including IDs containing JSON path syntax.
pub(super) const ACTIVE_FILTER: &str = "NOT EXISTS (
    SELECT 1 FROM silences s WHERE s.expires_at > :now AND (
        (s.host_id = e.host_id AND (s.service_id IS NULL OR s.service_id = e.service_id))
        OR s.group_id = (SELECT value FROM json_each(:groups) WHERE key = e.host_id)
    )
)";

pub(super) fn latest_expiry(
    db: &Connection,
    host: &str,
    service: &str,
    group: Option<&str>,
) -> Result<Option<DateTime<Utc>>> {
    let milliseconds: Option<i64> = db.query_row(
        "SELECT MAX(expires_at) FROM silences
         WHERE (host_id = ?1 AND (service_id IS NULL OR service_id = ?2)) OR group_id = ?3",
        params![host, service, group],
        |row| row.get(0),
    )?;
    Ok(milliseconds.and_then(DateTime::from_timestamp_millis))
}
