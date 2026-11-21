use super::{Store, TransitionKind, status_code};
use crate::{
    config::{CheckWindow, Config},
    runner::{Reachability, Status},
};
use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::sync::Arc;
use tokio::sync::watch;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notification_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        host_id TEXT NOT NULL,
        service_id TEXT NOT NULL,
        episode_id INTEGER,
        kind TEXT NOT NULL CHECK (kind IN ('confirmed', 'severity_changed', 'recovery')),
        severity INTEGER NOT NULL,
        occurred_at TEXT NOT NULL,
        reminder_for INTEGER
    );
    CREATE INDEX IF NOT EXISTS notification_events_by_service
        ON notification_events(host_id, service_id, id);
    CREATE INDEX IF NOT EXISTS notification_events_by_episode ON notification_events(episode_id);
    CREATE INDEX IF NOT EXISTS notification_recoveries_by_time
        ON notification_events(occurred_at, id) WHERE kind = 'recovery';
    CREATE TABLE IF NOT EXISTS notification_deliveries (
        event_id INTEGER NOT NULL REFERENCES notification_events(id),
        recipient TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'retrying', 'delivered', 'failed', 'discarded')),
        attempts INTEGER NOT NULL DEFAULT 0,
        retry_at INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL,
        diagnostic TEXT,
        held INTEGER NOT NULL DEFAULT 0,
        remind_from INTEGER,
        PRIMARY KEY (event_id, recipient)
    );
    CREATE INDEX IF NOT EXISTS notification_deliveries_due
        ON notification_deliveries(retry_at, event_id, recipient) WHERE state IN ('queued', 'retrying');
    CREATE INDEX IF NOT EXISTS notification_deliveries_recent
        ON notification_deliveries(updated_at DESC, event_id DESC, recipient);";

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<()> {
    if !has_column(tx, "notification_deliveries", "held")? {
        // Recovery can be recorded before the delivery worker starts, so
        // preserve suppression history for the existing outbox during upgrade.
        tx.execute_batch(
            "ALTER TABLE notification_deliveries ADD COLUMN held INTEGER NOT NULL DEFAULT 0;
             UPDATE notification_deliveries SET held = 1
             WHERE state IN ('queued', 'retrying', 'sending') AND event_id IN (
                 SELECT e.id FROM notification_events e JOIN check_problems p ON p.host_id = e.host_id
                 WHERE e.service_id != '' AND p.check_kind = 'host' AND p.service_id = ''
                   AND p.confirmed_severity = 2
             );",
        )?;
    }
    if !has_column(tx, "notification_events", "reminder_for")? {
        tx.execute_batch("ALTER TABLE notification_events ADD COLUMN reminder_for INTEGER;")?;
    }
    if !has_column(tx, "notification_deliveries", "remind_from")? {
        tx.execute_batch(
            "ALTER TABLE notification_deliveries ADD COLUMN remind_from INTEGER;
             UPDATE notification_deliveries AS d SET remind_from = retry_at
             WHERE state IN ('delivered', 'failed') AND EXISTS (
                 SELECT 1 FROM notification_events e WHERE e.id = d.event_id AND e.kind != 'recovery'
                   AND NOT EXISTS (SELECT 1 FROM notification_events newer
                     WHERE newer.host_id = e.host_id AND newer.service_id = e.service_id AND newer.id > e.id)
             );",
        )?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS notification_reminders_by_event
             ON notification_events(reminder_for) WHERE reminder_for IS NOT NULL;
         CREATE INDEX IF NOT EXISTS notification_reminders_due
             ON notification_deliveries(remind_from) WHERE remind_from IS NOT NULL;",
    )?;
    Ok(())
}

fn has_column(db: &Connection, table: &str, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        params![table, name],
        |row| row.get(0),
    )?)
}

fn host_is_down(db: &Connection, host: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM check_problems WHERE check_kind = 'host'
            AND host_id = ?1 AND service_id = '' AND confirmed_severity = 2)",
        [host],
        |row| row.get(0),
    )?)
}

// This marker survives restarts and lets dispatch reconcile against what each
// recipient actually saw. The host's current confirmed state gates dispatch.
pub(super) fn hold_services(tx: &Transaction<'_>, host: &str) -> Result<()> {
    tx.execute(
        "UPDATE notification_deliveries SET held = 1
         WHERE state IN ('queued', 'retrying', 'sending') AND event_id IN (
            SELECT id FROM notification_events WHERE host_id = ?1 AND service_id != ''
         )",
        [host],
    )?;
    Ok(())
}

fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Default)]
pub struct NotificationPolicy {
    pub recipients: Vec<String>,
    pub window: Option<CheckWindow>,
}

/// A confirmed problem begins an episode; subsequent severity changes and its
/// recovery share that ID. This history is independent of check history pruning.
pub(super) fn record(
    tx: &Transaction<'_>,
    check: (&str, &str),
    kind: TransitionKind,
    previous_severity: Option<Status>,
    severity: Status,
    occurred_at: &str,
    policy: &NotificationPolicy,
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
         WHERE host_id = ?1 AND service_id = ?2 AND reminder_for IS NULL ORDER BY id DESC LIMIT 1",
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
    let episode = previous
        .filter(|(_, _, previous_kind)| {
            kind != TransitionKind::Confirmed && *previous_kind != TransitionKind::Recovery
        })
        .map(|(_, episode, _)| episode);
    if policy.recipients.is_empty() {
        match (kind, episode, previous) {
            // Keep one recovery marker to close an existing episode.
            (TransitionKind::Recovery, Some(_), _) => {}
            // One marker supersedes the last delivery during disabled or
            // recipient-free monitoring; further empty changes add no history.
            (TransitionKind::SeverityChanged, Some(_), Some((id, _, _)))
                if tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM notification_deliveries WHERE event_id = ?1)",
                    [id],
                    |row| row.get::<_, bool>(0),
                )? => {}
            _ => return Ok(()),
        }
    }
    let now = Utc::now();
    let updated_at = timestamp(now);
    if let Some((id, _, _)) = previous {
        tx.execute(
            "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?2,
                    diagnostic = 'Superseded by a newer confirmed state'
             WHERE (event_id = ?1 OR event_id IN (SELECT id FROM notification_events WHERE reminder_for = ?1))
               AND state IN ('queued', 'retrying')",
            params![id, updated_at],
        )?;
        tx.execute("UPDATE notification_deliveries SET remind_from = NULL WHERE event_id = ?1 AND remind_from IS NOT NULL", [id])?;
    }
    tx.execute(
        "INSERT INTO notification_events (host_id, service_id, episode_id, kind, severity, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![host_id, service_id, episode, kind.as_str(), status_code(severity), occurred_at],
    )?;
    let event = tx.last_insert_rowid();
    let ready_at = now.timestamp_millis();
    let held = policy.window.as_ref().is_some_and(|window| {
        let observed_at = DateTime::parse_from_rfc3339(occurred_at)
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or(now);
        !window.is_open(observed_at)
    }) || (!service_id.is_empty() && host_is_down(tx, host_id)?);
    for recipient in &policy.recipients {
        // If SMTP is still in flight, record recovery intent conditionally. The
        // worker will require an actual acceptance before sending this recovery.
        if kind == TransitionKind::Recovery && !has_problem_delivery(tx, episode, recipient, true)?
        {
            continue;
        }
        tx.execute(
            "INSERT INTO notification_deliveries (event_id, recipient, state, updated_at, retry_at, held)
             VALUES (?1, ?2, 'queued', ?3, ?4, ?5) ON CONFLICT DO NOTHING",
            params![event, recipient, updated_at, ready_at, held],
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
    reminder: bool,
}

impl Notification {
    pub fn target_label(&self) -> &str {
        if self.service_id.is_empty() {
            "Host reachability"
        } else {
            &self.service_id
        }
    }

    pub fn label(&self) -> String {
        if self.reminder {
            let state = if self.service_id.is_empty() {
                Reachability::from_status(self.severity).label()
            } else {
                self.severity.label()
            };
            return format!("Reminder: {state}");
        }
        if self.service_id.is_empty() {
            let state = Reachability::from_status(self.severity).label();
            return match self.kind {
                TransitionKind::Recovery => format!("Recovery: {state}"),
                TransitionKind::Confirmed => format!("Confirmed problem: {state}"),
                TransitionKind::SeverityChanged => {
                    format!("Confirmed reachability change: {state}")
                }
                _ => unreachable!("pending transitions do not create notifications"),
            };
        }
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
    pub reminder_from: Option<DateTime<Utc>>,
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
        ensure!(
            self.1.is_some(),
            "email delivery requires the exclusive server database lock"
        );
        self.notification_db(|db| {
            db.execute(
                "UPDATE notification_deliveries SET state = 'retrying', retry_at = 0,
                   updated_at = ?1, diagnostic = 'Interrupted SMTP attempt; outcome unknown'
                 WHERE state = 'sending'",
                [timestamp(Utc::now())],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn next_notification(
        &self,
        active: watch::Receiver<Arc<Config>>,
    ) -> Result<Option<Notification>> {
        self.next_notification_at(active, Utc::now()).await
    }

    pub async fn next_notification_at(
        &self,
        active: watch::Receiver<Arc<Config>>,
        now: DateTime<Utc>,
    ) -> Result<Option<Notification>> {
        ensure!(
            self.1.is_some(),
            "email delivery requires the exclusive server database lock"
        );
        self.notification_db(move |db| {
            let tx = db.transaction()?;
            // Keep the configuration stable until reconciliation commits.
            let config = active.borrow();
            if config.smtp.is_none() {
                tx.execute(
                    "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?1,
                       diagnostic = 'Email disabled' WHERE state IN ('queued', 'retrying')",
                    [timestamp(now)],
                )?;
                tx.execute("UPDATE notification_deliveries SET remind_from = NULL WHERE remind_from IS NOT NULL", [])?;
                tx.commit()?;
                return Ok(None);
            }
            let reachability_hosts = serde_json::to_string(
                &config
                    .hosts
                    .iter()
                    .filter(|host| host.reachability.is_some())
                    .map(|host| &host.id)
                    .collect::<Vec<_>>(),
            )?;
            let closed_targets = config.hosts.iter().flat_map(|host| {
                host.notification_windows()
                    .filter(|(_, window)| !window.is_open(now))
                    .map(|(id, _)| (&host.id, id))
            }).collect::<Vec<_>>();
            let closed_targets = if closed_targets.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&closed_targets)?)
            };
            // Remember changes that were queued before the window closed (e.g.
            // an SMTP retry). Recording transitions also marks those made while closed.
            if let Some(closed_targets) = &closed_targets {
                tx.execute(
                    "UPDATE notification_deliveries AS d SET held = 1
                     WHERE d.state IN ('queued', 'retrying') AND d.held = 0 AND EXISTS (
                         SELECT 1 FROM notification_events e WHERE e.id = d.event_id
                           AND e.kind = 'severity_changed' AND e.reminder_for IS NULL
                           AND (e.host_id, e.service_id) IN (
                               SELECT value ->> 0, value ->> 1 FROM json_each(?1)
                           )
                     )",
                    [closed_targets],
                )?;
            }
            // Materialize at most one reminder per poll; no backlog accumulates
            // during restarts, suppression, or an unfinished SMTP attempt.
            queue_due_reminder(&tx, &config, now, &reachability_hosts, closed_targets.as_deref())?;
            // Bound how much reconciliation can delay check writes or reload.
            for _ in 0..32 {
                let candidate = tx
                    .query_row(
                        "SELECT e.id, e.host_id, e.service_id, d.recipient, e.kind, e.severity,
                            e.occurred_at, d.attempts, e.reminder_for IS NOT NULL
                     FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                     WHERE d.state IN ('queued', 'retrying') AND d.retry_at <= ?1
                       AND (e.service_id = '' OR e.host_id NOT IN (
                           SELECT host_id FROM check_problems WHERE check_kind = 'host'
                             AND service_id = '' AND confirmed_severity = 2
                             AND host_id IN (SELECT value FROM json_each(?2))
                       ))
                       AND (e.host_id, e.service_id) NOT IN (
                           SELECT value ->> 0, value ->> 1 FROM json_each(?3)
                       )
                     ORDER BY d.retry_at, d.event_id, d.recipient LIMIT 1",
                        params![now.timestamp_millis(), reachability_hosts, closed_targets],
                        read_notification,
                    )
                    .optional()?;
                let Some(candidate) = candidate else {
                    tx.commit()?;
                    return Ok(None);
                };
                if let Some(reason) = obsolete_reason(&tx, &config, &candidate)? {
                    discard(&tx, &candidate, reason)?;
                    continue;
                }
                tx.commit()?;
                return Ok(Some(candidate));
            }
            tx.commit()?;
            Ok(None)
        })
        .await
    }

    /// Revalidate at the dispatch boundary. After this commit the attempt is
    /// treated as in flight; SMTP I/O never holds the database lock.
    pub async fn begin_notification(
        &self,
        notification: Notification,
        active: watch::Receiver<Arc<Config>>,
        relay_config: Arc<Config>,
    ) -> Result<Option<Notification>> {
        self.begin_notification_with_clock(notification, active, relay_config, Utc::now)
            .await
    }

    pub async fn begin_notification_at(
        &self,
        notification: Notification,
        active: watch::Receiver<Arc<Config>>,
        relay_config: Arc<Config>,
        now: DateTime<Utc>,
    ) -> Result<Option<Notification>> {
        self.begin_notification_with_clock(notification, active, relay_config, move || now)
            .await
    }

    async fn begin_notification_with_clock(
        &self,
        mut notification: Notification,
        active: watch::Receiver<Arc<Config>>,
        relay_config: Arc<Config>,
        clock: impl FnOnce() -> DateTime<Utc> + Send + 'static,
    ) -> Result<Option<Notification>> {
        ensure!(
            self.1.is_some(),
            "email delivery requires the exclusive server database lock"
        );
        self.notification_db(move |db| {
            let tx = db.transaction()?;
            let config = active.borrow();
            if !Arc::ptr_eq(&config, &relay_config) || config.smtp.is_none() {
                return Ok(None);
            }
            let host = config.hosts.iter().find(|host| host.id == notification.host_id);
            let now = clock();
            if host.and_then(|host| host.notification_window(&notification.service_id))
                .is_some_and(|window| !window.is_open(now))
            {
                tx.execute(
                    "UPDATE notification_deliveries SET held = 1 WHERE event_id = ?1 AND recipient = ?2
                     AND state IN ('queued', 'retrying') AND held = 0",
                    params![notification.event_id, notification.recipient],
                )?;
                tx.commit()?;
                return Ok(None);
            }
            if !notification.service_id.is_empty()
                && host.is_some_and(|host| host.reachability.is_some())
                && host_is_down(&tx, &notification.host_id)?
            {
                return Ok(None);
            }
            if let Some(reason) = obsolete_reason(&tx, &config, &notification)? {
                discard(&tx, &notification, reason)?;
                tx.commit()?;
                return Ok(None);
            }
            if reconcile_held_change(&tx, &mut notification)? {
                tx.commit()?;
                return Ok(None);
            }
            let updated = tx.execute(
                "UPDATE notification_deliveries SET state = 'sending', attempts = attempts + 1, updated_at = ?3
                 WHERE event_id = ?1 AND recipient = ?2 AND state IN ('queued', 'retrying')",
                params![notification.event_id, notification.recipient, timestamp(now)],
            )?;
            tx.commit()?;
            notification.attempts += 1;
            Ok((updated != 0).then_some(notification))
        }).await
    }

    pub async fn finish_notification(
        &self,
        notification: Notification,
        outcome: DeliveryOutcome,
    ) -> Result<()> {
        self.finish_notification_at(notification, outcome, Utc::now())
            .await
    }

    pub async fn finish_notification_at(
        &self,
        notification: Notification,
        outcome: DeliveryOutcome,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.notification_db(move |db| {
            let tx = db.transaction()?;
            let (state, diagnostic, delay, undo_attempt) = match outcome {
                DeliveryOutcome::Delivered => ("delivered", None, 0, false),
                DeliveryOutcome::Retry(message) => ("retrying", Some(message), (3_i64 << notification.attempts.saturating_sub(1).min(7)).min(300), false),
                DeliveryOutcome::Failed(message) => ("failed", Some(message), 0, false),
                DeliveryOutcome::RecheckConfiguration => ("queued", None, 0, true),
            };
            let updated = tx.execute(
                "UPDATE notification_deliveries AS d SET
                    state = CASE WHEN ?3 IN ('queued', 'retrying') AND EXISTS (
                        SELECT 1 FROM notification_events e JOIN notification_events newer
                        ON newer.host_id = e.host_id AND newer.service_id = e.service_id
                           AND newer.reminder_for IS NULL AND newer.id > COALESCE(e.reminder_for, e.id)
                        WHERE e.id = d.event_id
                    ) THEN 'discarded' ELSE ?3 END,
                    diagnostic = ?4, retry_at = ?5, updated_at = ?6, attempts = attempts - ?7
                 WHERE event_id = ?1 AND recipient = ?2 AND state = 'sending'",
                params![notification.event_id, notification.recipient, state, diagnostic,
                    now.timestamp_millis() + delay * 1000, timestamp(now), undo_attempt],
            )?;
            if updated != 0 && notification.kind != TransitionKind::Recovery
                && matches!(state, "delivered" | "failed")
            {
                tx.execute(
                    "UPDATE notification_deliveries SET remind_from = ?3 WHERE recipient = ?2
                     AND event_id = (SELECT COALESCE(reminder_for, id) FROM notification_events WHERE id = ?1)
                     AND EXISTS (SELECT 1 FROM notification_events e
                         WHERE e.id = notification_deliveries.event_id AND NOT EXISTS (
                             SELECT 1 FROM notification_events newer WHERE newer.host_id = e.host_id
                               AND newer.service_id = e.service_id AND newer.reminder_for IS NULL AND newer.id > e.id
                         ))",
                    params![notification.event_id, notification.recipient, now.timestamp_millis()],
                )?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    /// Retain active episodes (including success evidence needed by a later
    /// recovery) and completed episodes with unfinished or recent deliveries.
    pub async fn prune_notifications(&self, now: DateTime<Utc>) -> Result<()> {
        let cutoff = timestamp(now - super::TRANSITION_RETENTION);
        self.notification_db(move |db| {
            let tx = db.transaction()?;
            // Keep the first accepted reminder if the lifecycle message failed:
            // it can be the only evidence that qualifies the recipient for recovery.
            // Otherwise, old reminders can expire even in an active episode.
            let old_reminders = {
                let mut query = tx.prepare(
                    "SELECT e.id FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                     WHERE d.updated_at < ?1 AND e.reminder_for IS NOT NULL
                       AND (d.state IN ('failed', 'discarded') OR (d.state = 'delivered' AND EXISTS (
                           SELECT 1 FROM notification_deliveries prior WHERE prior.recipient = d.recipient
                             AND prior.state = 'delivered' AND (prior.event_id = e.reminder_for
                               OR prior.event_id IN (SELECT id FROM notification_events
                                   WHERE reminder_for = e.reminder_for AND id < e.id))
                       ))) LIMIT 500",
                )?;
                query.query_map([&cutoff], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in old_reminders {
                tx.execute("DELETE FROM notification_deliveries WHERE event_id = ?1", [id])?;
                tx.execute("DELETE FROM notification_events WHERE id = ?1", [id])?;
            }
            let expired = {
                let mut query = tx.prepare(
                    "SELECT COALESCE(e.episode_id, e.id) FROM notification_events e
                     WHERE e.kind = 'recovery' AND e.occurred_at < ?1
                       AND NOT EXISTS (
                           SELECT 1 FROM notification_deliveries d
                           WHERE (d.event_id = COALESCE(e.episode_id, e.id) OR d.event_id IN (
                               SELECT id FROM notification_events WHERE episode_id = e.episode_id
                           )) AND (d.state IN ('queued', 'sending', 'retrying') OR d.updated_at >= ?1)
                       ) ORDER BY e.occurred_at, e.id LIMIT 500",
                )?;
                query.query_map([cutoff], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for episode in expired {
                tx.execute(
                    "DELETE FROM notification_deliveries WHERE event_id IN (
                        SELECT id FROM notification_events WHERE id = ?1 OR episode_id = ?1
                     )", [episode],
                )?;
                tx.execute("DELETE FROM notification_events WHERE id = ?1 OR episode_id = ?1", [episode])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn recent_deliveries(&self) -> Result<Vec<DeliveryRecord>> {
        self.notification_db(|db| {
            let mut statement = db.prepare(
                "SELECT e.id, e.host_id, e.service_id, d.recipient, e.kind, e.severity,
                        e.occurred_at, d.attempts, e.reminder_for IS NOT NULL,
                        d.state, d.diagnostic, d.updated_at,
                        CASE WHEN p.acknowledged IS NOT 1 THEN d.remind_from END
                 FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                 LEFT JOIN check_problem_episodes p ON p.host_id = e.host_id AND p.service_id = e.service_id
                 ORDER BY d.updated_at DESC, d.event_id DESC, d.recipient LIMIT 100",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(DeliveryRecord {
                    notification: read_notification(row)?,
                    state: row.get(9)?,
                    diagnostic: row.get(10)?,
                    updated_at: row.get(11)?,
                    reminder_from: row
                        .get::<_, Option<i64>>(12)?
                        .and_then(DateTime::from_timestamp_millis),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }
}

// Compare a held change with what the recipient actually saw, not the last
// suppressed transition. If nothing was delivered, introduce the current problem.
fn reconcile_held_change(tx: &Transaction<'_>, notification: &mut Notification) -> Result<bool> {
    if notification.reminder || notification.kind != TransitionKind::SeverityChanged {
        return Ok(false);
    }
    let Some(episode) = tx.query_row(
        "SELECT e.episode_id FROM notification_events e JOIN notification_deliveries d ON d.event_id = e.id
         WHERE e.id = ?1 AND d.recipient = ?2 AND d.held = 1",
        params![notification.event_id, notification.recipient],
        |row| row.get::<_, Option<i64>>(0),
    ).optional()? else {
        return Ok(false);
    };
    let known = tx
        .query_row(
            "SELECT e.severity, d.retry_at
         FROM notification_events e JOIN notification_deliveries d ON d.event_id = e.id
         WHERE (e.id = ?1 OR e.episode_id = ?1) AND d.recipient = ?2
           AND d.state = 'delivered' AND e.kind != 'recovery'
         ORDER BY e.id DESC LIMIT 1",
            params![episode, notification.recipient],
            |row| Ok((row.get::<_, i32>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((severity, delivered_at)) = known else {
        notification.kind = TransitionKind::Confirmed;
        return Ok(false);
    };
    if severity != status_code(notification.severity) {
        return Ok(false);
    }
    discard(
        tx,
        notification,
        "Current severity already delivered before suppression",
    )?;
    tx.execute(
        "UPDATE notification_deliveries SET remind_from = ?3 WHERE event_id = ?1 AND recipient = ?2",
        params![notification.event_id, notification.recipient, delivered_at],
    )?;
    Ok(true)
}

fn queue_due_reminder(
    tx: &Transaction<'_>,
    config: &Config,
    now: DateTime<Utc>,
    reachability_hosts: &str,
    closed_targets: Option<&str>,
) -> Result<()> {
    let due = tx
        .query_row(
            "SELECT d.event_id, d.recipient
         FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
         WHERE d.remind_from <= ?1 AND (e.service_id = '' OR e.host_id NOT IN (
             SELECT host_id FROM check_problems WHERE check_kind = 'host'
               AND service_id = '' AND confirmed_severity = 2
               AND host_id IN (SELECT value FROM json_each(?2))
         )) AND (e.host_id, e.service_id) NOT IN (
             SELECT value ->> 0, value ->> 1 FROM json_each(?3)
         ) AND NOT EXISTS (
             SELECT 1 FROM check_problem_episodes p
             WHERE p.host_id = e.host_id AND p.service_id = e.service_id AND p.acknowledged = 1
         ) ORDER BY d.remind_from, d.event_id, d.recipient LIMIT 1",
            params![
                now.timestamp_millis() - i64::from(config.reminder_interval_seconds) * 1000,
                reachability_hosts,
                closed_targets
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((event, recipient)) = due else {
        return Ok(());
    };
    tx.execute(
        "UPDATE notification_deliveries SET remind_from = NULL WHERE event_id = ?1 AND recipient = ?2",
        params![event, recipient],
    )?;
    let at = timestamp(now);
    tx.execute(
        "INSERT INTO notification_events (host_id, service_id, episode_id, kind, severity, occurred_at, reminder_for)
         SELECT host_id, service_id, COALESCE(episode_id, id), kind, severity, ?2, id
         FROM notification_events WHERE id = ?1",
        params![event, at],
    )?;
    tx.execute(
        "INSERT INTO notification_deliveries (event_id, recipient, state, updated_at, retry_at)
         VALUES (?1, ?2, 'queued', ?3, ?4)",
        params![
            tx.last_insert_rowid(),
            recipient,
            at,
            now.timestamp_millis()
        ],
    )?;
    Ok(())
}

fn obsolete_reason(
    db: &Connection,
    config: &Config,
    notification: &Notification,
) -> Result<Option<&'static str>> {
    let Some((episode, current, acknowledged)) = db
        .query_row(
            "SELECT e.episode_id,
                NOT EXISTS(SELECT 1 FROM notification_events newer
                    WHERE newer.reminder_for IS NULL AND newer.id > COALESCE(e.reminder_for, e.id)
                    AND newer.host_id = e.host_id AND newer.service_id = e.service_id)
                AND EXISTS(SELECT 1 FROM check_problems p
                    WHERE p.check_kind = CASE WHEN e.service_id = '' THEN 'host' ELSE 'service' END
                    AND p.host_id = e.host_id AND p.service_id = e.service_id
                    AND p.confirmed_severity IS NULLIF(e.severity, 0)),
                EXISTS(SELECT 1 FROM check_problem_episodes p
                    WHERE p.host_id = e.host_id AND p.service_id = e.service_id AND p.acknowledged = 1)
         FROM notification_events e WHERE e.id = ?1",
            [notification.event_id],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, bool>(1)?, row.get::<_, bool>(2)?)),
        )
        .optional()?
    else {
        return Ok(Some("Notification history expired"));
    };
    if !current {
        return Ok(Some("Superseded by a newer confirmed state"));
    }
    if notification.reminder && acknowledged {
        return Ok(Some("Problem acknowledged"));
    }
    if !config.hosts.iter().any(|host| {
        host.id == notification.host_id
            && host.recipients.contains(&notification.recipient)
            && if notification.service_id.is_empty() {
                host.reachability.is_some()
            } else {
                host.services
                    .iter()
                    .any(|service| service.id == notification.service_id)
            }
    }) {
        return Ok(Some(if notification.service_id.is_empty() {
            "Reachability check or recipient no longer configured"
        } else {
            "Service or recipient no longer configured"
        }));
    }
    if notification.kind == TransitionKind::Recovery
        && !has_problem_delivery(db, episode, &notification.recipient, false)?
    {
        return Ok(Some(
            "No problem notification was delivered to this recipient",
        ));
    }
    Ok(None)
}

fn discard(db: &Connection, notification: &Notification, reason: &str) -> Result<()> {
    db.execute(
        "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?3, diagnostic = ?4
         WHERE event_id = ?1 AND recipient = ?2 AND state IN ('queued', 'retrying')",
        params![
            notification.event_id,
            notification.recipient,
            timestamp(Utc::now()),
            reason
        ],
    )?;
    Ok(())
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
        reminder: row.get(8)?,
    })
}
