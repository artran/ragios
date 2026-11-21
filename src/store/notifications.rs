//! Notification eligibility and durable hold reconciliation live here.
//!
//! `Eligibility` owns recording, selection, reminder, and dispatch policy; the
//! phases intentionally observe different facts. `record` and the hold helpers
//! participate in their callers' SQLite transactions. Store methods own the
//! selection/dispatch transactions; SMTP runs only after dispatch commits.
//! See `docs/notification-policy.md` for the phase distinctions and regressions.

use super::{Store, TransitionKind, silences, status_code};
use crate::{
    config::Config,
    runner::{Reachability, Status},
};
use anyhow::{Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, named_params, params};
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
fn hold_services(tx: &Transaction<'_>, host: &str) -> Result<()> {
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

/// Owns phase-specific eligibility. Recording remembers suppression history;
/// selection skips temporarily suppressed targets; dispatch revalidates current
/// facts and reconciles held changes against each recipient's accepted mail.
struct Eligibility<'a> {
    config: &'a Config,
    now: DateTime<Utc>,
}

// SQL uses these configuration facts as values, never as JSON path syntax.
struct SelectionFacts {
    reachability_hosts: String,
    groups: String,
    closed_targets: Option<String>,
}

// Both queued selection and reminder selection skip temporary suppression.
// Acknowledgement is deliberately absent: it applies only to reminders.
fn selection_filter() -> String {
    format!(
        "(e.service_id = '' OR e.host_id NOT IN (
        SELECT host_id FROM check_problems WHERE check_kind = 'host'
          AND service_id = '' AND confirmed_severity = 2
          AND host_id IN (SELECT value FROM json_each(:reachability))
    )) AND {NOT_SILENCED} AND (e.host_id, e.service_id) NOT IN (
        SELECT value ->> 0, value ->> 1 FROM json_each(:closed_targets)
    )"
    )
}

const NOT_SILENCED: &str = "NOT EXISTS (
    SELECT 1 FROM silences s WHERE s.expires_at > :now AND (
        (s.host_id = e.host_id AND (s.service_id IS NULL OR s.service_id = e.service_id))
        OR s.group_id = (SELECT value FROM json_each(:groups) WHERE key = e.host_id)
    )
)";

fn host_groups(config: &Config) -> Result<String> {
    Ok(serde_json::to_string(
        &config
            .hosts
            .iter()
            .map(|host| (&host.id, &host.group))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )?)
}

// Called under the caller's lock/transaction, before a Silence or new group
// membership becomes visible. Include sending rows: SMTP may still be in flight.
pub(super) fn hold_silenced(db: &Connection, config: &Config, now: DateTime<Utc>) -> Result<()> {
    let groups = host_groups(config)?;
    db.execute(
        &format!(
            "UPDATE notification_deliveries AS d SET held = 1
         WHERE state IN ('queued', 'retrying', 'sending') AND held = 0
           AND EXISTS (SELECT 1 FROM notification_events e WHERE e.id = d.event_id
             AND e.kind = 'severity_changed' AND NOT {NOT_SILENCED})"
        ),
        named_params! {":now": now.timestamp_millis(), ":groups": groups},
    )?;
    Ok(())
}

impl<'a> Eligibility<'a> {
    fn new(config: &'a Config, now: DateTime<Utc>) -> Self {
        Self { config, now }
    }

    fn host(&self, host_id: &str) -> Option<&crate::config::Host> {
        self.config.hosts.iter().find(|host| host.id == host_id)
    }

    fn window_closed(&self, host: &str, service: &str, at: DateTime<Utc>) -> bool {
        self.host(host)
            .and_then(|host| host.notification_window(service))
            .is_some_and(|window| !window.is_open(at))
    }

    fn silenced(&self, db: &Connection, host: &str, service: &str) -> Result<bool> {
        silences::is_silenced(
            db,
            (
                host,
                service,
                self.host(host).map(|host| host.group.as_str()),
            ),
            self.now,
        )
    }

    fn select(&self, tx: &Transaction<'_>) -> Result<Option<Notification>> {
        if self.config.smtp.is_none() {
            tx.execute(
                "UPDATE notification_deliveries SET state = 'discarded', updated_at = ?1,
                   diagnostic = 'Email disabled' WHERE state IN ('queued', 'retrying')",
                [timestamp(self.now)],
            )?;
            tx.execute("UPDATE notification_deliveries SET remind_from = NULL WHERE remind_from IS NOT NULL", [])?;
            return Ok(None);
        }
        let closed_targets = self
            .config
            .hosts
            .iter()
            .flat_map(|host| {
                host.notification_windows()
                    .filter(|(_, window)| !window.is_open(self.now))
                    .map(|(id, _)| (&host.id, id))
            })
            .collect::<Vec<_>>();
        let facts = SelectionFacts {
            reachability_hosts: serde_json::to_string(
                &self
                    .config
                    .hosts
                    .iter()
                    .filter(|host| host.reachability.is_some())
                    .map(|host| &host.id)
                    .collect::<Vec<_>>(),
            )?,
            groups: host_groups(self.config)?,
            closed_targets: if closed_targets.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&closed_targets)?)
            },
        };
        // A retry may have been queued while open. Remember its closed-window
        // history before selection, even if no message is ready to dispatch.
        if let Some(closed_targets) = &facts.closed_targets {
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
        // At most one reminder per poll; no accumulated suppression backlog.
        self.queue_due_reminder(tx, &facts)?;
        // Bound reconciliation so check writes and reload can get the lock.
        for _ in 0..32 {
            let candidate = tx.query_row(
                &format!("SELECT e.id, e.host_id, e.service_id, d.recipient, e.kind, e.severity,
                    e.occurred_at, d.attempts, e.reminder_for IS NOT NULL
                 FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
                 WHERE d.state IN ('queued', 'retrying') AND d.retry_at <= :now AND {}
                 ORDER BY d.retry_at, d.event_id, d.recipient LIMIT 1", selection_filter()),
                named_params! {":now": self.now.timestamp_millis(), ":reachability": facts.reachability_hosts,
                    ":groups": facts.groups, ":closed_targets": facts.closed_targets},
                read_notification,
            ).optional()?;
            let Some(candidate) = candidate else {
                return Ok(None);
            };
            if let Some(reason) = self.obsolete_reason(tx, &candidate)? {
                discard(tx, &candidate, reason)?;
                continue;
            }
            return Ok(Some(candidate));
        }
        Ok(None)
    }

    fn prepare_dispatch(
        &self,
        tx: &Transaction<'_>,
        notification: &mut Notification,
    ) -> Result<bool> {
        let host = self.host(&notification.host_id);
        if self.window_closed(&notification.host_id, &notification.service_id, self.now) {
            tx.execute(
                "UPDATE notification_deliveries SET held = 1 WHERE event_id = ?1 AND recipient = ?2
                 AND state IN ('queued', 'retrying') AND held = 0",
                params![notification.event_id, notification.recipient],
            )?;
            return Ok(false);
        }
        if !notification.service_id.is_empty()
            && host.is_some_and(|host| host.reachability.is_some())
            && host_is_down(tx, &notification.host_id)?
        {
            return Ok(false);
        }
        if let Some(reason) = self.obsolete_reason(tx, notification)? {
            discard(tx, notification, reason)?;
            return Ok(false);
        }
        if self.silenced(tx, &notification.host_id, &notification.service_id)? {
            return Ok(false);
        }
        Ok(!reconcile_held_change(tx, notification)?)
    }

    fn held_at_recording(
        &self,
        db: &Connection,
        (host, service): (&str, &str),
        kind: TransitionKind,
        occurred_at: &str,
    ) -> Result<bool> {
        let observed_at = DateTime::parse_from_rfc3339(occurred_at)
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or(self.now);
        Ok(self.window_closed(host, service, observed_at)
            || (!service.is_empty() && host_is_down(db, host)?)
            || (kind == TransitionKind::SeverityChanged && self.silenced(db, host, service)?))
    }
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
    config: Option<&Config>,
) -> Result<()> {
    let (host_id, service_id) = check;
    // Even result-only writes must preserve host-down suppression history.
    if service_id.is_empty()
        && severity == Status::Critical
        && previous_severity != Some(Status::Critical)
        && matches!(
            kind,
            TransitionKind::Confirmed | TransitionKind::SeverityChanged
        )
    {
        hold_services(tx, host_id)?;
    }
    if !matches!(
        kind,
        TransitionKind::Confirmed | TransitionKind::SeverityChanged | TransitionKind::Recovery
    ) || (kind == TransitionKind::Recovery && previous_severity.is_none())
    {
        return Ok(());
    }
    let now = Utc::now();
    let eligibility = config.map(|config| Eligibility::new(config, now));
    let recipients = eligibility
        .as_ref()
        .filter(|policy| policy.config.smtp.is_some())
        .and_then(|policy| policy.host(host_id))
        .map(|host| host.recipients.as_slice())
        .unwrap_or_default();
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
    if recipients.is_empty() {
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
    let held = match &eligibility {
        Some(policy) => policy.held_at_recording(tx, check, kind, occurred_at)?,
        None => !service_id.is_empty() && host_is_down(tx, host_id)?,
    };
    for recipient in recipients {
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
    /// Mark holds before exposing new group membership. Serialize with silence
    /// creation so neither operation can miss the other's configuration.
    pub async fn publish_notification_config(
        &self,
        config: Arc<Config>,
        published: watch::Sender<Arc<Config>>,
    ) -> Result<()> {
        self.with_connection(move |db| {
            hold_silenced(db, &config, Utc::now())?;
            published.send_replace(config);
            Ok(())
        })
        .await
    }

    // The worker also marks holds when it starts, including after a restart.
    pub async fn hold_silenced_deliveries(&self, config: Arc<Config>) -> Result<()> {
        self.with_connection(move |db| hold_silenced(db, &config, Utc::now()))
            .await
    }

    /// Only the single delivery worker calls this at startup. An interrupted
    /// SMTP attempt has no known outcome until the remote server confirms it.
    pub async fn resume_notifications(&self) -> Result<()> {
        ensure!(
            self.1.is_some(),
            "email delivery requires the exclusive server database lock"
        );
        self.with_connection(|db| {
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
        self.with_connection(move |db| {
            let tx = db.transaction()?;
            // Keep the configuration stable until reconciliation commits.
            let config = active.borrow();
            let candidate = Eligibility::new(&config, now).select(&tx)?;
            tx.commit()?;
            Ok(candidate)
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
        self.with_connection(move |db| {
            let tx = db.transaction()?;
            let config = active.borrow();
            if !Arc::ptr_eq(&config, &relay_config) || config.smtp.is_none() {
                return Ok(None);
            }
            let now = clock();
            if !Eligibility::new(&config, now).prepare_dispatch(&tx, &mut notification)? {
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
        self.with_connection(move |db| {
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
        self.with_connection(move |db| {
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
        self.with_connection(|db| {
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

impl Eligibility<'_> {
    fn queue_due_reminder(&self, tx: &Transaction<'_>, facts: &SelectionFacts) -> Result<()> {
        let now = self.now;
        let due = tx
        .query_row(
            &format!(
                "SELECT d.event_id, d.recipient
         FROM notification_deliveries d JOIN notification_events e ON e.id = d.event_id
         WHERE d.remind_from <= :due AND {} AND NOT EXISTS (
             SELECT 1 FROM check_problem_episodes p
             WHERE p.host_id = e.host_id AND p.service_id = e.service_id AND p.acknowledged = 1
         ) ORDER BY d.remind_from, d.event_id, d.recipient LIMIT 1", selection_filter()
            ),
            named_params! {
                ":due": now.timestamp_millis() - i64::from(self.config.reminder_interval_seconds) * 1000,
                ":now": now.timestamp_millis(),
                ":reachability": facts.reachability_hosts,
                ":groups": facts.groups,
                ":closed_targets": facts.closed_targets,
            },
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
        &self,
        db: &Connection,
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
        if !self.config.hosts.iter().any(|host| {
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
