use super::{
    CHECK_STATE_SELECT, CheckKind, CheckState, LatestHosts, LatestResults, Store, StoredCheck,
    Transition, is_confirmed_down,
    notifications::{DeliveryRecord, read_recent_deliveries},
    read_transitions,
};
use crate::{
    config::{Config, Host, Probe, ReachabilityCheck, Service},
    runner::{CheckResult, Status},
    schedule::{Cadence, SavedCheckTiming},
};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Statement, params};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::watch;

pub enum CheckId {
    Service { host_id: String, service_id: String },
    Reachability { host_id: String },
}

pub struct CheckSnapshot {
    pub config: Arc<Config>,
    pub services: LatestResults,
    pub hosts: LatestHosts,
    pub timings: HashMap<(String, String), SavedCheckTiming>,
}

pub struct CheckDetailSnapshot {
    checks: CheckSnapshot,
    pub state: Option<CheckState>,
    pub observed_probe: Option<Probe>,
    pub transitions: Vec<Transition>,
    host_index: usize,
    service_index: Option<usize>,
}

pub enum CheckDefinition<'a> {
    Service(&'a Service),
    Reachability(&'a ReachabilityCheck),
}

impl CheckDetailSnapshot {
    pub fn checks(&self) -> &CheckSnapshot {
        &self.checks
    }

    pub fn definition(&self) -> (&Host, CheckDefinition<'_>) {
        let host = &self.checks.config.hosts[self.host_index];
        let definition = match self.service_index {
            Some(index) => CheckDefinition::Service(&host.services[index]),
            None => CheckDefinition::Reachability(
                host.reachability.as_ref().expect("resolved reachability"),
            ),
        };
        (host, definition)
    }
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
    /// Latest rows for the overview or one Host, plus timing for the fleet banner.
    pub async fn browse_checks(
        &self,
        active: watch::Receiver<Arc<Config>>,
        host_id: Option<String>,
    ) -> Result<CheckSnapshot> {
        self.browse(active, move |db, config| {
            read_checks(db, config, |host| {
                host_id.as_deref().is_none_or(|id| host.id == id)
            })
        })
        .await
    }

    /// Current state and retained history for one configured Check.
    pub async fn browse_check(
        &self,
        active: watch::Receiver<Arc<Config>>,
        id: CheckId,
    ) -> Result<Option<CheckDetailSnapshot>> {
        self.browse(active, move |db, config| read_detail(db, config, &id))
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

fn read_detail(
    db: &Connection,
    config: Arc<Config>,
    id: &CheckId,
) -> Result<Option<CheckDetailSnapshot>> {
    let (host_id, service_id, kind) = match id {
        CheckId::Service {
            host_id,
            service_id,
        } => (host_id, service_id.as_str(), CheckKind::Service),
        CheckId::Reachability { host_id } => (host_id, "", CheckKind::Host),
    };
    let Some(host_index) = config.hosts.iter().position(|host| host.id == *host_id) else {
        return Ok(None);
    };
    let host = &config.hosts[host_index];
    let service_index = match kind {
        CheckKind::Service => {
            let Some(index) = host
                .services
                .iter()
                .position(|service| service.id == service_id)
            else {
                return Ok(None);
            };
            Some(index)
        }
        CheckKind::Host if host.reachability.is_some() => None,
        CheckKind::Host => return Ok(None),
    };
    let current = prepare_current(db)?
        .query_row(
            params![kind.as_str(), host_id, service_id],
            StoredCheck::read,
        )
        .optional()?;
    let observed_probe = match (&current, kind) {
        (Some(saved), CheckKind::Host) => {
            Some(saved.probe.context("host result missing probe origin")?)
        }
        _ => None,
    };
    let state = current.map(|saved| saved.state);
    let checks = read_checks(db, config, |_| false)?;
    let transitions = read_transitions(db, kind, host_id, service_id)?;
    Ok(Some(CheckDetailSnapshot {
        checks,
        state,
        observed_probe,
        transitions,
        host_index,
        service_index,
    }))
}

fn prepare_current(db: &Connection) -> rusqlite::Result<Statement<'_>> {
    db.prepare(&format!(
        "{CHECK_STATE_SELECT} WHERE r.check_kind = ?1 AND r.host_id = ?2 AND r.service_id = ?3"
    ))
}

fn read_checks(
    db: &Connection,
    config: Arc<Config>,
    display: impl Fn(&Host) -> bool,
) -> Result<CheckSnapshot> {
    let mut current = config
        .hosts
        .iter()
        .any(&display)
        .then(|| prepare_current(db))
        .transpose()?;
    let mut due = db.prepare(
        "SELECT r.finished_at, p.pending_severity IS NOT NULL
         FROM check_results r JOIN check_problems p USING (check_kind, host_id, service_id)
         WHERE r.check_kind = ?1 AND r.host_id = ?2 AND r.service_id = ?3",
    )?;
    let mut snapshot = CheckSnapshot {
        config,
        services: LatestResults::new(),
        hosts: LatestHosts::new(),
        timings: HashMap::new(),
    };
    // Every check page shows a fleet-wide overdue banner. Only rows rendered
    // on this page need diagnostics, probe origin, or acknowledgement data.
    for host in &snapshot.config.hosts {
        let targets = host
            .reachability
            .iter()
            .map(|_| (CheckKind::Host, ""))
            .chain(
                host.services
                    .iter()
                    .map(|service| (CheckKind::Service, service.id.as_str())),
            );
        for (kind, service_id) in targets {
            let key = (host.id.clone(), service_id.to_owned());
            let args = params![kind.as_str(), host.id, service_id];
            if let Some(current) = current.as_mut().filter(|_| display(host)) {
                if let Some(saved) = current.query_row(args, StoredCheck::read).optional()? {
                    snapshot.timings.insert(
                        key,
                        SavedCheckTiming::from_result(&saved.state.result, saved.state.problem),
                    );
                    match kind {
                        CheckKind::Host => {
                            let (id, state) = saved.into_host()?;
                            snapshot.hosts.insert(id, state);
                        }
                        CheckKind::Service => {
                            snapshot
                                .services
                                .insert((saved.host_id, saved.service_id), saved.state);
                        }
                    }
                }
            } else if let Some(timing) = due
                .query_row(args, |row| {
                    let finished: String = row.get(0)?;
                    Ok(SavedCheckTiming {
                        finished: CheckResult::parse_finished_at(&finished),
                        cadence: if row.get(1)? {
                            Cadence::Retry
                        } else {
                            Cadence::Normal
                        },
                    })
                })
                .optional()?
            {
                snapshot.timings.insert(key, timing);
            }
        }
    }
    Ok(snapshot)
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
            if is_confirmed_down(reachability) {
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
