mod silences;

use crate::monitor::{CheckRequest, CheckRequestOutcome, Key, SchedulerStatus};
use axum::{
    Form, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use ragios::{
    config::{Check, Config, Host, HostGroup, Probe, Service},
    runner::{CheckResult, Reachability, Status},
    schedule::{self, Freshness},
    store::{
        AcknowledgeOutcome, CheckDefinition, CheckDetailSnapshot, CheckId, CheckSnapshot,
        NotificationSnapshot, NotificationSuppression, ProblemEpisode, ProblemState, Store,
        TRANSITION_RETENTION, Transition, TransitionKind,
    },
};
use serde::Deserialize;
use std::{fmt::Write, net::SocketAddr, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
struct Dashboard {
    config: watch::Receiver<Arc<Config>>,
    scheduler: watch::Receiver<SchedulerStatus>,
    store: Store,
    requests: mpsc::Sender<CheckRequest>,
    csrf: Arc<str>,
}

type WebError = (StatusCode, &'static str);
type WebResult = Result<Html<String>, WebError>;

pub fn router(
    config: watch::Receiver<Arc<Config>>,
    scheduler: watch::Receiver<SchedulerStatus>,
    store: Store,
    requests: mpsc::Sender<CheckRequest>,
    listener: SocketAddr,
) -> anyhow::Result<Router> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate dashboard CSRF token: {error}"))?;
    let mut csrf = String::with_capacity(random.len() * 2);
    for byte in random {
        let _ = write!(csrf, "{byte:02x}");
    }
    Ok(Router::new()
        .route("/", get(page))
        .route("/status", get(status))
        .route("/notifications", get(notifications_page))
        .route("/notifications/status", get(notifications_status))
        .route("/silences", get(silences::page).post(silences::create))
        .route("/silences/status", get(silences::status))
        .route("/hosts/{host}", get(host_page))
        .route("/hosts/{host}/status", get(host_status))
        .route("/hosts/{host}/reachability", get(reachability_page))
        .route(
            "/hosts/{host}/reachability/status",
            get(reachability_status),
        )
        .route("/hosts/{host}/services/{service}", get(service_page))
        .route(
            "/hosts/{host}/services/{service}/status",
            get(service_status),
        )
        .route(
            "/hosts/{host}/reachability/check-now",
            post(reachability_check_now),
        )
        .route(
            "/hosts/{host}/services/{service}/check-now",
            post(service_check_now),
        )
        .route(
            "/hosts/{host}/reachability/acknowledge",
            post(reachability_acknowledge),
        )
        .route(
            "/hosts/{host}/services/{service}/acknowledge",
            post(service_acknowledge),
        )
        .route(
            "/assets/htmx.min.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/htmx.min.js"),
                )
            }),
        )
        .layer(middleware::from_fn_with_state(
            listener,
            require_served_host,
        ))
        .with_state(Dashboard {
            config,
            scheduler,
            store,
            requests,
            csrf: csrf.into(),
        }))
}

async fn require_served_host(
    State(listener): State<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    // Trust the actual bound socket, not proxy headers or DNS. Check both
    // authority sources when present; one must not mask an unserved host.
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    let authority = request.uri().authority();
    if (host.is_none() && authority.is_none())
        || hosts.next().is_some()
        || host.is_some_and(|host| !host.to_str().is_ok_and(|host| serves(listener, host)))
        || authority.is_some_and(|authority| !serves(listener, authority.as_str()))
    {
        return (
            StatusCode::FORBIDDEN,
            "Host is not served by this dashboard.",
        )
            .into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}

fn serves(listener: SocketAddr, host: &str) -> bool {
    if host.parse::<SocketAddr>() == Ok(listener) {
        return true;
    }
    let port = listener.port();
    if listener.ip().is_loopback()
        && (host.eq_ignore_ascii_case(&format!("localhost:{port}"))
            || (port == 80 && host.eq_ignore_ascii_case("localhost")))
    {
        return true;
    }
    // A browser omits the default port from the HTTP authority.
    port == 80 && format!("{host}:80").parse::<SocketAddr>() == Ok(listener)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Filters {
    group: String,
    problem: ProblemFilter,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ProblemFilter {
    #[default]
    All,
    Any,
    Confirmed,
    Pending,
}

impl ProblemFilter {
    const CHOICES: [(Self, &str, &str); 4] = [
        (Self::All, "all", "All checks"),
        (Self::Any, "any", "Any problem"),
        (Self::Confirmed, "confirmed", "Confirmed problems"),
        (
            Self::Pending,
            "pending",
            "Pending problems and severity changes",
        ),
    ];

    /// Not-yet-checked checks have no problem, so only `All` includes them.
    fn matches(self, problem: Option<ProblemState>) -> bool {
        let problem = problem.unwrap_or_default();
        match self {
            Self::All => true,
            Self::Any => problem.confirmed_severity.is_some() || problem.pending_severity.is_some(),
            Self::Confirmed => problem.confirmed_severity.is_some(),
            Self::Pending => problem.pending_severity.is_some(),
        }
    }
}

impl Filters {
    fn is_default(&self) -> bool {
        self.group.is_empty() && self.problem == ProblemFilter::All
    }

    fn query(&self) -> String {
        let problem = ProblemFilter::CHOICES
            .iter()
            .find(|(choice, ..)| *choice == self.problem)
            .map_or("all", |(_, value, _)| value);
        format!("group={}&problem={problem}", encode(&self.group))
    }
}

fn layout(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title><script src="/assets/htmx.min.js" defer></script>
<style>
body{{font:1rem system-ui,sans-serif;margin:2rem;background:#f5f7fa;color:#152336}}
main{{max-width:90rem;margin:auto}}table{{width:100%;border-collapse:collapse;background:white}}
th,td{{padding:.7rem;text-align:left;border-bottom:1px solid #d8dee8;vertical-align:top}}
pre{{white-space:pre-wrap;overflow-wrap:anywhere;margin:0;max-width:40rem}}
.OK{{color:#146138}}.WARNING{{color:#805000}}.CRITICAL{{color:#b02020}}.UNKNOWN{{color:#654398}}
small{{color:#45556b}}.scroll{{overflow-x:auto}}a{{color:#1b4f9c}}
form{{display:flex;flex-wrap:wrap;gap:1rem;align-items:end;margin:1rem 0}}
label{{display:grid;gap:.25rem}}select,button{{font:inherit;padding:.3rem}}
.empty{{background:white;padding:1rem;border:1px solid #d8dee8}}
.facts{{max-width:60rem}}.facts th{{width:16rem}}nav{{margin-bottom:1rem}}
.stale{{color:#45556b;background:#eceff4;font-style:italic}}
.alert{{background:#fff1d6;border:2px solid #b06a00;padding:1rem;font-weight:bold}}
</style></head><body><main>{body}</main></body></html>"#,
        title = escape(title)
    ))
}

async fn page(State(dashboard): State<Dashboard>, Query(filters): Query<Filters>) -> WebResult {
    let checks = load_checks(&dashboard, None).await?;
    let fragment = overview(&Snapshot::new(&dashboard, &checks), &filters);
    let mut form = String::from(
        r##"<form id="filters" action="/" method="get" hx-get="/status" hx-target="#status" hx-trigger="change, submit" hx-sync="this:replace"><label>Host group <select name="group"><option value="">All groups</option>"##,
    );
    for group in &checks.config.groups {
        let _ = write!(
            form,
            r#"<option value="{}"{}>{}</option>"#,
            escape(&group.id),
            selected(group.id == filters.group),
            escape(&group.name)
        );
    }
    form.push_str(r#"</select></label><label>Problem status <select name="problem">"#);
    for (choice, value, label) in ProblemFilter::CHOICES {
        let _ = write!(
            form,
            r#"<option value="{value}"{}>{label}</option>"#,
            selected(choice == filters.problem)
        );
    }
    form.push_str(
        r#"</select></label><button>Filter</button><a href="/">Clear filters</a></form>"#,
    );
    Ok(layout(
        "Ragios — host reachability and service health",
        &format!(
            r##"<h1>Ragios</h1><p>Host reachability and service health · <a href="/notifications">Email delivery</a> · <a href="/silences">Silences</a></p>
<p>Latest results, confirmed problems, and pending changes are shown separately.
Reachability is assessed from this monitoring server, independently of service health.
A failed probe does not prove the machine is powered off. Service checks continue regardless of host reachability.</p>
{form}
<div id="status" hx-get="/status" hx-include="#filters" hx-sync="#filters:replace" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>
<noscript>Reload this page to refresh results.</noscript>"##
        ),
    ))
}

fn selected(selected: bool) -> &'static str {
    if selected { " selected" } else { "" }
}

async fn status(
    State(dashboard): State<Dashboard>,
    Query(filters): Query<Filters>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let checks = load_checks(&dashboard, None).await?;
    let fragment = overview(&Snapshot::new(&dashboard, &checks), &filters);
    let mut response = Html(fragment).into_response();
    // A filter change keeps the address bar shareable; polling leaves it alone.
    if headers.get("hx-trigger").is_some_and(|id| id == "filters")
        && let Ok(url) = HeaderValue::from_str(&format!("/?{}", filters.query()))
    {
        response.headers_mut().insert("hx-replace-url", url);
    }
    Ok(response)
}

async fn notifications_page(State(dashboard): State<Dashboard>) -> WebResult {
    let fragment = notification_deliveries(&dashboard).await?;
    Ok(layout(
        "Ragios — Email delivery",
        &format!(
            r#"<nav><a href="/">Overview</a> · <a href="/silences">Silences</a></nav><h1>Email delivery</h1>
<div hx-get="/notifications/status" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>"#,
        ),
    ))
}

async fn notifications_status(State(dashboard): State<Dashboard>) -> WebResult {
    Ok(Html(notification_deliveries(&dashboard).await?))
}

async fn notification_deliveries(dashboard: &Dashboard) -> Result<String, WebError> {
    let snapshot = dashboard
        .store
        .browse_notifications(dashboard.config.clone())
        .await
        .map_err(load_error)?;
    Ok(render_deliveries(snapshot))
}

fn render_deliveries(snapshot: NotificationSnapshot) -> String {
    let NotificationSnapshot {
        reminder_interval_seconds,
        deliveries,
        suppressions,
    } = snapshot;
    let mut html = match reminder_interval_seconds {
        Some(interval) => {
            format!("<p>SMTP email enabled.</p><p>Reminder interval: {interval} seconds.</p>")
        }
        None => "<p>SMTP email disabled.</p>".into(),
    };
    for suppression in suppressions {
        match suppression {
            NotificationSuppression::OutsideWindow {
                host_id,
                service_id,
                window_id,
            } => {
                let _ = write!(
                    html,
                    "<p>{} / {}: Outside notification window ({})</p>",
                    escape(&host_id),
                    escape(service_id.as_deref().unwrap_or("Host reachability")),
                    escape(&window_id),
                );
            }
            NotificationSuppression::HostDown { host_id } => {
                let _ = write!(
                    html,
                    "<p>{}: Held: host confirmed down. Service checks continue.</p>",
                    escape(&host_id),
                );
            }
        }
    }
    if deliveries.is_empty() {
        html.push_str("<p>No notification deliveries recorded.</p>");
        return html;
    }
    html.push_str("<h2>Recent notification deliveries</h2><div class=\"scroll\"><table><thead><tr><th>Host / Service</th><th>Notification</th><th>Recipient</th><th>State</th><th>Attempts</th><th>Last update</th><th>Next reminder (if eligible)</th><th>Diagnostic</th></tr></thead><tbody>");
    for record in deliveries {
        let notification = record.notification;
        let reminder_due =
            record
                .reminder_from
                .zip(reminder_interval_seconds)
                .map(|(from, interval)| {
                    (from + chrono::TimeDelta::seconds(i64::from(interval))).to_rfc3339()
                });
        let _ = write!(
            html,
            "<tr><td>{} / {}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&notification.host_id),
            escape(notification.target_label()),
            escape(&notification.label()),
            escape(&notification.recipient),
            escape(&record.state),
            notification.attempts,
            escape(&record.updated_at),
            escape(reminder_due.as_deref().unwrap_or("—")),
            escape(record.diagnostic.as_deref().unwrap_or("—")),
        );
    }
    html.push_str("</tbody></table></div>");
    html
}

fn load_error(error: anyhow::Error) -> WebError {
    eprintln!("load dashboard results: {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Cannot load monitoring results; consult the server log.",
    )
}

struct Snapshot<'a> {
    checks: &'a CheckSnapshot,
    // Live observation, not part of the durable snapshot.
    scheduler: SchedulerStatus,
    now: DateTime<Utc>,
}

impl<'a> Snapshot<'a> {
    fn new(dashboard: &Dashboard, checks: &'a CheckSnapshot) -> Self {
        Self {
            checks,
            scheduler: dashboard.scheduler.borrow().clone(),
            now: Utc::now(),
        }
    }

    fn host(&self, id: &str) -> Option<&Host> {
        self.checks.config.hosts.iter().find(|host| host.id == id)
    }

    fn freshness(&self, key: &Key, check: &Check) -> Freshness {
        schedule::freshness_from_timing(
            check,
            self.checks.config.retry_interval_seconds,
            self.checks.timings.get(key).copied(),
            self.scheduler.unchecked_since.get(key).copied(),
            self.now,
        )
    }

    fn overdue_count(&self) -> usize {
        let mut count = 0;
        for host in &self.checks.config.hosts {
            if let Some(probe) = &host.reachability {
                let freshness = self.freshness(&reachability_key(host), &probe.check);
                count += usize::from(freshness.is_overdue());
            }
            for service in &host.services {
                let key = service_key(host, service);
                count += usize::from(self.freshness(&key, &service.check).is_overdue());
            }
        }
        count
    }
}

// Keys match the scheduler's: host reachability uses an empty service ID.
fn reachability_key(host: &Host) -> Key {
    (host.id.clone(), String::new())
}

fn service_key(host: &Host, service: &Service) -> Key {
    (host.id.clone(), service.id.clone())
}

fn freshness_cell(freshness: Freshness) -> String {
    let time = |due: DateTime<Utc>| due.to_rfc3339_opts(SecondsFormat::Millis, true);
    match freshness {
        Freshness::NotYetChecked => "<td>Awaiting first check</td>".into(),
        Freshness::Current(due) => format!("<td>Due {}</td>", time(due)),
        Freshness::Overdue(Some(due)) => format!(
            "<td class=\"stale\">Overdue since {}; latest result is stale</td>",
            time(due)
        ),
        Freshness::Missing(due) => format!(
            "<td class=\"stale\">Overdue since {}; no result yet</td>",
            time(due)
        ),
        Freshness::Overdue(None) => {
            "<td class=\"stale\">Overdue; due time unknown, latest result is stale</td>".into()
        }
    }
}

/// Banners for conditions that stop monitoring from making progress.
fn alerts(snapshot: &Snapshot) -> String {
    let mut html = String::new();
    if let Some(fault) = &snapshot.scheduler.fault {
        let _ = write!(
            html,
            r#"<p class="alert">Scheduler fault since {}: {}. Monitoring is not recording progress.</p>"#,
            fault.since.to_rfc3339_opts(SecondsFormat::Millis, true),
            escape(&fault.message)
        );
    }
    match snapshot.overdue_count() {
        0 => {}
        1 => html.push_str(r#"<p class="alert">1 check is overdue.</p>"#),
        count => {
            let _ = write!(html, r#"<p class="alert">{count} checks are overdue.</p>"#);
        }
    }
    html
}

async fn load_checks(
    dashboard: &Dashboard,
    host_id: Option<String>,
) -> Result<CheckSnapshot, WebError> {
    dashboard
        .store
        .browse_checks(dashboard.config.clone(), host_id)
        .await
        .map_err(load_error)
}

fn overview(snapshot: &Snapshot, filters: &Filters) -> String {
    let table = checks_table(
        snapshot,
        |host| filters.group.is_empty() || host.group == filters.group,
        filters.problem,
    );
    let table = table.unwrap_or_else(|| {
        if filters.is_default() {
            r#"<p class="empty">No hosts are configured.</p>"#.into()
        } else {
            r#"<p class="empty">No checks match these filters. <a href="/">Show all checks</a></p>"#
                .into()
        }
    });
    alerts(snapshot) + &table
}

/// Rows for the selected hosts' checks, in group order, or `None` if no check
/// matches. Each row links to its host and check details.
fn checks_table(
    snapshot: &Snapshot,
    include: impl Fn(&Host) -> bool,
    problem: ProblemFilter,
) -> Option<String> {
    let CheckSnapshot {
        config,
        services,
        hosts,
        ..
    } = snapshot.checks;
    let mut rows = String::new();
    for group in &config.groups {
        for host in config
            .hosts
            .iter()
            .filter(|host| host.group == group.id && include(host))
        {
            if let Some(probe) = &host.reachability {
                let state = hosts.get(&host.id);
                if problem.matches(state.map(|state| state.problem)) {
                    row_start(
                        &mut rows,
                        group,
                        host,
                        "Host reachability",
                        &format!("Configured probe: {}", probe.probe.label()),
                        &reachability_url(host),
                    );
                    let episode = state.and_then(|state| state.episode.as_ref());
                    let state = state.map(|state| (&state.result, state.problem));
                    render_result(
                        &mut rows,
                        state,
                        episode,
                        snapshot.freshness(&reachability_key(host), &probe.check),
                        CheckDisplay::Reachability(
                            hosts.get(&host.id).map_or(probe.probe, |state| state.probe),
                        ),
                        config.confirmation_attempts,
                    );
                }
            }
            for service in &host.services {
                let key = service_key(host, service);
                let state = services.get(&key);
                if problem.matches(state.map(|state| state.problem)) {
                    row_start(
                        &mut rows,
                        group,
                        host,
                        &service.name,
                        &service.id,
                        &service_url(host, service),
                    );
                    let episode = state.and_then(|state| state.episode.as_ref());
                    let state = state.map(|state| (&state.result, state.problem));
                    render_result(
                        &mut rows,
                        state,
                        episode,
                        snapshot.freshness(&key, &service.check),
                        CheckDisplay::Service,
                        config.confirmation_attempts,
                    );
                }
            }
        }
    }
    (!rows.is_empty()).then(|| format!(
        "<div class=\"scroll\"><table><thead><tr><th>Group</th><th>Host</th><th>Check</th><th>Latest result</th><th>Freshness</th><th>Confirmed problem</th><th>Pending problem / severity change</th><th>Output</th><th>Started (UTC)</th><th>Finished (UTC)</th><th>Duration</th></tr></thead><tbody>{rows}</tbody></table></div>"
    ))
}

fn host_url(host: &Host) -> String {
    format!("/hosts/{}", encode(&host.id))
}

fn service_url(host: &Host, service: &Service) -> String {
    format!("{}/services/{}", host_url(host), encode(&service.id))
}

fn reachability_url(host: &Host) -> String {
    format!("{}/reachability", host_url(host))
}

fn group_url(group: &HostGroup) -> String {
    format!("/?group={}", encode(&group.id))
}

fn row_start(
    html: &mut String,
    group: &HostGroup,
    host: &Host,
    name: &str,
    subtitle: &str,
    check_url: &str,
) {
    let _ = write!(
        html,
        "<tr><td><a href=\"{}\">{}</a><br><small>{}</small></td><td><a href=\"{}\">{}</a><br><small>{}</small>{}</td><td><a href=\"{}\">{}</a><br><small>{}</small></td>",
        group_url(group),
        escape(&group.name),
        escape(&group.id),
        host_url(host),
        escape(&host.name),
        escape(&host.id),
        if host.reachability.is_none() {
            "<br><small>Reachability not configured</small>"
        } else {
            ""
        },
        check_url,
        escape(name),
        escape(subtitle)
    );
}

#[derive(Clone, Copy)]
enum CheckDisplay {
    Service,
    Reachability(Probe),
}

impl CheckDisplay {
    fn label(self, status: Status) -> &'static str {
        match self {
            Self::Service => status.label(),
            Self::Reachability(_) => Reachability::from_status(status).label(),
        }
    }
}

/// A stale result keeps its plugin status but loses its status colour, so an
/// old OK never looks like current successful monitoring.
fn latest_cell(result: &CheckResult, display: CheckDisplay, freshness: Freshness) -> String {
    let class = result.status.label();
    let label = display.label(result.status);
    let mut cell = if freshness.is_overdue() {
        format!("<td class=\"stale\">{label} (stale)")
    } else {
        format!("<td class=\"{class}\">{label}")
    };
    if let CheckDisplay::Reachability(probe) = display {
        let degraded = if result.status == Status::Warning {
            "degraded; "
        } else {
            ""
        };
        let _ = write!(
            cell,
            " ({degraded}{class})<br><small>Latest probe: {}</small>",
            probe.label()
        );
    }
    cell.push_str("</td>");
    cell
}

fn confirmed_cell(
    problem: ProblemState,
    display: CheckDisplay,
    episode: Option<&ProblemEpisode>,
) -> String {
    let acknowledgement = if episode.is_some_and(|episode| episode.acknowledged) {
        "<br><small>Acknowledged — reminders paused</small>"
    } else {
        ""
    };
    match problem.confirmed_severity {
        Some(severity) => format!(
            "<td class=\"{}\">Confirmed problem: {}{acknowledgement}</td>",
            severity.label(),
            display.label(severity)
        ),
        None => "<td>No confirmed problem</td>".into(),
    }
}

fn pending_cell(
    problem: ProblemState,
    display: CheckDisplay,
    confirmation_attempts: u32,
) -> String {
    match problem.pending_severity {
        Some(severity) => {
            let kind = if problem.confirmed_severity.is_some() {
                "Pending severity change"
            } else {
                "Pending problem"
            };
            format!(
                "<td class=\"{}\">{kind}: {} ({}/{confirmation_attempts})</td>",
                severity.label(),
                display.label(severity),
                problem.pending_attempts
            )
        }
        None => "<td>No pending problem</td>".into(),
    }
}

fn render_result(
    html: &mut String,
    state: Option<(&CheckResult, ProblemState)>,
    episode: Option<&ProblemEpisode>,
    freshness: Freshness,
    display: CheckDisplay,
    confirmation_attempts: u32,
) {
    if let Some((result, problem)) = state {
        html.push_str(&latest_cell(result, display, freshness));
        html.push_str(&freshness_cell(freshness));
        html.push_str(&confirmed_cell(problem, display, episode));
        html.push_str(&pending_cell(problem, display, confirmation_attempts));
        let _ = write!(
            html,
            "<td><pre>{}</pre></td><td>{}</td><td>{}</td><td>{} ms</td>",
            escape(&result.output),
            escape(&result.started_at),
            escape(&result.finished_at),
            result.duration_ms
        );
    } else {
        html.push_str("<td>Not yet checked</td>");
        html.push_str(&freshness_cell(freshness));
        html.push_str("<td colspan=\"6\">No result available</td>");
    }
    html.push_str("</tr>\n");
}

fn not_configured(what: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        layout(
            "Not configured — Ragios",
            &format!(
                r#"<nav><a href="/">Overview</a></nav><h1>Not configured</h1><p class="empty">The current configuration has no {what} with this ID. <a href="/">Return to the overview</a></p>"#
            ),
        ),
    )
        .into_response()
}

/// A detail page polls its own `/status` fragment, which carries everything
/// that can change between checks.
fn detail_page(title: &str, heading: &str, url: &str, fragment: &str) -> Response {
    layout(
        &format!("{title} — Ragios"),
        &format!(
            r#"{heading}<div id="detail" hx-get="{url}/status" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>
<noscript>Reload this page to refresh results.</noscript>"#
        ),
    )
    .into_response()
}

fn host_heading(config: &Config, host: &Host) -> String {
    let group = config.groups.iter().find(|group| group.id == host.group);
    let mut heading = format!(
        r#"<nav><a href="/">Overview</a></nav><h1>{}</h1><p>Host <code>{}</code>"#,
        escape(&host.name),
        escape(&host.id)
    );
    if let Some(group) = group {
        let _ = write!(
            heading,
            r#" · group <a href="{}">{}</a>"#,
            group_url(group),
            escape(&group.name)
        );
    }
    heading.push_str("</p>");
    heading
}

async fn host_page(
    State(dashboard): State<Dashboard>,
    Path(host_id): Path<String>,
) -> Result<Response, WebError> {
    let checks = load_checks(&dashboard, Some(host_id.clone())).await?;
    let snapshot = Snapshot::new(&dashboard, &checks);
    let Some(host) = snapshot.host(&host_id) else {
        return Ok(not_configured("host"));
    };
    let heading = host_heading(&snapshot.checks.config, host);
    Ok(detail_page(
        &host.name,
        &heading,
        &host_url(host),
        &host_checks(&snapshot, host),
    ))
}

async fn host_status(
    State(dashboard): State<Dashboard>,
    Path(host_id): Path<String>,
) -> Result<Response, WebError> {
    let checks = load_checks(&dashboard, Some(host_id.clone())).await?;
    let snapshot = Snapshot::new(&dashboard, &checks);
    Ok(match snapshot.host(&host_id) {
        Some(host) => Html(host_checks(&snapshot, host)).into_response(),
        None => not_configured("host"),
    })
}

fn host_checks(snapshot: &Snapshot, host: &Host) -> String {
    // Every resolved host has at least one service, so the table is never empty.
    let table = checks_table(
        snapshot,
        |candidate| candidate.id == host.id,
        ProblemFilter::All,
    )
    .unwrap_or_default();
    alerts(snapshot) + &table
}

/// A configured check on a host, identified for history lookup and display.
struct CheckDetail<'a> {
    host: &'a Host,
    name: &'a str,
    subtitle: String,
    url: String,
    display: CheckDisplay,
    state: Option<(&'a CheckResult, ProblemState)>,
    episode: Option<&'a ProblemEpisode>,
    freshness: Freshness,
    transitions: &'a [Transition],
}

impl<'a> CheckDetail<'a> {
    fn new(snapshot: &Snapshot, detail: &'a CheckDetailSnapshot) -> Self {
        let (host, definition) = detail.definition();
        let (name, subtitle, url, display, freshness) = match definition {
            CheckDefinition::Service(service) => (
                service.name.as_str(),
                service.id.clone(),
                service_url(host, service),
                CheckDisplay::Service,
                snapshot.freshness(&service_key(host, service), &service.check),
            ),
            CheckDefinition::Reachability(probe) => (
                "Host reachability",
                format!("Configured probe: {}", probe.probe.label()),
                reachability_url(host),
                CheckDisplay::Reachability(detail.observed_probe.unwrap_or(probe.probe)),
                snapshot.freshness(&reachability_key(host), &probe.check),
            ),
        };
        let saved = detail.state.as_ref();
        Self {
            host,
            name,
            subtitle,
            url,
            display,
            freshness,
            state: saved.map(|state| (&state.result, state.problem)),
            episode: saved.and_then(|state| state.episode.as_ref()),
            transitions: &detail.transitions,
        }
    }

    fn heading(&self) -> String {
        format!(
            r#"<nav><a href="/">Overview</a> › <a href="{}">{}</a></nav><h1>{}</h1><p><code>{}</code> on host <code>{}</code></p>"#,
            host_url(self.host),
            escape(&self.host.name),
            escape(self.name),
            escape(&self.subtitle),
            escape(&self.host.id)
        )
    }

    fn fragment(&self, snapshot: &Snapshot, csrf: &str) -> String {
        let transitions = self.transitions;
        let confirmation_attempts = snapshot.checks.config.confirmation_attempts;
        let mut html = alerts(snapshot);
        html.push_str("<h2>Current state</h2><table class=\"facts\"><tbody>");
        match self.state {
            Some((result, problem)) => {
                let _ = write!(
                    html,
                    "<tr><th>Latest result</th>{}</tr><tr><th>Freshness</th>{}</tr><tr><th>Confirmed problem</th>{}</tr><tr><th>Pending problem / severity change</th>{}</tr><tr><th>Output</th><td><pre>{}</pre></td></tr><tr><th>Started (UTC)</th><td>{}</td></tr><tr><th>Finished (UTC)</th><td>{}</td></tr><tr><th>Duration</th><td>{} ms</td></tr><tr><th>Result age</th><td>{}</td></tr>",
                    latest_cell(result, self.display, self.freshness),
                    freshness_cell(self.freshness),
                    confirmed_cell(problem, self.display, self.episode),
                    pending_cell(problem, self.display, confirmation_attempts),
                    escape(&result.output),
                    escape(&result.started_at),
                    escape(&result.finished_at),
                    result.duration_ms,
                    age(result, snapshot.now)
                );
            }
            None => {
                let _ = write!(
                    html,
                    "<tr><th>Latest result</th><td>Not yet checked</td></tr><tr><th>Freshness</th>{}</tr>",
                    freshness_cell(self.freshness)
                );
            }
        }
        html.push_str("</tbody></table>");
        if let Some(episode) = self.episode.filter(|episode| !episode.acknowledged) {
            let _ = write!(
                html,
                r#"<form method="post" action="{}/acknowledge"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="episode" value="{}"><button>Acknowledge problem</button></form>"#,
                self.url,
                escape(&episode.token),
            );
        }
        html.push_str("<h2>State changes</h2>");
        if transitions.is_empty() {
            let _ = write!(
                html,
                r#"<p class="empty">No state changes in the last {} days.</p>"#,
                TRANSITION_RETENTION.num_days()
            );
            return html;
        }
        html.push_str("<div class=\"scroll\"><table><thead><tr><th>Occurred (UTC)</th><th>Change</th><th>Result</th><th>Confirmed problem afterwards</th></tr></thead><tbody>");
        for transition in transitions {
            let result = transition.result_status;
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td class=\"{}\">{}</td>{}</tr>",
                escape(&transition.occurred_at),
                change_label(transition, self.display),
                result.label(),
                self.display.label(result),
                confirmed_cell(transition.state, self.display, None)
            );
        }
        html.push_str("</tbody></table></div>");
        html
    }
}

fn change_label(transition: &Transition, display: CheckDisplay) -> String {
    let state = transition.state;
    let label =
        |severity: Option<Status>| severity.map_or("none", |severity| display.label(severity));
    let pending = label(state.pending_severity);
    let confirmed = label(state.confirmed_severity);
    let attempt = state.pending_attempts;
    match transition.kind {
        TransitionKind::Pending => format!("Pending problem: {pending} (attempt {attempt})"),
        TransitionKind::Confirmed => format!("Problem confirmed: {confirmed}"),
        TransitionKind::SeverityChangePending => {
            format!("Pending severity change: {pending} (attempt {attempt})")
        }
        TransitionKind::SeverityChanged => format!("Confirmed severity changed: {confirmed}"),
        TransitionKind::SeverityChangeCancelled => {
            format!("Severity change cancelled; confirmed {confirmed} remains")
        }
        TransitionKind::Recovery => "Recovered".into(),
    }
}

fn age(result: &CheckResult, now: DateTime<Utc>) -> String {
    let Some(finished) = result.finished() else {
        return "unknown".into();
    };
    let seconds = now.signed_duration_since(finished).num_seconds().max(0);
    match seconds {
        0..60 => format!("{seconds} s ago"),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h {} min ago", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{} d {} h ago", seconds / 86400, seconds % 86400 / 3600),
    }
}

async fn check_detail(
    dashboard: &Dashboard,
    id: CheckId,
    fragment_only: bool,
) -> Result<Response, WebError> {
    let Some(facts) = dashboard
        .store
        .browse_check(dashboard.config.clone(), id)
        .await
        .map_err(load_error)?
    else {
        return Ok(not_configured("check"));
    };
    let snapshot = Snapshot::new(dashboard, facts.checks());
    let detail = CheckDetail::new(&snapshot, &facts);
    let fragment = detail.fragment(&snapshot, &dashboard.csrf);
    let mut response = if fragment_only {
        Html(fragment).into_response()
    } else {
        let control = check_now_form(&detail.url, &dashboard.csrf);
        detail_page(
            detail.name,
            &(detail.heading() + &control),
            &detail.url,
            &fragment,
        )
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn service_page(
    State(dashboard): State<Dashboard>,
    Path((host, service)): Path<(String, String)>,
) -> Result<Response, WebError> {
    check_detail(
        &dashboard,
        CheckId::Service {
            host_id: host,
            service_id: service,
        },
        false,
    )
    .await
}

async fn service_status(
    State(dashboard): State<Dashboard>,
    Path((host, service)): Path<(String, String)>,
) -> Result<Response, WebError> {
    check_detail(
        &dashboard,
        CheckId::Service {
            host_id: host,
            service_id: service,
        },
        true,
    )
    .await
}

async fn reachability_page(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckId::Reachability { host_id: host }, false).await
}

async fn reachability_status(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckId::Reachability { host_id: host }, true).await
}

fn check_now_form(url: &str, csrf: &str) -> String {
    format!(
        r##"<form method="post" action="{url}/check-now" hx-post="{url}/check-now" hx-target="#check-request-status" hx-disabled-elt="find button" hx-on::response-error="document.getElementById('check-request-status').textContent = 'Check request failed. Reload this page and try again.'"><input type="hidden" name="csrf" value="{csrf}"><button>Check now</button></form>
<div id="check-request-status" role="status"></div>"##
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckNowForm {
    csrf: String,
}

// The token is process-local. Also reject explicit foreign browser origins,
// even if they include a token. The host middleware has already validated the
// request authority against the HTTP listener; proxy headers are not trusted.
fn require_mutation(
    dashboard: &Dashboard,
    uri: &Uri,
    headers: &HeaderMap,
    csrf: &str,
) -> Result<(), WebError> {
    let authority = uri
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| headers.get(header::HOST)?.to_str().ok());
    let foreign_origin = headers.get_all(header::ORIGIN).iter().any(|origin| {
        !authority.is_some_and(|authority| {
            origin
                .to_str()
                .is_ok_and(|origin| origin.eq_ignore_ascii_case(&format!("http://{authority}")))
        })
    });
    if csrf != dashboard.csrf.as_ref()
        || foreign_origin
        || headers
            .get_all("sec-fetch-site")
            .iter()
            .any(|site| site != "same-origin")
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Invalid dashboard request; reload the dashboard.",
        ));
    }
    Ok(())
}

async fn check_now(
    dashboard: &Dashboard,
    key: Key,
    uri: &Uri,
    headers: &HeaderMap,
    form: CheckNowForm,
) -> Result<Response, WebError> {
    require_mutation(dashboard, uri, headers, &form.csrf)?;
    if uri.query().is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Check requests do not accept query parameters.",
        ));
    }
    let unavailable = (StatusCode::SERVICE_UNAVAILABLE, "Scheduler is unavailable.");
    let (reply, outcome) = oneshot::channel();
    let outcome = match dashboard.requests.try_send(CheckRequest { key, reply }) {
        Ok(()) => outcome.await.map_err(|_| unavailable)?,
        Err(mpsc::error::TrySendError::Full(_)) => CheckRequestOutcome::Busy,
        Err(mpsc::error::TrySendError::Closed(_)) => return Err(unavailable),
    };
    let message = match outcome {
        CheckRequestOutcome::Started => "Check started. Its result will appear after it finishes.",
        CheckRequestOutcome::AlreadyRunning => {
            "This check is already running. No duplicate was started."
        }
        CheckRequestOutcome::Busy => {
            "The scheduler is busy. No check was queued; try again shortly."
        }
        CheckRequestOutcome::NotConfigured => return Ok(not_configured("check")),
    };
    if headers
        .get("hx-request")
        .is_some_and(|value| value == "true")
    {
        return Ok(Html(format!("<p>{message}</p>")).into_response());
    }
    let detail_url = escape(
        uri.path()
            .strip_suffix("/check-now")
            .expect("check-now route"),
    );
    Ok(layout("Check request", &format!(r#"<h1>Check request</h1><p>{message}</p><p><a href="{detail_url}">View check and latest result</a></p>"#)).into_response())
}

async fn service_check_now(
    State(dashboard): State<Dashboard>,
    Path(key): Path<Key>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<CheckNowForm>,
) -> Result<Response, WebError> {
    check_now(&dashboard, key, &uri, &headers, form).await
}

async fn reachability_check_now(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<CheckNowForm>,
) -> Result<Response, WebError> {
    check_now(&dashboard, (host, String::new()), &uri, &headers, form).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgeForm {
    csrf: String,
    episode: String,
}

async fn acknowledge(
    dashboard: &Dashboard,
    path: CheckId,
    uri: &Uri,
    headers: &HeaderMap,
    form: AcknowledgeForm,
) -> Result<Response, WebError> {
    require_mutation(dashboard, uri, headers, &form.csrf)?;
    if uri.query().is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Acknowledgement requests do not accept query parameters.",
        ));
    }
    let (host, service) = match path {
        CheckId::Service {
            host_id,
            service_id,
        } => (host_id, Some(service_id)),
        CheckId::Reachability { host_id } => (host_id, None),
    };
    match dashboard
        .store
        .acknowledge_problem(dashboard.config.clone(), host, service, form.episode)
        .await
        .map_err(load_error)?
    {
        AcknowledgeOutcome::Acknowledged => {
            let detail_url = uri
                .path()
                .strip_suffix("/acknowledge")
                .expect("acknowledge route");
            Ok(Redirect::to(detail_url).into_response())
        }
        AcknowledgeOutcome::NotConfigured => Ok(not_configured("check")),
        AcknowledgeOutcome::NoMatchingProblem => Err((
            StatusCode::CONFLICT,
            "This confirmed problem is no longer current; reload the dashboard.",
        )),
    }
}

async fn service_acknowledge(
    State(dashboard): State<Dashboard>,
    Path((host, service)): Path<Key>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<AcknowledgeForm>,
) -> Result<Response, WebError> {
    acknowledge(
        &dashboard,
        CheckId::Service {
            host_id: host,
            service_id: service,
        },
        &uri,
        &headers,
        form,
    )
    .await
}

async fn reachability_acknowledge(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<AcknowledgeForm>,
) -> Result<Response, WebError> {
    acknowledge(
        &dashboard,
        CheckId::Reachability { host_id: host },
        &uri,
        &headers,
        form,
    )
    .await
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Percent-encode a path segment or query value; IDs are arbitrary text.
fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
