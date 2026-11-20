use super::{Store, TransitionKind, status_code};
use crate::{
    config::Config,
    runner::{CheckResult, Status},
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::Arc;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notification_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        host_id TEXT NOT NULL,
        service_id TEXT NOT NULL,
        episode_id INTEGER,
        kind TEXT NOT NULL CHECK (kind IN ('confirmed', 'severity_changed', 'recovery')),
        severity INTEGER NOT NULL,
        occurred_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS notification_events_by_service
        ON notification_events(host_id, service_id, id);
    CREATE INDEX IF NOT EXISTS notification_events_by_episode ON notification_events(episode_id);
    CREATE TABLE IF NOT EXISTS notification_deliveries (
        event_id INTEGER NOT NULL REFERENCES notification_events(id),
        recipient TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'retrying', 'delivered', 'failed', 'discarded')),
        attempts INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL,
        diagnostic TEXT,
        PRIMARY KEY (event_id, recipient)
    );
    CREATE INDEX IF NOT EXISTS notification_deliveries_due
        ON notification_deliveries(retry_at, event_id, recipient) WHERE state IN ('queued', 'retrying');";

/// A confirmed problem begins an episode; subsequent severity changes and its
/// recovery share that ID. This history is independent of check history pruning.
pub(super) fn record(
    tx: &Transaction<'_>,
    check: (&str, &str),
    kind: TransitionKind,
    previous_severity: Option<Status>,
    result: &CheckResult,
    recipients: &[String],
) -> Result<()> {
    if !matches!(
        kind,
        TransitionKind::Confirmed | TransitionKind::SeverityChanged | TransitionKind::Recovery
    ) || (kind == TransitionKind::Recovery && previous_severity.is_none())
    {
        return Ok(());
    }
    let (host_id, service_id) = check;
    let previous = tx
        .query_row(
            "SELECT id, COALESCE(episode_id, id), kind FROM notification_events
         WHERE host_id = ?1 AND service_id = ?2 ORDER BY id DESC LIMIT 1",
            params![host_id, service_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, TransitionKind>(2)?,
                ))
            },
        )
        .optional()?;
    if previous.is_none() && recipients.is_empty() {
        return Ok(());
    }
    let episode = previous
        .filter(|(_, _, previous_kind)| {
            kind != TransitionKind::Confirmed && *previous_kind != TransitionKind::Recovery
        })
        .map(|(_, episode, _)| episode);
    if let Some((id, _, _)) = previous {
        tx.execute(
            "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?2,
                    diagnostic = 'Superseded by a newer confirmed state'
             WHERE event_id = ?1 AND state IN ('queued', 'retrying')",
            params![id, result.finished_at],
        )?;
    }
    tx.execute(
        "INSERT INTO notification_events (host_id, service_id, episode_id, kind, severity, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![host_id, service_id, episode, kind.as_str(), status_code(result.status), result.finished_at],
    )?;
    let event = tx.last_insert_rowid();
    let ready_at = Utc::now().timestamp_millis();
    for recipient in recipients {
        // If SMTP is still in flight, record recovery intent conditionally. The
        // worker will require an actual acceptance before sending this recovery.
        if kind == TransitionKind::Recovery && !has_problem_delivery(tx, episode, recipient, true)?
        {
            continue;
        }
        tx.execute(
            "INSERT INTO notification_deliveries (event_id, recipient, state, updated_at, retry_at)
             VALUES (?1, ?2, 'queued', ?3, ?4) ON CONFLICT DO NOTHING",
            params![event, recipient, result.finished_at, ready_at],
        )?;
    }
    Ok(())
}

fn has_problem_delivery(
    connection: &Connection,
    episode: Option<i64>,
    recipient: &str,
    include_sending: bool,
) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM notification_events e
             JOIN notification_deliveries d ON d.event_id = e.id
             WHERE (e.id = ?1 OR e.episode_id = ?1) AND e.kind != 'recovery'
               AND d.recipient = ?2 AND (d.state = 'delivered' OR (?3 AND d.state = 'sending')))",
        params![episode, recipient, include_sending],
        |row| row.get(0),
    )?)
}

#[derive(Clone)]
pub struct Notification {
    pub event_id: i64,
    pub host_id: String,
    pub service_id: String,
    pub recipient: String,
    pub kind: TransitionKind,
    pub severity: Status,
    pub occurred_at: String,
    pub attempts: u32,
}

impl Notification {
    pub fn label(&self) -> String {
        match self.kind {
            TransitionKind::Recovery => "Recovery: OK".into(),
            TransitionKind::Confirmed => format!("Confirmed problem: {}", self.severity.label()),
            TransitionKind::SeverityChanged => {
                format!("Confirmed severity change: {}", self.severity.label())
            }
            _ => unreachable!("pending transitions do not create notifications"),
        }
    }
}

pub struct DeliveryRecord {
    pub notification: Notification,
    pub state: String,
    pub diagnostic: Option<String>,
    pub updated_at: String,
}

#[derive(Clone)]
pub enum DeliveryOutcome {
    Delivered,
    Retry(String),
    Failed(String),
    RecheckConfiguration,
}

impl Store {
    async fn notification_db<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = store
                .0
                .lock()
                .map_err(|_| anyhow!("database lock poisoned"))?;
            operation(&mut connection)
        })
        .await?
    }

    /// Only the single delivery worker calls this at startup. An interrupted
    /// SMTP attempt has no known outcome until the remote server confirms it.
    pub async fn resume_notifications(&self) -> Result<()> {
        self.notification_db(|db| {
            db.execute(
                "UPDATE notification_deliveries SET state = 'retrying', retry_at = 0,
                   updated_at = ?1, diagnostic = 'Interrupted SMTP attempt; outcome unknown'
                 WHERE state = 'sending'",
                [Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn claim_notification(&self, config: Arc<Config>) -> Result<Option<Notification>> {
        self.notification_db(move |db| {
            let tx = db.transaction()?;
            if config.smtp.is_none() {
                tx.execute(
                    "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?1,
                       diagnostic = 'Email disabled' WHERE state IN ('queued', 'retrying')",
                    [Utc::now().to_rfc3339()],
                )?;
                tx.commit()?;
                return Ok(None);
            }
            let now = Utc::now();
            loop {
                let candidate = tx.query_row(
                    "SELECT e.id, e.host_id, e.service_id, d.recipient, e.kind, e.severity,
                            e.occurred_at, d.attempts, e.episode_id,
                            EXISTS(SELECT 1 FROM notification_events newer WHERE newer.id > e.id
                                AND newer.host_id = e.host_id AND newer.service_id = e.service_id)
                            OR NOT EXISTS(SELECT 1 FROM check_problems p WHERE p.check_kind = 'service'
                                AND p.host_id = e.host_id AND p.service_id = e.service_id
                                AND p.confirmed_severity IS NULLIF(e.severity, 0))
                     FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                     WHERE d.state IN ('queued', 'retrying') AND d.retry_at <= ?1
                     ORDER BY d.retry_at, d.event_id, d.recipient LIMIT 1",
                    [now.timestamp_millis()],
                    |row| Ok((read_notification(row)?, row.get::<_, Option<i64>>(8)?, row.get::<_, bool>(9)?)),
                ).optional()?;
                let Some((mut candidate, episode, superseded)) = candidate else {
                    tx.commit()?;
                    return Ok(None);
                };
                let configured = config.hosts.iter().any(|host| host.id == candidate.host_id
                    && host.recipients.contains(&candidate.recipient)
                    && host.services.iter().any(|service| service.id == candidate.service_id));
                let obsolete = if superseded {
                    Some("Superseded by a newer confirmed state")
                } else if !configured {
                    Some("Service or recipient no longer configured")
                } else if candidate.kind == TransitionKind::Recovery
                    && !has_problem_delivery(&tx, episode, &candidate.recipient, false)?
                {
                    Some("No problem notification was delivered to this recipient")
                } else {
                    None
                };
                if let Some(reason) = obsolete {
                    tx.execute(
                        "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?3, diagnostic = ?4
                         WHERE event_id = ?1 AND recipient = ?2",
                        params![candidate.event_id, candidate.recipient, now.to_rfc3339(), reason],
                    )?;
                    continue;
                }
                tx.execute(
                    "UPDATE notification_deliveries SET state = 'sending', attempts = attempts + 1, updated_at = ?3
                     WHERE event_id = ?1 AND recipient = ?2",
                    params![candidate.event_id, candidate.recipient, now.to_rfc3339()],
                )?;
                tx.commit()?;
                candidate.attempts += 1;
                return Ok(Some(candidate));
            }
        }).await
    }

    pub async fn finish_notification(
        &self,
        notification: Notification,
        outcome: DeliveryOutcome,
    ) -> Result<()> {
        self.notification_db(move |db| {
            let now = Utc::now();
            let (state, diagnostic, delay, undo_attempt) = match outcome {
                DeliveryOutcome::Delivered => ("delivered", None, 0, false),
                DeliveryOutcome::Retry(message) => ("retrying", Some(message), (3_i64 << notification.attempts.saturating_sub(1).min(7)).min(300), false),
                DeliveryOutcome::Failed(message) => ("failed", Some(message), 0, false),
                DeliveryOutcome::RecheckConfiguration => ("queued", None, 0, true),
            };
            db.execute(
                "UPDATE notification_deliveries AS d SET
                    state = CASE WHEN ?3 IN ('queued', 'retrying') AND EXISTS (
                        SELECT 1 FROM notification_events e JOIN notification_events newer
                        ON newer.host_id = e.host_id AND newer.service_id = e.service_id AND newer.id > e.id
                        WHERE e.id = d.event_id
                    ) THEN 'discarded' ELSE ?3 END,
                    diagnostic = ?4, retry_at = ?5, updated_at = ?6, attempts = attempts - ?7
                 WHERE event_id = ?1 AND recipient = ?2 AND state = 'sending'",
                params![notification.event_id, notification.recipient, state, diagnostic,
                    now.timestamp_millis() + delay * 1000, now.to_rfc3339(), undo_attempt],
            )?;
            Ok(())
        }).await
    }

    pub async fn recent_deliveries(&self) -> Result<Vec<DeliveryRecord>> {
        self.notification_db(|db| {
            let mut statement = db.prepare(
                "SELECT e.id, e.host_id, e.service_id, d.recipient, e.kind, e.severity,
                        e.occurred_at, d.attempts, d.state, d.diagnostic, d.updated_at
                 FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                 ORDER BY d.event_id DESC, d.recipient LIMIT 100",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(DeliveryRecord {
                    notification: read_notification(row)?,
                    state: row.get(8)?,
                    diagnostic: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }
}

fn read_notification(row: &rusqlite::Row<'_>) -> rusqlite::Result<Notification> {
    Ok(Notification {
        event_id: row.get(0)?,
        host_id: row.get(1)?,
        service_id: row.get(2)?,
        recipient: row.get(3)?,
        kind: row.get(4)?,
        severity: Status::from_code(Some(row.get(5)?)),
        occurred_at: row.get(6)?,
        attempts: row.get(7)?,
    })
}
