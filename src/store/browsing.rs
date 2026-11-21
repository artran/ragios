use super::{
    CHECK_STATE_SELECT, CheckKind, LatestHosts, LatestResults, Store, StoredCheck, Transition,
    notifications::{DeliveryRecord, read_recent_deliveries},
    read_transitions,
};
use crate::{config::Config, runner::Status};
use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub enum CheckPath {
    Service(String, String),
    Reachability(String),
}

pub struct CheckSnapshot {
    pub config: Arc<Config>,
    pub services: LatestResults,
    pub hosts: LatestHosts,
    pub transitions: Vec<Transition>,
}

pub struct NotificationSnapshot {
    /// Absent when SMTP is disabled.
    pub reminder_interval_seconds: Option<u32>,
    pub deliveries: Vec<DeliveryRecord>,
    pub suppressions: Vec<NotificationSuppression>,
}

pub enum NotificationSuppression {
    HostDown {
        host_id: String,
    },
    OutsideWindow {
        host_id: String,
        service_id: Option<String>,
        window_id: String,
    },
}

impl Store {
    pub async fn browse_checks(
        &self,
        active: watch::Receiver<Arc<Config>>,
        history: Option<CheckPath>,
    ) -> Result<CheckSnapshot> {
        self.browse(active, move |db, config| {
            read_checks(db, config, history.as_ref())
        })
        .await
    }

    pub async fn browse_notifications(
        &self,
        active: watch::Receiver<Arc<Config>>,
    ) -> Result<NotificationSnapshot> {
        self.browse(active, |db, config| read_notifications(db, &config))
            .await
    }

    async fn browse<T: Send + 'static>(
        &self,
        mut active: watch::Receiver<Arc<Config>>,
        read: impl Fn(&Connection, Arc<Config>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.with_connection(move |db| {
            loop {
                // Publication through this Store takes the same connection lock.
                // Check the watch version as well, without holding a watch borrow
                // during SQLite work, through the end of the entire transaction.
                let config = Arc::clone(&active.borrow_and_update());
                let tx = db.transaction()?;
                let facts = read(&tx, config)?;
                tx.commit()?;
                if !active.borrow_and_update().has_changed() {
                    return Ok(facts);
                }
            }
        })
        .await
    }
}

fn read_checks(
    db: &Connection,
    config: Arc<Config>,
    history: Option<&CheckPath>,
) -> Result<CheckSnapshot> {
    let mut current = db.prepare(&format!(
        "{CHECK_STATE_SELECT} WHERE r.check_kind = ?1 AND r.host_id = ?2 AND r.service_id = ?3"
    ))?;
    let mut services = LatestResults::new();
    let mut hosts = LatestHosts::new();
    // Even detail pages show a global overdue banner. Read current facts for
    // configured checks only, and history for at most the selected check.
    for host in &config.hosts {
        if host.reachability.is_some()
            && let Some(saved) = current
                .query_row(
                    params![CheckKind::Host.as_str(), host.id, ""],
                    StoredCheck::read,
                )
                .optional()?
        {
            let (id, state) = saved.into_host()?;
            hosts.insert(id, state);
        }
        for service in &host.services {
            if let Some(saved) = current
                .query_row(
                    params![CheckKind::Service.as_str(), host.id, service.id],
                    StoredCheck::read,
                )
                .optional()?
            {
                services.insert((saved.host_id, saved.service_id), saved.state);
            }
        }
    }
    let transitions = match history {
        Some(CheckPath::Service(host, service))
            if config
                .hosts
                .iter()
                .any(|h| h.id == *host && h.services.iter().any(|s| s.id == *service)) =>
        {
            read_transitions(db, CheckKind::Service, host, service)?
        }
        Some(CheckPath::Reachability(host))
            if config
                .hosts
                .iter()
                .any(|h| h.id == *host && h.reachability.is_some()) =>
        {
            read_transitions(db, CheckKind::Host, host, "")?
        }
        _ => Vec::new(),
    };
    Ok(CheckSnapshot {
        config,
        services,
        hosts,
        transitions,
    })
}

fn read_notifications(db: &Connection, config: &Config) -> Result<NotificationSnapshot> {
    let deliveries = read_recent_deliveries(db)?;
    let reminder_interval_seconds = config
        .smtp
        .as_ref()
        .map(|_| config.reminder_interval_seconds);
    let mut suppressions = Vec::new();
    if reminder_interval_seconds.is_some() {
        let mut problem = db.prepare(
            "SELECT confirmed_severity FROM check_problems
             WHERE check_kind = ?1 AND host_id = ?2 AND service_id = ?3",
        )?;
        let mut confirmed =
            |kind: CheckKind, host: &str, service: &str| -> Result<Option<Status>> {
                Ok(problem
                    .query_row(params![kind.as_str(), host, service], |row| {
                        row.get::<_, Option<i32>>(0)
                    })
                    .optional()?
                    .flatten()
                    .map(|code| Status::from_code(Some(code))))
            };
        let now = Utc::now();
        for host in &config.hosts {
            let reachability = if host.reachability.is_some() {
                confirmed(CheckKind::Host, &host.id, "")?
            } else {
                None
            };
            for (id, window) in host.notification_windows() {
                if window.is_open(now) {
                    continue;
                }
                let severity = if id.is_empty() {
                    reachability
                } else {
                    confirmed(CheckKind::Service, &host.id, id)?
                };
                if severity.is_some() {
                    suppressions.push(NotificationSuppression::OutsideWindow {
                        host_id: host.id.clone(),
                        service_id: (!id.is_empty()).then(|| id.to_owned()),
                        window_id: window.id.clone(),
                    });
                }
            }
            if reachability == Some(Status::Critical) {
                suppressions.push(NotificationSuppression::HostDown {
                    host_id: host.id.clone(),
                });
            }
        }
    }
    Ok(NotificationSnapshot {
        reminder_interval_seconds,
        deliveries,
        suppressions,
    })
}

#[cfg(test)]
mod tests;
