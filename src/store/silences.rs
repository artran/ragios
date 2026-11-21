use super::Store;
use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Row, named_params, params};
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
        self.with_connection(move |db| {
            let tx = db.transaction()?;
            let config = active.borrow();
            if !silence.scope.configured(&config) {
                return Ok(CreateSilence::InvalidScope);
            }
            let expiry = silence.expires_at.timestamp_millis();
            let now = Utc::now();
            if expiry <= now.timestamp_millis() {
                return Ok(CreateSilence::InvalidExpiry);
            }
            let (host, service, group) = match &silence.scope {
                SilenceScope::Service { host, service } => (Some(host), Some(service), None),
                SilenceScope::Host(host) => (Some(host), None, None),
                SilenceScope::Group(group) => (None, None, Some(group)),
            };
            tx.execute(
                "INSERT INTO silences (host_id, service_id, group_id, expires_at, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host, service, group, expiry, silence.reason],
            )?;
            hold_pending(&tx, &host_groups(&config)?, now)?;
            tx.commit()?;
            Ok(CreateSilence::Created)
        })
        .await
    }

    /// Keep all active silences visible while paging through expired ones.
    pub async fn silences_at(
        &self,
        now: DateTime<Utc>,
        expired_page: u32,
    ) -> Result<(Vec<Silence>, bool)> {
        const PAGE_SIZE: i64 = 100;
        self.with_connection(move |db| {
            let mut statement = db.prepare(
                "SELECT host_id, service_id, group_id, expires_at, reason FROM silences
                 WHERE expires_at > ?1 OR id IN (
                    SELECT id FROM silences WHERE expires_at <= ?1 ORDER BY expires_at DESC, id DESC LIMIT ?2 OFFSET ?3
                 ) ORDER BY expires_at DESC, id DESC",
            )?;
            let rows = statement.query_map(params![now.timestamp_millis(), PAGE_SIZE + 1, PAGE_SIZE * i64::from(expired_page)], read_silence)?;
            let mut silences = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let has_older = silences.iter().rev().take_while(|silence| silence.expires_at <= now).count() as i64 > PAGE_SIZE;
            if has_older {
                silences.pop();
            }
            Ok((silences, has_older))
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

pub(super) fn is_silenced(
    db: &Connection,
    (host, service, group): (&str, &str, Option<&str>),
    now: DateTime<Utc>,
) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM silences WHERE expires_at > ?4
         AND ((host_id = ?1 AND (service_id IS NULL OR service_id = ?2)) OR group_id = ?3))",
        params![host, service, group, now.timestamp_millis()],
        |row| row.get(0),
    )?)
}

pub(super) fn host_groups(config: &Config) -> Result<String> {
    Ok(serde_json::to_string(
        &config
            .hosts
            .iter()
            .map(|host| (&host.id, &host.group))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )?)
}

pub(super) fn hold_pending(db: &Connection, groups: &str, now: DateTime<Utc>) -> Result<()> {
    // Only severity changes need the durable held marker: the delivery worker
    // can then reconcile them even after group membership changes or a restart.
    db.execute(
        &format!(
            "UPDATE notification_deliveries AS d SET held = 1
        WHERE state IN ('queued', 'retrying', 'sending') AND held = 0
          AND EXISTS (SELECT 1 FROM notification_events e WHERE e.id = d.event_id
            AND e.kind = 'severity_changed' AND NOT {ACTIVE_FILTER})"
        ),
        named_params! {":now": now.timestamp_millis(), ":groups": groups},
    )?;
    Ok(())
}
